//! 拿到锁之后：启动表第 5 步起、运行、停机（T04 方案 2.4、2.8）。返回最后写下的 `Phase`，
//! 进程退出状态由调用方对它调 `process_status` 算。

#![cfg(unix)]

use std::ffi::OsString;
use std::future::Future;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use iwork_ipc::rendezvous::{ExeStamp, ExitCode, LockInfo, Phase, StopReason};
use iwork_ipc::server::{Publisher, Server, ServerConfig};
use iwork_ipc::{LocalEndpoint, LocalListener};
use iwork_ports::store::StoragePort;
use iwork_store::{DataDir, FsArtifactStore, StoreFailure};
use tokio::sync::{Notify, broadcast};
use tokio_util::sync::CancellationToken;

use crate::args::DaemonArgs;
use crate::assembly::{self, RuntimeSlot};
use crate::boot::{
    self, BootError, Identity, PAUSE_BEFORE_EXIT, Step, Store, StorePorts, VERSION, report,
    starting, status_of,
};
use crate::dev;
use crate::exit::{exited, process_status};
use crate::gate::{GatedStore, SealGate};
use crate::handler::dispatch::{CommandPorts, Dispatcher};
use crate::handler::lifecycle::autostart_handler;
use crate::handler::{DaemonHandler, Gate, Late};
use crate::host::TurnHost;
use crate::host::changes::{ReadBudget, VcsChanges};
use crate::host::ports::{HostEnv, Wired};
use crate::instance::{self, LockOutcome, StandbyAction};
use crate::logging::{self, AuditSignal, LogGuard};
use crate::publish::{LiveDeltas, PublishObserver, PublishState};
use crate::source::{SnapshotNotWired, StoreSource};
use crate::status::{StatusBoard, StatusIdentity};
use crate::stop::{self, StopHooks, StopStep, StopSwitch};
use crate::supervisor::{RestartPolicy, Supervisor, TaskSpec};
use crate::tasks::outbox::{self, OutboxRoute};
use crate::tasks::signals::{self, Signals};
use crate::tasks::{
    autostart_align, engine_sync, install_watch, maintenance, publish_check, sleep_watch,
    store_health,
};
use crate::time::{MonotonicIds, SystemClock, TokioTimer};
use crate::work::{LateWork, RunningWork};
use iwork_host::fs::OsPathResolver;

/// 第 2 步的结果。
pub struct Places {
    /// socket 目录（会合点）。
    pub socket_run: PathBuf,
    pub socket_path: PathBuf,
    pub data: DataDir,
}

fn locate() -> Result<Places, BootError> {
    let fail = |m: String| BootError::Endpoint(m);
    if dev::fail_at(Step::Endpoint.name()) {
        return Err(fail("开发注入：端点不可用".into()));
    }
    let endpoint = LocalEndpoint::for_current_user().map_err(|e| fail(e.to_string()))?;
    let socket_run = endpoint
        .run_dir()
        .ok_or_else(|| fail("端点没有 socket 目录".into()))?
        .to_path_buf();
    let socket_path = match endpoint.endpoint() {
        iwork_ipc::transport::Endpoint::Unix(p) => p.clone(),
        other => return Err(fail(format!("端点不是 Unix socket：{other:?}"))),
    };
    let data = DataDir::locate().map_err(|e| fail(e.to_string()))?;
    data.ensure_layout().map_err(|e| fail(e.to_string()))?;
    Ok(Places {
        socket_run,
        socket_path,
        data,
    })
}

pub fn run_identified(
    args: DaemonArgs,
    raw: Vec<OsString>,
    id: Identity,
) -> std::process::ExitCode {
    let managed = args.service;
    let places = match locate() {
        Ok(p) => p,
        Err(e) => {
            report(&e);
            return status_of(process_status(&e.exit_phase(), managed));
        }
    };
    // 第 3 步：锁
    let info = LockInfo {
        pid: std::process::id(),
        version: VERSION.into(),
        started_at_ms: iwork_ports::clock::Clock::now(&SystemClock::new()).0,
    };
    let lock = match instance::acquire(&places.data.run(), &places.socket_run, &info) {
        Ok(LockOutcome::Acquired(l)) => l,
        Ok(LockOutcome::HeldByOther(holder)) => {
            if managed {
                return standby(&places, &id.exe, &raw);
            }
            eprintln!(
                "iwork-daemon：已有后台服务在运行（pid {}），本实例让路",
                holder.map_or_else(|| "未知".into(), |h| h.pid.to_string())
            );
            return status_of(process_status(&exited(ExitCode::Stopped), managed));
        }
        Err(e) => {
            let e = BootError::Endpoint(format!("加锁失败：{e}"));
            report(&e);
            return status_of(process_status(&e.exit_phase(), managed));
        }
    };
    // 第 4 步：状态文件与日志。拿到锁才算状态文件的主人
    let board = StatusBoard::create(
        &places.socket_run,
        StatusIdentity {
            version: VERSION.into(),
            protocol_major: iwork_proto::PROTOCOL_MAJOR,
            exe_stamp: id.exe_stamp,
            log_dir: places.data.logs(),
        },
        starting(Step::StatusLog),
    );
    let log = logging::init(&places.data.logs());
    let exe = id.exe.clone();
    let phase = run_locked(args, raw.clone(), id, places, board.clone(), &log);
    tracing::info!(phase = ?phase, "后台服务退出");
    // 测试用：状态已写定、锁还在手里时停住，好让等锁的实例在「前任刚退出」的那一刻接手
    dev::pause_at(PAUSE_BEFORE_EXIT);
    drop(log);
    drop(lock);
    // 停机表第 8 步「升级」一列：原地换成磁盘上的程序（锁、socket 随 CLOEXEC 关掉），新程序从启动表第 1 步来
    if let Phase::Exited {
        reason: Some(StopReason::Upgrade),
        ..
    } = phase
    {
        dev::pause_at(boot::PAUSE_BEFORE_UPGRADE_EXEC);
        let err = exec_self(&exe, &raw);
        eprintln!("iwork-daemon：自行重启时 exec 失败：{err}");
    }
    status_of(process_status(&phase, managed))
}

