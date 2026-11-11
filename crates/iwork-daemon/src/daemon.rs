//! 拿到锁之后：启动表第 5 步起、运行、停机（T04 方案 2.4、2.8）。返回最后写下的 `Phase`，
//! 进程退出状态由调用方对它调 `process_status` 算。

#![cfg(unix)]

use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use iwork_ipc::rendezvous::{ExeStamp, ExitCode, LockInfo, Phase, StopReason};
use iwork_ipc::server::{Server, ServerConfig};
use iwork_ipc::{LocalEndpoint, LocalListener};
use iwork_ports::store::StoragePort;
use iwork_store::DataDir;
use tokio_util::sync::CancellationToken;

use crate::args::DaemonArgs;
use crate::boot::{
    self, BootError, Identity, PAUSE_BEFORE_EXIT, Step, VERSION, report, starting, status_of,
};
use crate::dev;
use crate::exit::{exited, process_status, stop_code};
use crate::handler::{DaemonHandler, Gate, NotWired};
use crate::instance::{self, LockOutcome, StandbyAction};
use crate::logging::{self, LogGuard};
use crate::source::{SnapshotNotWired, StoreSource};
use crate::status::{StatusBoard, StatusIdentity};
use crate::time::SystemClock;

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
    let phase = run_locked(args, raw, id, places, board.clone(), &log);
    tracing::info!(phase = ?phase, "后台服务退出");
    // 测试用：状态已写定、锁还在手里时停住，好让等锁的实例在「前任刚退出」的那一刻接手
    dev::pause_at(PAUSE_BEFORE_EXIT);
    drop(log);
    drop(lock);
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
    _log: &LogGuard,
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
    let phase = runtime.block_on(boot_and_serve(args, id, places, board.clone()));
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

/// 进入某一步：状态文件写 `Starting{step}`；开发构建可在这里注入失败。
fn enter(board: &StatusBoard, step: Step) -> bool {
    board.set_phase(starting(step));
    !dev::fail_at(step.name())
}

async fn boot_and_serve(
    args: DaemonArgs,
    id: Identity,
    places: Places,
    board: StatusBoard,
) -> Phase {
    let t0 = Instant::now();
    let clock = Arc::new(SystemClock::new());

    // 第 5 步：存储
    board.set_phase(starting(Step::Store));
    let store = match boot::open_store(&places.data, clock.clone()) {
        Ok(s) => s,
        Err(e) => return fail(&board, e),
    };

    // 第 7 步：投递闸
    board.set_phase(starting(Step::DeliveryGate));
    store.mark_delivery_started();

    // 第 9 步：组装 ipc
    if !enter(&board, Step::Server) {
        return fail(&board, injected(Step::Server));
    }
    let gate = Arc::new(Gate::default());
    let mut cfg = ServerConfig::new(VERSION);
    cfg.trust = args.trust;
    let (server, _publisher) = Server::new(
        cfg,
        id.own.clone(),
        DaemonHandler::new(gate.clone(), NotWired),
        StoreSource::new(store.clone(), SnapshotNotWired),
    );

    // 第 10 步：绑定
    board.set_phase(starting(Step::Bind));
    let listener = match bind(&places) {
        Ok(l) => l,
        Err(e) => return fail(&board, e),
    };

    // 第 12 步：就绪
    let shutdown = CancellationToken::new();
    let serve = tokio::spawn(server.serve(listener, shutdown.clone()));
    board.set_phase(Phase::Ready);
    let head = store.head_seq().await.map(|s| s.0).unwrap_or_default();
    tracing::info!(
        version = VERSION,
        protocol_major = iwork_proto::PROTOCOL_MAJOR,
        head,
        boot_ms = t0.elapsed().as_millis() as u64,
        since_start_ms = id.started.elapsed().as_millis() as u64,
        "后台服务就绪"
    );

    let reason = wait_for_signal().await;
    board.set_phase(Phase::Stopping { reason });
    gate.close_and_drain(Duration::from_secs(1)).await;
    shutdown.cancel();
    let _ = tokio::time::timeout(Duration::from_secs(2), serve).await;
    drop(store);
    let phase = Phase::Exited {
        code: stop_code(reason),
        reason: Some(reason),
        message: None,
    };
    board.set_phase(phase.clone());
    phase
}

fn injected(step: Step) -> BootError {
    BootError::Fatal {
        step,
        message: "开发注入的失败".into(),
    }
}

fn bind(places: &Places) -> Result<LocalListener, BootError> {
    if dev::fail_at(Step::Bind.name()) {
        return Err(BootError::Bind("开发注入：绑定失败".into()));
    }
    LocalListener::bind(&places.socket_path)
        .map_err(|e| BootError::Bind(format!("绑定 {} 失败：{e}", places.socket_path.display())))
}

async fn wait_for_signal() -> StopReason {
    use tokio::signal::unix::{SignalKind, signal};
    let mut term = signal(SignalKind::terminate()).expect("登记 SIGTERM");
    let mut int = signal(SignalKind::interrupt()).expect("登记 SIGINT");
    let mut hup = signal(SignalKind::hangup()).expect("登记 SIGHUP");
    tokio::select! {
        _ = term.recv() => {}
        _ = int.recv() => {}
        _ = hup.recv() => {}
    }
    StopReason::Signal
}