/// 托管实例撞锁：阻塞等锁，前任没了照角色表决定接不接班（方案 2.3）。
fn standby(places: &Places, exe: &Path, raw: &[OsString]) -> std::process::ExitCode {
    eprintln!("iwork-daemon：已有后台服务在运行，托管实例等锁接班");
    let lock = match instance::wait_for_all(
        &places.data.run(),
        &places.socket_run,
        std::time::Duration::from_millis(200),
    ) {
        Ok(l) => l,
        Err(e) => {
            let e = BootError::Endpoint(format!("等锁失败：{e}"));
            report(&e);
            return status_of(process_status(&e.exit_phase(), true));
        }
    };
    let prev = iwork_ipc::rendezvous::read_status(&places.socket_run);
    let disk = ExeStamp::of(exe).ok();
    match instance::standby_action(prev.as_ref(), disk) {
        StandbyAction::StandDown => {
            eprintln!(
                "iwork-daemon：前任的停机原因不要求再起（{:?}），不接班",
                prev.map(|p| p.phase)
            );
            status_of(process_status(&exited(ExitCode::Stopped), true))
        }
        StandbyAction::TakeOver => {
            drop(lock);
            let err = exec_self(exe, raw);
            eprintln!("iwork-daemon：接班时 exec 失败：{err}");
            status_of(process_status(&exited(ExitCode::RestartRequested), true))
        }
    }
}

/// 原地换成磁盘上的程序（同样的参数）。成功不返回。
pub fn exec_self(exe: &Path, raw: &[OsString]) -> std::io::Error {
    use std::os::unix::process::CommandExt;
    std::process::Command::new(exe).args(raw).exec()
}

/// 第 5 步起的全部。`board` 已经写过 `Starting{status+log}`。
fn run_locked(
    args: DaemonArgs,
    _raw: Vec<OsString>,
    id: Identity,
    places: Places,
    board: StatusBoard,
    log: &LogGuard,
) -> Phase {
    let runtime = match tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .thread_name("iwork-daemon")
        .build()
    {
        Ok(r) => r,
        Err(e) => {
            return fail(
                &board,
                BootError::Fatal {
                    step: Step::StatusLog,
                    message: format!("起异步运行时失败：{e}"),
                },
            );
        }
    };
    let phase = runtime.block_on(boot_and_serve(
        args,
        id,
        places,
        board.clone(),
        log.audit.clone(),
    ));
    // 停机第 7 步的硬上限归停机流程；这里只给运行时里残留的任务一点时间
    runtime.shutdown_timeout(Duration::from_secs(1));
    phase
}

fn fail(board: &StatusBoard, e: BootError) -> Phase {
    tracing::error!(code = e.code().code(), error = ?e, "启动失败");
    eprintln!(
        "iwork-daemon：启动失败（逻辑退出码 {}）：{e:?}",
        e.code().code()
    );
    if let Some(p) = e.status() {
        board.set_phase(p);
    }
    e.exit_phase()
}

/// 进入某一步：状态文件写 `Starting{step}`；开发构建可在这里停住或注入失败。
fn enter(board: &StatusBoard, step: Step) -> bool {
    board.set_phase(starting(step));
    dev::pause_at(step.name());
    !dev::fail_at(step.name())
}

async fn boot_and_serve(
    args: DaemonArgs,
    id: Identity,
    places: Places,
    board: StatusBoard,
    audit: broadcast::Sender<AuditSignal>,
) -> Phase {
    let t0 = Instant::now();
    let clock = Arc::new(SystemClock::new());

    // 第 5 步：存储
    board.set_phase(starting(Step::Store));
    let store = match boot::open_store(&places.data, clock.clone()) {
        Ok(s) => s,
        Err(e) => return fail(&board, e),
    };

    // 第 5a、6、7、8 步
    let ids = Arc::new(MonotonicIds::new(SystemClock::new()));
    let artifacts = match FsArtifactStore::new(&store, ids.clone(), clock.clone()) {
        Ok(a) => a,
        Err(e) => {
            return fail(
                &board,
                BootError::Fatal {
                    step: Step::Artifacts,
                    message: e.to_string(),
                },
            );
        }
    };
    let ports = StorePorts {
        store: store.clone(),
        read: store.read_model(boot::read_defaults()),
        artifacts,
    };
    let interrupted = match boot::prepare(&ports, ids.as_ref(), |step| enter(&board, step)).await {
        Ok(i) => i,
        Err(e) => return fail(&board, e),
    };

    // 第 9 步起：回合宿主（`RunningWork`）在这一步构造。T04 交付时没有在跑的东西；开发构建可装假的
    #[cfg(feature = "dev")]
    if let Some(work) = crate::dev_parts::DevWork::from_env() {
        return serve_with(
            Arc::new(work),
            |_host| {},
            || {},
            args,
            id,
            places,
            board,
            store,
            artifacts_of(ports),
            audit,
            t0,
            interrupted.len(),
        )
        .await;
    }
    // 停机流程管的「在跑的东西」是回合宿主；它在第 9 步才组装好，先交一个空的，组装完填上
    let late_work = Arc::new(LateWork::default());
    let (fill, release) = (late_work.clone(), late_work.clone());
    serve_with(
        late_work,
        move |host| fill.fill(host),
        move || release.release(),
        args,
        id,
        places,
        board,
        store,
        artifacts_of(ports),
        audit,
        t0,
        interrupted.len(),
    )
    .await
}

fn artifacts_of(ports: StorePorts) -> boot::Artifacts {
    ports.artifacts
}

/// 观察者核对用的 head。
struct StoreHead(Arc<Store>);

impl publish_check::HeadSource for StoreHead {
    async fn head(&self) -> Result<u64, iwork_ports::store::StorageError> {
        self.0.head_seq().await.map(|s| s.0)
    }
}

/// 停机第 4–6 步要动的东西。
struct DaemonStop<'a> {
    supervisor: &'a mut Supervisor,
    serve_token: CancellationToken,
    store: Option<Arc<Store>>,
    /// 关存储之前放掉停机流程手里的回合宿主（它经带闸的存储握着存储）
    release_work: Option<Box<dyn FnOnce() + Send>>,
}

impl StopHooks for DaemonStop<'_> {
    async fn stop_tasks(&mut self) {
        self.supervisor
            .stop_all_except(TASK_SERVE, StopStep::StopTasks.limit())
            .await;
    }

    async fn stop_ipc(&mut self) {
        self.serve_token.cancel();
        if !self
            .supervisor
            .wait_for(TASK_SERVE, StopStep::StopIpc.limit())
            .await
        {
            tracing::warn!("ipc 到点没停完");
        }
    }

    async fn close_store(&mut self) -> Result<(), iwork_ports::store::StorageError> {
        if let Some(release) = self.release_work.take() {
            release();
        }
        let Some(store) = self.store.take() else {
            return Ok(());
        };
        match store.maintenance().await {
            Ok(()) | Err(iwork_ports::store::StorageError::Busy) => {}
            Err(e) => tracing::warn!(error = %e, "停机时 WAL 合回主库失败"),
        }
        // 第 4、5 步之后任务（连同登记表里的工厂）与 ipc 都放手了，这里应是唯一的持有者。
        // 不是就说明有持有者漏了：报错，存储改由最后一个持有者丢弃时关（不受这一步的上限管）
        let store =
            Arc::try_unwrap(store).map_err(|still| iwork_ports::store::StorageError::Io {
                message: format!(
                    "存储还有 {} 个别的持有者，关不了",
                    Arc::strong_count(&still) - 1
                ),
            })?;
        tokio::task::spawn_blocking(move || store.close())
            .await
            .unwrap_or_else(|e| {
                Err(iwork_ports::store::StorageError::Io {
                    message: format!("关存储的线程异常：{e}"),
                })
            })?;
        tracing::info!("存储已关闭");
        Ok(())
    }
}

const TASK_SERVE: &str = "ipc-serve";

/// 任务登记表里看着存储与发布的两行致命任务（方案 2.10）：存储失效、发布链路断了，任务就结束，
/// 监督器据此整体退出。真实装配与测试用的是同一个函数。
fn health_tasks(
    failed: impl Future<Output = StoreFailure> + Send + 'static,
    store_failed: Arc<AtomicBool>,
    audit: broadcast::Receiver<AuditSignal>,
    head: impl publish_check::HeadSource + 'static,
    watermark: impl Fn() -> u64 + Send + 'static,
    count_fault: impl Fn() + Send + 'static,
) -> [TaskSpec; 2] {
    [
        TaskSpec::fatal("store-health", store_health::run(failed, store_failed)),
        TaskSpec::fatal(
            "publish-check",
            publish_check::run(audit, head, watermark, count_fault, publish_check::SETTLE),
        ),
    ]
}

/// 运行：等停机请求，或监督器报出要整体退出的故障（后者按「故障」停机）。
async fn wait_stop_reason(supervisor: &mut Supervisor, stop: &StopSwitch) -> StopReason {
    tokio::select! {
        r = stop.wait() => r,
        fault = supervisor.watch() => {
            tracing::error!(task = fault.task(), ?fault, "整体退出");
            stop.request(StopReason::Fatal);
            StopReason::Fatal
        }
    }
}

type Autostart = iwork_host::autostart::Platform;

/// 本机的自启实现：注册项里写本进程的程序路径。发布构建用系统的 `launchctl` / `systemctl`；开发构建只用
/// `IWORK_DEV_AUTOSTART_TOOL` 给的替身，没给就什么都不做（组 3 审查 2/F-4：真进程测试、基准、演练脚本都是开发构建，
/// 只隔离了 `HOME`，没隔离 launchd，用系统命令会动到本机用户真实的后台）。
fn autostart_port(id: &Identity) -> Autostart {
    let (tool, assume_supported) = autostart_tool(dev_autostart_tool());
    iwork_host::autostart::for_current_user(iwork_host::autostart::AutostartConfig {
        program: id.exe.clone(),
        home: std::env::home_dir().unwrap_or_default(),
        tool,
        assume_supported,
    })
}

#[cfg(feature = "dev")]
fn dev_autostart_tool() -> Option<Option<std::ffi::OsString>> {
    Some(std::env::var_os("IWORK_DEV_AUTOSTART_TOOL"))
}

#[cfg(not(feature = "dev"))]
fn dev_autostart_tool() -> Option<Option<std::ffi::OsString>> {
    None
}

/// 用哪个命令：发布构建（`None`）用系统的；开发构建（`Some`）有替身用替身（同时不检查 systemd 用户实例），
/// 没有就禁用。
fn autostart_tool(dev: Option<Option<std::ffi::OsString>>) -> (iwork_host::autostart::Tool, bool) {
    use iwork_host::autostart::Tool;
    match dev {
        None => (Tool::System, false),
        Some(Some(path)) => (Tool::Substitute(PathBuf::from(path)), true),
        Some(None) => (Tool::Disabled, false),
    }
}

/// 启动表第 9a 步。
async fn unregister_if_disabled(port: &Autostart, store: &Arc<Store>) {
    use iwork_ports::autostart::{AutostartPort, AutostartState};
    use iwork_ports::read_model::ReadModelPort;
    let intent = match store.read_model(boot::read_defaults()).settings().await {
        Ok(s) => s.autostart,
        Err(e) => {
            tracing::warn!(error = %e, "读设置失败，跳过自启注销");
            return;
        }
    };
    if intent {
        return;
    }
    match port.state().await {
        Ok(AutostartState::Registered { .. } | AutostartState::Partial) => {
            tracing::info!("自启已关而注册项还在（全部或一半）：注销（服务管理器会结束本进程）");
            if let Err(e) = port.set(false).await {
                tracing::warn!(error = %e, "注销自启失败，继续启动");
            }
        }
        Ok(AutostartState::NotRegistered) => {}
        Err(e) => tracing::warn!(error = %e, "查自启注册状态失败，继续启动"),
    }
}

#[cfg(not(feature = "dev"))]
fn install_timing() -> install_watch::Timing {
    install_watch::Timing::REAL
}

/// 开发构建：`IWORK_DEV_INSTALL_FAST=<n>` 把安装检查的全部时长除以 n（真进程测试用；单测用暂停的时钟量真值）。
#[cfg(feature = "dev")]
fn install_timing() -> install_watch::Timing {
    match std::env::var("IWORK_DEV_INSTALL_FAST")
        .ok()
        .and_then(|v| v.parse().ok())
    {
        Some(div) => install_watch::Timing::scaled(div),
        None => install_watch::Timing::REAL,
    }
}

#[cfg(not(feature = "dev"))]
fn outbox_routes() -> Vec<OutboxRoute> {
    Vec::new()
}

#[cfg(feature = "dev")]
fn outbox_routes() -> Vec<OutboxRoute> {
    crate::dev_parts::outbox_routes()
}

#[cfg(not(feature = "dev"))]
fn dev_tasks(_stop: &StopSwitch, _store: &Arc<Store>) -> Vec<TaskSpec> {
    Vec::new()
}

/// 开发构建的触发器：停机（`IWORK_DEV_STOP_ON`）、让写线程失效（`IWORK_DEV_BREAK_WRITER_ON`）。
#[cfg(feature = "dev")]
fn dev_tasks(stop: &StopSwitch, store: &Arc<Store>) -> Vec<TaskSpec> {
    let mut v = Vec::new();
    if let Some((file, reason)) = crate::dev_parts::stop_trigger() {
        let stop = stop.clone();
        v.push(TaskSpec::restartable("dev-stop", move || {
            crate::dev_parts::stop_on(file.clone(), reason, stop.clone())
        }));
    }
    if let Some(file) = crate::dev_parts::break_writer_trigger() {
        let store = store.clone();
        v.push(TaskSpec::restartable("dev-break-writer", move || {
            crate::dev_parts::break_writer_on(file.clone(), store.clone())
        }));
    }
    v
}

/// 提交观察者。开发构建设了 `IWORK_DEV_BREAK_WRITER_ON` 时外面包一层：见到触发器提交的那批就在写线程上 panic。
#[cfg(not(feature = "dev"))]
fn observer(o: PublishObserver<Publisher>) -> Box<dyn iwork_store::CommitObserver> {
    Box::new(o)
}

#[cfg(feature = "dev")]
fn observer(o: PublishObserver<Publisher>) -> Box<dyn iwork_store::CommitObserver> {
    crate::dev_parts::wrap_observer(o)
}

#[allow(clippy::too_many_arguments)]
async fn serve_with<W: RunningWork>(
    work: Arc<W>,
    bind_host: impl FnOnce(TurnHost<HostP>),
    release_host: impl FnOnce() + Send + 'static,
    args: DaemonArgs,
    id: Identity,
    places: Places,
    board: StatusBoard,
    store: Arc<Store>,
    artifacts: boot::Artifacts,
    audit: broadcast::Sender<AuditSignal>,
    t0: Instant,
    interrupted: usize,
) -> Phase {
    // 第 9 步：组装 ipc、装提交观察者、各参与方恢复
    if !enter(&board, Step::Server) {
        return fail(&board, boot::injected(Step::Server));
    }
    let autostart = Arc::new(autostart_port(&id));
    // E4：从这里起交出去的存储都是带封口闸的这一个（受理函数、各任务、outbox 投递）。裸的只留给组装处自己
    // （装观察者、读 head、维护、关存储）与启动时的恢复（上面的第 8 步）
    let gated = Arc::new(GatedStore::new(
        store.clone(),
        Arc::new(SealGate::default()),
    ));
    let gate = Arc::new(Gate::default());
    let stop_switch = StopSwitch::new();
    // 受理要用引擎与回合宿主，它们要用 ipc 的发布方：先交一个空的内层，组装完、bind 之前填上
    let late = Arc::new(std::sync::OnceLock::new());
    let mut cfg = ServerConfig::new(VERSION);
    cfg.trust = args.trust;
    let (server, publisher) = Server::new(
        cfg,
        id.own.clone(),
        DaemonHandler::new(
            gate.clone(),
            Late(late.clone()),
            work.clone(),
            stop_switch.clone(),
        )
        .with_autostart(autostart_handler(autostart.clone(), gated.clone())),
        StoreSource::new(store.clone(), SnapshotNotWired),
    );
    let publishing = PublishState::new();
    let publisher = Arc::new(publisher);
    let observer = PublishObserver::new(publisher.clone(), publishing.clone());
    if let Err(e) = store.set_commit_observer(self::observer(observer)).await {
        return fail(
            &board,
            BootError::Fatal {
                step: Step::Server,
                message: format!("装提交观察者失败：{e}"),
            },
        );
    }
    // 观察者装上之后、bind 之前：各参与方做自己的恢复（T10 接回合宿主与审批引擎）。这里提交的事件
    // 经观察者发布；那时还没有连接，客户端连上后靠 serve 的基线与补发拿到它们
    for p in boot::startup_participants() {
        if let Err(e) = p.start(&store).await {
            return fail(&board, boot::read_error(Step::Server, e));
        }
    }
    // 第 9 步的回合与审批参与方（T10a 方案 2.8 第 1–5 步）
    let turns = match assemble_turns(
        &places,
        &id,
        &store,
        gated.as_ref(),
        LiveDeltas::new(publisher.clone(), publishing.routes.clone()),
        publishing.host.clone(),
    )
    .await
    {
        Ok(t) => t,
        Err(e) => return fail(&board, e),
    };
    if late.set(turns.dispatcher).is_err() {
        unreachable!("内层只在这里填一次");
    }
    bind_host(turns.host.clone());
    // 请求入口那一份随 ipc 停下（停机第 5 步）放手；这里的不留，否则关存储时还握着它
    drop(late);

    // 第 9a 步：托管实例 + 已提交的设置是关 → 查到自己还注册着就注销。注销会结束本进程——这正是要的：
    // 这时还没 bind、没有客户端、没有回合在跑（方案 2.12）。失败只记 warn，继续启动
    if args.service {
        board.set_phase(starting(Step::AutostartOff));
        unregister_if_disabled(autostart.as_ref(), &store).await;
    }

    // 第 10 步：绑定
    board.set_phase(starting(Step::Bind));
    let listener = match bind(&places) {
        Ok(l) => l,
        Err(e) => return fail(&board, e),
    };

    // 第 11 步：按任务登记表起全部任务
    if !enter(&board, Step::Tasks) {
        return fail(&board, boot::injected(Step::Tasks));
    }
    let tasks_failed = |message: String| BootError::Fatal {
        step: Step::Tasks,
        message,
    };
    // T04 交付时登记表是空的（T10 登记真实去向）；开发构建可加假去向
    let routes = outbox_routes();
    if let Err(e) = outbox::validate(&routes) {
        return fail(&board, tasks_failed(e));
    }
    let routes = Arc::new(routes);
    // 信号处理登记在写 `Ready` 之前：之后到的 SIGTERM 一定走停机流程
    let signals = match Signals::register() {
        Ok(s) => s,
        Err(e) => return fail(&board, tasks_failed(format!("登记信号处理失败：{e}"))),
    };
    match store.head_seq().await {
        Ok(h) => publishing.set_baseline(h.0),
        Err(e) => return fail(&board, tasks_failed(format!("读 head 失败：{e}"))),
    }
    let serve_token = CancellationToken::new();
    let store_failed = Arc::new(AtomicBool::new(false));
    let woke = Arc::new(Notify::new());
    let clock = Arc::new(SystemClock::new());
    let mut specs = dev_tasks(&stop_switch, &store);
    specs.extend(health_tasks(
        store.failed(),
        store_failed.clone(),
        audit.subscribe(),
        StoreHead(store.clone()),
        {
            let p = publishing.clone();
            move || p.watermark()
        },
        {
            let b = board.clone();
            move || b.add_ipc_fault()
        },
    ));
    specs.extend([
        TaskSpec::fatal(TASK_SERVE, server.serve(listener, serve_token.clone())),
        TaskSpec::fatal("turn-host", {
            let host = turns.host.clone();
            async move {
                let e = host.run().await;
                tracing::error!(error = %e, "回合宿主致命错误，整体退出");
            }
        }),
        TaskSpec::restartable("engine-sync", {
            let (engine, wake, host) = (
                turns.engine.clone(),
                publishing.engine.clone(),
                turns.host.clone(),
            );
            move || {
                let host = host.clone();
                engine_sync::run(engine.clone(), wake.clone(), move || host.wake())
            }
        }),
        TaskSpec::restartable("signals", {
            let stop = stop_switch.clone();
            let mut pre = Some(signals);
            move || signals::run(pre.take(), stop.clone())
        }),
        TaskSpec::restartable("outbox", {
            let (store, routes, wake) = (gated.clone(), routes.clone(), publishing.outbox.clone());
            move || {
                outbox::run(
                    store.clone(),
                    routes.clone(),
                    wake.clone(),
                    outbox::log_unclaimed(),
                )
            }
        }),
        TaskSpec::restartable("maintenance", {
            let (store, artifacts) = (store.clone(), artifacts.clone());
            move || maintenance::run(store.clone(), artifacts.clone())
        }),
        TaskSpec::restartable("sleep-watch", {
            let (clock, woke) = (clock.clone(), woke.clone());
            move || sleep_watch::run(clock.clone(), woke.clone(), sleep_watch::INTERVAL)
        }),
        TaskSpec::restartable("autostart-align", {
            let read = Arc::new(store.read_model(boot::read_defaults()));
            let (port, wake) = (autostart.clone(), publishing.autostart.clone());
            let (managed, program) = (args.service, id.exe.clone());
            move || {
                autostart_align::run(
                    port.clone(),
                    read.clone(),
                    managed,
                    program.clone(),
                    wake.clone(),
                )
            }
        }),
        TaskSpec::restartable("install-watch", {
            let disk = Arc::new(install_watch::RealDisk {
                daemon_path: id.exe.clone(),
                running: id.running_image.clone(),
                own: id.own.clone(),
                forms: crate::install::forms(),
            });
            let started = tokio::time::Instant::from_std(id.started);
            let (stop, woke, audit) = (stop_switch.clone(), woke.clone(), audit.clone());
            move || {
                install_watch::run(
                    disk.clone(),
                    install_timing(),
                    started,
                    stop.clone(),
                    audit.subscribe(),
                    woke.clone(),
                )
            }
        }),
    ]);
    let mut supervisor = Supervisor::start(specs, RestartPolicy::default());
    // 组装用的这几份交给任务与请求入口之后就不要了：关存储时这里不能还握着存储（1/F-2）
    drop(turns.host);
    drop(turns.engine);
    drop(gated);

    // 第 12 步：就绪
    board.set_phase(Phase::Ready);
    let head = store.head_seq().await.map(|s| s.0).unwrap_or_default();
    tracing::info!(
        version = VERSION,
        protocol_major = iwork_proto::PROTOCOL_MAJOR,
        head,
        interrupted,
        boot_ms = t0.elapsed().as_millis() as u64,
        since_start_ms = id.started.elapsed().as_millis() as u64,
        image = %id.running_image,
        "后台服务就绪"
    );

    // 运行：等停机请求，或监督器报出要整体退出的故障
    let reason = wait_stop_reason(&mut supervisor, &stop_switch).await;
    stop::arm_hard_deadline(reason, args.service, stop::HARD_LIMIT);
    let mut hooks = DaemonStop {
        supervisor: &mut supervisor,
        serve_token,
        store: Some(store),
        release_work: Some(Box::new(release_host)),
    };
    let phase = stop::stop(
        reason,
        store_failed.load(Ordering::SeqCst),
        &board,
        &gate,
        work.as_ref(),
        &mut hooks,
    )
    .await;
    supervisor.abort_all();
    phase
}

/// 回合与审批这一侧装配好的东西。
struct TurnParts {
    dispatcher: Disp,
    host: TurnHost<HostP>,
    engine: Arc<Engine>,
}

type GStore = GatedStore<Store>;
type Read = iwork_store::SqliteReadModel<iwork_app::codec::ProtoEventCodec>;
type Engine = iwork_core::approval::engine::ApprovalEngine<
    SystemClock,
    MonotonicIds<SystemClock>,
    GStore,
    RuntimeSlot,
    TokioTimer,
    OsPathResolver,
>;
type HostP = Wired<
    GStore,
    Read,
    RuntimeSlot,
    Arc<Engine>,
    LiveDeltas<Publisher>,
    SystemClock,
    MonotonicIds<SystemClock>,
    TokioTimer,
    OsPathResolver,
    VcsChanges<iwork_vcs_git::GitVcs>,
>;
type Disp = Dispatcher<GStore, Read, Engine>;

/// 改动探针开不开（D10）：7/F-2（T08 子模块 `.git` 链接）修好之前关着，关着时一次版本控制调用都不发。
const CHANGE_PROBE_ENABLED: bool = false;

fn startup_failed(message: String) -> BootError {
    BootError::Fatal {
        step: Step::Server,
        message,
    }
}

/// 启动表第 9 步的回合与审批参与方（T10a 方案 2.8 第 1–5 步）。读到不认识的事件种类原样走 `read_error`（归 12）。
async fn assemble_turns(
    places: &Places,
    id: &Identity,
    raw: &Arc<Store>,
    gated: &GStore,
    live: LiveDeltas<Publisher>,
    host_wake: Arc<Notify>,
) -> Result<TurnParts, BootError> {
    use iwork_ports::discovery::RuntimeDiscovery;
    let read = raw.read_model(boot::read_defaults());
    let ids = MonotonicIds::new(SystemClock::new());
    let storage = |e: iwork_ports::store::StorageError| boot::read_error(Step::Server, e);
    // 1. 探测运行时，提交运行时那一行（S8 合入之前探测是「什么都找不到」的占位）
    let probe = assembly::NothingFound
        .discover(&iwork_domain::runtime::RuntimeKind(
            assembly::RUNTIME_KIND.into(),
        ))
        .await
        .map_err(|e| startup_failed(format!("探测运行时失败：{e}")))?;
    assembly::commit_runtime_row(gated, &read, &ids, assembly::runtime_row(&probe, VERSION))
        .await
        .map_err(storage)?;
    // 1a. 登记内置助手 iWork
    assembly::register_builtin_assistant(gated, &read, &SystemClock::new())
        .await
        .map_err(storage)?;
    // 2. 运行时
    let data_dir = places.data.root().to_path_buf();
    let agent = RuntimeSlot(
        assembly::runtime_launch(assembly::bridge_program(&id.exe), &data_dir, &probe)
            .map(iwork_agent_acp::AcpRuntime::new),
    );
    let home = std::env::home_dir()
        .and_then(|h| h.canonicalize().ok())
        .ok_or_else(|| startup_failed("取不到家目录".into()))?;
    let case_insensitive = OsPathResolver::detect_case_insensitive(&data_dir).unwrap_or(false);
    let fs = OsPathResolver::new(home.clone(), case_insensitive);
    let env = HostEnv {
        platform: HostEnv::platform(),
        home,
        data_dir,
        tmp_dirs: vec![std::env::temp_dir()],
    };
    // 3、4. 引擎折叠到位，恢复计划落盘
    let keys = assembly::keys_for_pending(&read, &env, &fs)
        .await
        .map_err(storage)?;
    let engine = assembly::start_engine(
        iwork_core::approval::engine::EnginePorts {
            clock: SystemClock::new(),
            ids: MonotonicIds::new(SystemClock::new()),
            store: gated.clone(),
            reviewer: agent.clone(),
            timer: TokioTimer,
            fs: fs.clone(),
        },
        keys,
        gated,
        &ids,
    )
    .await
    .map_err(|e| match e {
        iwork_core::approval::engine::EngineError::Storage(s) => storage(s),
        other => startup_failed(format!("审批引擎起不来：{other}")),
    })?;
    // 5. 回合宿主
    let host = TurnHost::start(
        Arc::new(Wired {
            store: gated.clone(),
            read: raw.read_model(boot::read_defaults()),
            agent,
            engine: engine.clone(),
            live,
            clock: SystemClock::new(),
            ids: MonotonicIds::new(SystemClock::new()),
            timer: TokioTimer,
            fs,
            env,
            changes: VcsChanges::new(
                Arc::new(iwork_vcs_git::GitVcs::default()),
                CHANGE_PROBE_ENABLED,
                Arc::new(ReadBudget::default()),
            ),
            gate: gated.gate().clone(),
        }),
        host_wake,
    )
    .await
    .map_err(|e| match e {
        crate::host::HostStartError::Storage(s) => storage(s),
        other => startup_failed(other.to_string()),
    })?;
    let dispatcher = Dispatcher::new(CommandPorts {
        store: Arc::new(gated.clone()),
        read: Arc::new(read),
        engine: engine.clone(),
        ids: Arc::new(MonotonicIds::new(SystemClock::new())),
        clock: Arc::new(SystemClock::new()),
        host: Arc::new(host.clone()),
    });
    Ok(TurnParts {
        dispatcher,
        host,
        engine,
    })
}

fn bind(places: &Places) -> Result<LocalListener, BootError> {
    if dev::fail_at(Step::Bind.name()) {
        return Err(BootError::Bind("开发注入：绑定失败".into()));
    }
    LocalListener::bind(&places.socket_path)
        .map_err(|e| BootError::Bind(format!("绑定 {} 失败：{e}", places.socket_path.display())))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 2/F-4：开发构建没给替身 → 禁用（不跑任何命令）；给了 → 用替身；发布构建 → 系统命令。
    #[test]
    fn dev_build_never_uses_system_autostart_tool() {
        use iwork_host::autostart::Tool;
        assert_eq!(autostart_tool(Some(None)), (Tool::Disabled, false));
        assert_eq!(
            autostart_tool(Some(Some("/t/tool".into()))),
            (Tool::Substitute(PathBuf::from("/t/tool")), true)
        );
        assert_eq!(autostart_tool(None), (Tool::System, false));
        #[cfg(feature = "dev")]
        assert_ne!(
            dev_autostart_tool(),
            None,
            "开发构建走开发的那一列，不会落到系统命令"
        );
    }
    use crate::status::StatusIdentity;
    use iwork_ipc::rendezvous::ExeStamp;
    use iwork_ports::store::StorageError;
    use std::sync::Mutex;

    /// 记录第 4–6 步调用的假实现。
    #[derive(Default)]
    struct Recorded(Arc<Mutex<Vec<&'static str>>>);

    impl StopHooks for Recorded {
        async fn stop_tasks(&mut self) {
            self.0.lock().unwrap().push("stop_tasks");
        }
        async fn stop_ipc(&mut self) {
            self.0.lock().unwrap().push("stop_ipc");
        }
        async fn close_store(&mut self) -> Result<(), StorageError> {
            self.0.lock().unwrap().push("close_store");
            Ok(())
        }
    }

    struct FixedHead(u64);
    impl publish_check::HeadSource for FixedHead {
        async fn head(&self) -> Result<u64, StorageError> {
            Ok(self.0)
        }
    }

    struct Harness {
        _dir: tempfile::TempDir,
        board: StatusBoard,
        store_failed: Arc<AtomicBool>,
        supervisor: Supervisor,
        stop: StopSwitch,
    }

    /// 和 `serve_with` 一样的组装：两行健康任务 + 一个普通的可重启任务，交给监督器。
    fn harness(
        failed: tokio::sync::oneshot::Receiver<StoreFailure>,
        audit: &broadcast::Sender<AuditSignal>,
        head: u64,
        watermark: u64,
    ) -> Harness {
        let dir = tempfile::tempdir().unwrap();
        let board = StatusBoard::create(
            dir.path(),
            StatusIdentity {
                version: "t".into(),
                protocol_major: 1,
                exe_stamp: ExeStamp {
                    len: 0,
                    modified_unix_ns: 0,
                },
                log_dir: dir.path().into(),
            },
            Phase::Ready,
        );
        let store_failed = Arc::new(AtomicBool::new(false));
        let failed = async move {
            match failed.await {
                Ok(f) => f,
                Err(_) => std::future::pending().await,
            }
        };
        let b = board.clone();
        let mut specs = vec![TaskSpec::restartable("other", std::future::pending::<()>)];
        specs.extend(health_tasks(
            failed,
            store_failed.clone(),
            audit.subscribe(),
            FixedHead(head),
            move || watermark,
            move || b.add_ipc_fault(),
        ));
        Harness {
            _dir: dir,
            board,
            store_failed,
            supervisor: Supervisor::start(specs, RestartPolicy::default()),
            stop: StopSwitch::new(),
        }
    }

    /// 一路走到底：等停机原因 → 停机表 → 退出状态。返回（原因、最后的 Phase、第 4–6 步记录）。
    async fn run_to_exit(mut h: Harness) -> (StopReason, Phase, Vec<&'static str>, Harness) {
        let reason = tokio::time::timeout(
            Duration::from_secs(60),
            wait_stop_reason(&mut h.supervisor, &h.stop),
        )
        .await
        .expect("应整体退出");
        let rec = Recorded::default();
        let log = rec.0.clone();
        let mut hooks = rec;
        let phase = stop::stop(
            reason,
            h.store_failed.load(Ordering::SeqCst),
            &h.board,
            &Gate::default(),
            &crate::work::NoWork,
            &mut hooks,
        )
        .await;
        h.supervisor.abort_all();
        let l = log.lock().unwrap().clone();
        (reason, phase, l, h)
    }

    fn assert_fatal_exit(phase: &Phase, board: &StatusBoard) {
        assert_eq!(
            *phase,
            Phase::Exited {
                code: ExitCode::Fatal,
                reason: Some(StopReason::Fatal),
                message: None
            }
        );
        assert_eq!(board.snapshot().phase, *phase, "状态文件写的就是它");
        assert_eq!(process_status(phase, false), 20);
        assert_eq!(
            process_status(phase, true),
            20,
            "托管实例也报非零，服务管理器会再拉"
        );
    }

    /// 方案 S5：存储失效信号 → store-health 结束 → 监督器整体退出 → 停机「故障」列、跳过关存储 → 20。
    #[tokio::test(start_paused = true)]
    async fn store_failure_exits_fatal() {
        let (tx, rx) = tokio::sync::oneshot::channel();
        let (audit, _) = broadcast::channel(8);
        let h = harness(rx, &audit, 0, 0);
        tx.send(StoreFailure::WriterBroken).unwrap();
        let (reason, phase, steps, h) = run_to_exit(h).await;
        assert_eq!(reason, StopReason::Fatal);
        assert!(h.store_failed.load(Ordering::SeqCst));
        assert_eq!(steps, ["stop_tasks", "stop_ipc"], "跳过关存储");
        assert_fatal_exit(&phase, &h.board);
    }

    /// 方案 S6：ipc 报 PublishStalled、核对时库里的 head 仍超过发布水位 → publish-check 结束 → 整体退出 20。
    /// 存储本身没坏，第 6 步照常关存储。
    #[tokio::test(start_paused = true)]
    async fn publish_stall_confirmed_exits() {
        let (_tx, rx) = tokio::sync::oneshot::channel();
        let (audit, _) = broadcast::channel(8);
        let h = harness(rx, &audit, 10, 7);
        tokio::task::yield_now().await;
        audit
            .send(AuditSignal::ServerFault {
                cause: "PublishStalled".into(),
            })
            .unwrap();
        let (reason, phase, steps, h) = run_to_exit(h).await;
        assert_eq!(reason, StopReason::Fatal);
        assert!(!h.store_failed.load(Ordering::SeqCst));
        assert_eq!(steps, ["stop_tasks", "stop_ipc", "close_store"]);
        assert_fatal_exit(&phase, &h.board);
    }

    /// 对照：发布只是一时没追上（head 等于水位）→ 只记数，不退出。
    #[tokio::test(start_paused = true)]
    async fn transient_publish_stall_keeps_running() {
        let (_tx, rx) = tokio::sync::oneshot::channel();
        let (audit, _) = broadcast::channel(8);
        let mut h = harness(rx, &audit, 10, 10);
        tokio::task::yield_now().await;
        audit
            .send(AuditSignal::ServerFault {
                cause: "PublishStalled".into(),
            })
            .unwrap();
        let r = tokio::time::timeout(
            Duration::from_secs(30),
            wait_stop_reason(&mut h.supervisor, &h.stop),
        )
        .await;
        assert!(r.is_err(), "不该整体退出：{r:?}");
        assert_eq!(h.board.snapshot().ipc_faults, 1);
        h.supervisor.abort_all();
    }
}
