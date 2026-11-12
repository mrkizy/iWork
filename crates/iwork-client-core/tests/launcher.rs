//! 客户端拉起（T04 方案 2.7；S3、S11d、S14、S19）。连接尝试与拉起进程用假的；连上的那一刻交出一条
//! 事先连好的真连接（`iwork-fake-daemon`，真的 Unix socket + ipc 服务端）。事先连好是因为真握手要校验
//! 对端身份，Linux 上要对整个测试程序算 SHA-256，耗时随构建大小与机器负载变；放进预算里量，测的就是
//! 机器快不快，不是拉起逻辑。
#![cfg(unix)]

use std::collections::VecDeque;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use iwork_client_core::launcher::{
    Connect, Diagnostics, LaunchConfig, LaunchError, Launcher, Spawn, diagnose, should_launch,
};
use iwork_ipc::client::{Client, ClientConfig, ConnectError, Incoming};
use iwork_ipc::peer::RejectReason;
use iwork_ipc::rendezvous::{
    self, ExeStamp, ExitCode, Phase, Refusal, Relaunchable, StartupStatus, StopReason, relaunchable,
};
use iwork_proto::pb;
use tokio::time::Instant;

/// 一次假连接尝试的结果。
#[derive(Clone)]
enum Step {
    Io,
    Err(fn() -> ConnectError),
    /// 永不返回（不回话的端点、很慢的身份校验）
    Hang,
    /// 等一会儿后连上假后台
    DelayThenUp(Duration),
}

#[derive(Default)]
struct Ctl {
    /// 后台「起来了」之后每次尝试都连真的假后台
    up: AtomicBool,
    script: Mutex<VecDeque<Step>>,
    attempts: AtomicUsize,
    spawns: AtomicUsize,
    spawn_fails: AtomicBool,
    /// 拉起要卡多久（毫秒）
    spawn_delay_ms: AtomicU64,
    /// 拉起后后台是否真的起来
    spawn_brings_up: AtomicBool,
    /// 事先和假后台握好手的那一条连接；每个用例最多连上一次
    real: Mutex<Option<(Client, Incoming)>>,
}

#[derive(Clone)]
struct FakeConnect(Arc<Ctl>);

impl Connect for FakeConnect {
    fn connect(
        &self,
    ) -> impl std::future::Future<Output = Result<(Client, Incoming), ConnectError>> + Send + 'static
    {
        let ctl = self.0.clone();
        ctl.attempts.fetch_add(1, Ordering::SeqCst);
        let step = if ctl.up.load(Ordering::SeqCst) {
            None
        } else {
            Some(ctl.script.lock().unwrap().pop_front().unwrap_or(Step::Io))
        };
        let real = || {
            ctl.real
                .lock()
                .unwrap()
                .take()
                .expect("假后台没起，或者这条连接已经交出去过")
        };
        let real = match step {
            None | Some(Step::DelayThenUp(_)) => Some(real()),
            _ => None,
        };
        async move {
            match step {
                None => Ok(real.unwrap()),
                Some(Step::Io) => Err(ConnectError::Io(std::io::ErrorKind::NotFound.into())),
                Some(Step::Err(f)) => Err(f()),
                Some(Step::Hang) => std::future::pending().await,
                Some(Step::DelayThenUp(d)) => {
                    tokio::time::sleep(d).await;
                    Ok(real.unwrap())
                }
            }
        }
    }
}

struct FakeSpawn(Arc<Ctl>);

impl Spawn for FakeSpawn {
    fn spawn(&self, _exe: &Path, _args: &[&str]) -> std::io::Result<()> {
        self.0.spawns.fetch_add(1, Ordering::SeqCst);
        // 慢的 fork/exec（慢盘、杀毒扫描）：同步地卡住
        std::thread::sleep(Duration::from_millis(
            self.0.spawn_delay_ms.load(Ordering::SeqCst),
        ));
        if self.0.spawn_fails.load(Ordering::SeqCst) {
            return Err(std::io::Error::new(
                std::io::ErrorKind::PermissionDenied,
                "没有执行权限",
            ));
        }
        if self.0.spawn_brings_up.load(Ordering::SeqCst) {
            self.0.up.store(true, Ordering::SeqCst);
        }
        Ok(())
    }
}

struct World {
    dir: tempfile::TempDir,
    ctl: Arc<Ctl>,
    _daemon: Option<iwork_fake_daemon::FakeDaemon>,
}

impl World {
    async fn new(with_daemon: bool) -> World {
        let dir = tempfile::Builder::new()
            .prefix("iwl-")
            .tempdir_in("/tmp")
            .unwrap();
        std::fs::write(dir.path().join("iwork-daemon"), b"program A").unwrap();
        let ctl = Arc::new(Ctl {
            spawn_brings_up: AtomicBool::new(true),
            ..Default::default()
        });
        let daemon = if with_daemon {
            let d = iwork_fake_daemon::FakeDaemon::start(dir.path().join("d.sock"))
                .await
                .unwrap();
            let real = iwork_client_core::launcher::IpcConnect {
                endpoint: d.endpoint(),
                own: iwork_fake_daemon::self_trusting_identity().unwrap(),
                cfg: ClientConfig::new(pb::ClientKind::Desktop, "t"),
            };
            *ctl.real.lock().unwrap() = Some(real.connect().await.expect("和假后台握手"));
            Some(d)
        } else {
            None
        };
        World {
            dir,
            ctl,
            _daemon: daemon,
        }
    }

    fn exe(&self) -> PathBuf {
        self.dir.path().join("iwork-daemon")
    }

    fn launcher(&self, budget: Duration) -> Launcher<FakeConnect, FakeSpawn> {
        Launcher::new(
            FakeConnect(self.ctl.clone()),
            FakeSpawn(self.ctl.clone()),
            LaunchConfig {
                daemon_exe: self.exe(),
                budget,
                run_dir: Some(self.dir.path().to_path_buf()),
                endpoint: Ok(self.dir.path().join("d.sock")),
                args: vec![],
            },
        )
    }

    fn script(&self, steps: &[Step]) {
        self.ctl
            .script
            .lock()
            .unwrap()
            .extend(steps.iter().cloned());
    }

    fn spawns(&self) -> usize {
        self.ctl.spawns.load(Ordering::SeqCst)
    }

    fn write_status(&self, phase: Phase, stamp: ExeStamp) {
        rendezvous::write_status(self.dir.path(), &status(phase, stamp)).unwrap();
    }

    fn stamp(&self) -> ExeStamp {
        ExeStamp::of(&self.exe()).unwrap()
    }
}

fn status(phase: Phase, stamp: ExeStamp) -> StartupStatus {
    StartupStatus {
        pid: 1,
        version: "v".into(),
        protocol_major: 1,
        exe_stamp: stamp,
        log_dir: PathBuf::from("/logs"),
        updated_at_ms: 0,
        phase,
        ipc_faults: 0,
        not_cleared: 0,
    }
}

fn not_ready(r: Result<(Client, Incoming), LaunchError>) -> Box<Diagnostics> {
    match r {
        Err(LaunchError::NotReady(d)) => d,
        Err(e) => panic!("期望 NotReady：{e:?}"),
        Ok(_) => panic!("期望 NotReady，却连上了"),
    }
}

/// 专测「到预算就返回」的用例用的预算。
const BUDGET: Duration = Duration::from_secs(1);
/// 验「拉不拉起、连不连得上」的用例用的预算：要的是结果，不是时延；
/// 1s 在慢的 CI 机器上不够走完拉起和握手（这些路径一出结果就返回，预算大不拖慢用例）。
const LAUNCH_BUDGET: Duration = Duration::from_secs(20);

/// 只有 `Io` 拉起；五种别的错误原样返回 `Connect`，一次都不拉。
#[tokio::test]
async fn only_io_error_launches() {
    let others: [fn() -> ConnectError; 5] = [
        || ConnectError::PeerRejected(RejectReason::Unavailable { reason: "x".into() }),
        || ConnectError::ClosedBeforeWelcome,
        || ConnectError::VersionMismatch {
            client_major: 1,
            server_major: 2,
        },
        || ConnectError::Timeout,
        || ConnectError::Protocol("x".into()),
    ];
    for f in others {
        let w = World::new(false).await;
        w.script(&[Step::Err(f)]);
        match w.launcher(LAUNCH_BUDGET).connect_or_launch().await {
            Err(LaunchError::Connect(e)) => assert_eq!(e.to_string(), f().to_string()),
            other => panic!("{other:?}", other = other.err()),
        }
        assert_eq!(w.spawns(), 0, "{}", f());
    }
    // 对照：Io → 拉起一次、连上
    let w = World::new(true).await;
    w.script(&[Step::Io]);
    assert!(w.launcher(LAUNCH_BUDGET).connect_or_launch().await.is_ok());
    assert_eq!(w.spawns(), 1);
}

/// 两次拉起至少隔 2s。
#[tokio::test(start_paused = true)]
async fn relaunch_rate_limited() {
    let w = World::new(false).await;
    w.ctl.spawn_brings_up.store(false, Ordering::SeqCst);
    let mut l = w.launcher(BUDGET);
    for _ in 0..2 {
        not_ready(l.connect_or_launch().await);
    }
    assert_eq!(w.spawns(), 1, "2s 内只拉一次（两次调用共用了 2s 预算）");
    tokio::time::sleep(Duration::from_secs(1)).await;
    not_ready(l.connect_or_launch().await);
    assert_eq!(w.spawns(), 2, "隔够 2s 再拉");
}

/// 可执行文件起不来 → NotReady 带上拉起失败的原因。
#[tokio::test]
async fn spawn_failure_is_reported() {
    let w = World::new(false).await;
    w.ctl.spawn_fails.store(true, Ordering::SeqCst);
    let d = not_ready(w.launcher(LAUNCH_BUDGET).connect_or_launch().await);
    assert_eq!(d.stage, "拉起失败");
    assert!(d.spawn_error.as_deref().unwrap().contains("没有执行权限"));
    assert!(d.to_view().lines.iter().any(|(k, _)| k == "拉起失败"));
}

/// S19：接受连接却不回话的端点（尝试永不返回）→ 到预算就返回 NotReady，不拉起。
#[tokio::test(start_paused = true)]
async fn silent_server_returns_at_deadline() {
    let w = World::new(false).await;
    w.script(&[Step::Hang]);
    let t = Instant::now();
    let d = not_ready(w.launcher(BUDGET).connect_or_launch().await);
    assert_eq!(t.elapsed(), BUDGET);
    assert_eq!(d.stage, "连接中");
    assert_eq!(w.spawns(), 0);
}

/// S19：对端身份校验很慢（3s）→ 每次调用都在预算内返回；底层连接只发生一次，第三次调用拿到那次尝试的结果。
#[tokio::test]
async fn slow_peer_check_returns_at_deadline_and_attempt_is_reused() {
    let w = World::new(true).await;
    w.script(&[Step::DelayThenUp(Duration::from_millis(2500))]);
    let mut l = w.launcher(BUDGET);
    for _ in 0..2 {
        let t = std::time::Instant::now();
        not_ready(l.connect_or_launch().await);
        assert!(
            t.elapsed() < BUDGET + Duration::from_millis(300),
            "{:?}",
            t.elapsed()
        );
    }
    assert!(
        l.connect_or_launch().await.is_ok(),
        "第三次拿到那次尝试的结果"
    );
    assert_eq!(w.ctl.attempts.load(Ordering::SeqCst), 1, "没有叠新的尝试");
    assert_eq!(w.spawns(), 0);
}

/// S19：起进程本身很慢（2.5s，超过 1s 预算，也超过两次拉起的最小间隔 2s）→ 每次调用都在预算内返回「拉起中」；
/// 后面的调用接着等同一次拉起，过了间隔也不叠新的；拉起返回、后台起来后连上。
#[tokio::test]
async fn slow_spawn_returns_at_deadline_and_is_reused() {
    let w = World::new(true).await;
    w.ctl.spawn_delay_ms.store(2500, Ordering::SeqCst);
    let mut l = w.launcher(BUDGET);
    for _ in 0..2 {
        let t = std::time::Instant::now();
        let d = not_ready(l.connect_or_launch().await);
        assert!(
            t.elapsed() < BUDGET + Duration::from_millis(300),
            "{:?}",
            t.elapsed()
        );
        assert_eq!(d.stage, "拉起中");
    }
    assert!(
        l.connect_or_launch().await.is_ok(),
        "第三次等到那次拉起返回、连上"
    );
    assert_eq!(w.spawns(), 1, "没有叠新的拉起");
}

/// 把会合点的锁文件与状态文件换成 FIFO：打开读会一直卡住，直到有人打开写（模拟很慢的文件系统）。
fn slow_rendezvous(dir: &Path) -> Vec<PathBuf> {
    [rendezvous::LOCK_FILE, rendezvous::STATUS_FILE]
        .iter()
        .map(|name| {
            let p = dir.join(name);
            let c = std::ffi::CString::new(p.to_str().unwrap()).unwrap();
            // SAFETY：只在测试的临时目录里建 FIFO
            assert_eq!(unsafe { libc::mkfifo(c.as_ptr(), 0o600) }, 0);
            p
        })
        .collect()
}

/// 放开卡在 FIFO 上的读：先把 FIFO 改名挪开（之后才来读的直接 NotFound，不会再卡），再在 1s 内反复以非阻塞方式
/// 打开挪走后的写端又关掉——已经卡在打开或读上的一方读到 EOF。测试结束时运行时要等阻塞线程收尾，不放开就挂住。
fn release(fifos: &[PathBuf]) {
    use std::os::unix::fs::OpenOptionsExt;
    let gone: Vec<PathBuf> = fifos
        .iter()
        .map(|p| {
            let g = p.with_extension("gone");
            std::fs::rename(p, &g).unwrap();
            g
        })
        .collect();
    let until = std::time::Instant::now() + Duration::from_secs(1);
    while std::time::Instant::now() < until {
        for g in &gone {
            let _ = std::fs::OpenOptions::new()
                .write(true)
                .custom_flags(libc::O_NONBLOCK)
                .open(g);
        }
        std::thread::sleep(Duration::from_millis(20));
    }
}

/// 2/F-3：会合点读得很慢（锁文件、状态文件都卡住）→ 起进程前读状态这一步不越过截止时间，返回「读会合点」，
/// 诊断里写明没读完；不拉起（没读到状态就不知道该不该拉）。
#[tokio::test]
async fn slow_status_read_returns_at_deadline() {
    let w = World::new(false).await;
    let fifos = slow_rendezvous(w.dir.path());
    w.script(&[Step::Io]);
    let t = std::time::Instant::now();
    let d = not_ready(w.launcher(BUDGET).connect_or_launch().await);
    assert!(
        t.elapsed() < BUDGET + Duration::from_millis(300),
        "{:?}",
        t.elapsed()
    );
    assert_eq!(d.stage, "读会合点");
    assert!(!d.rendezvous_read);
    assert!(d.to_view().lines.iter().any(|(k, _)| k == "会合点"));
    assert_eq!(w.spawns(), 0);
    release(&fifos);
}

/// 2/F-3：连接尝试到期（不回话的端点）时凑诊断，会合点读得很慢 → 照样在截止时间返回，不等诊断。
#[tokio::test]
async fn slow_diagnostics_do_not_extend_the_deadline() {
    let w = World::new(false).await;
    let fifos = slow_rendezvous(w.dir.path());
    w.script(&[Step::Hang]);
    let t = std::time::Instant::now();
    let d = not_ready(w.launcher(BUDGET).connect_or_launch().await);
    assert!(
        t.elapsed() < BUDGET + Duration::from_millis(300),
        "{:?}",
        t.elapsed()
    );
    assert_eq!(d.stage, "连接中");
    assert!(!d.rendezvous_read);
    assert!(d.lock.is_none() && d.status.is_none());
    release(&fifos);
}

/// S19：一直连不上、拉起了也不起来 → 起进程、退避睡眠都以同一个截止时间为上界：整个调用恰好用完预算。
#[tokio::test(start_paused = true)]
async fn deadline_covers_spawn_and_backoff() {
    let w = World::new(false).await;
    w.ctl.spawn_brings_up.store(false, Ordering::SeqCst);
    let t = Instant::now();
    let d = not_ready(w.launcher(BUDGET).connect_or_launch().await);
    assert_eq!(t.elapsed(), BUDGET);
    assert_eq!(d.stage, "等后台就绪");
    assert_eq!(w.spawns(), 1);
    // 退避 10、20、40…ms，每次醒来都试一次
    assert!(w.ctl.attempts.load(Ordering::SeqCst) >= 6);
}

/// S3：同一个程序已经被存储拒绝过 → 连调 5 次都不拉；程序换过了 → 拉；手动重试 → 拉。
#[tokio::test(start_paused = true)]
async fn refused_same_exe_is_not_relaunched() {
    let w = World::new(false).await;
    w.write_status(
        Phase::Refused(Refusal::StoreOpenFailed {
            message: "x".into(),
        }),
        w.stamp(),
    );
    let mut l = w.launcher(BUDGET);
    for _ in 0..5 {
        let d = not_ready(l.connect_or_launch().await);
        assert_eq!(d.stage, "同一个程序已经说过起不来");
        tokio::time::sleep(Duration::from_secs(3)).await;
    }
    assert_eq!(w.spawns(), 0);
}

#[tokio::test]
async fn refused_other_exe_is_relaunched() {
    let w = World::new(true).await;
    let other = ExeStamp {
        len: 1,
        modified_unix_ns: 1,
    };
    w.write_status(
        Phase::Refused(Refusal::StoreOpenFailed {
            message: "x".into(),
        }),
        other,
    );
    assert!(w.launcher(LAUNCH_BUDGET).connect_or_launch().await.is_ok());
    assert_eq!(w.spawns(), 1);
}

#[tokio::test]
async fn manual_retry_relaunches() {
    let w = World::new(true).await;
    w.write_status(
        Phase::Refused(Refusal::StoreOpenFailed {
            message: "x".into(),
        }),
        w.stamp(),
    );
    let mut l = w.launcher(LAUNCH_BUDGET);
    not_ready(l.connect_or_launch().await);
    assert_eq!(w.spawns(), 0);
    assert!(l.retry_now().await.is_ok());
    assert_eq!(w.spawns(), 1);
}

/// S11d：前任绑不上 socket（`Exited{13}`，可重试）→ 照常拉。
#[tokio::test]
async fn bind_failure_is_relaunched() {
    let w = World::new(true).await;
    w.write_status(
        Phase::Exited {
            code: ExitCode::BindFailed,
            reason: None,
            message: Some("x".into()),
        },
        w.stamp(),
    );
    assert!(w.launcher(LAUNCH_BUDGET).connect_or_launch().await.is_ok());
    assert_eq!(w.spawns(), 1);
}

/// S11d：用户完全退出之后又打开客户端 → 照常拉（客户端在连，就是用户回来了）。
#[tokio::test]
async fn user_quit_then_client_reopens_launches() {
    let w = World::new(true).await;
    w.write_status(
        Phase::Exited {
            code: ExitCode::Stopped,
            reason: Some(StopReason::UserQuit),
            message: None,
        },
        w.stamp(),
    );
    assert!(w.launcher(LAUNCH_BUDGET).connect_or_launch().await.is_ok());
    assert_eq!(w.spawns(), 1);
}

/// 客户端一列没有自己的判断：对每一种 Phase × 程序换没换，结果等于角色表按 `relaunchable` 查出来的那一格。
#[test]
fn follows_role_table() {
    let same = ExeStamp {
        len: 1,
        modified_unix_ns: 1,
    };
    let other = ExeStamp {
        len: 2,
        modified_unix_ns: 1,
    };
    let mut phases = vec![
        Phase::Starting { step: "x".into() },
        Phase::Ready,
        Phase::Refused(Refusal::StoreOpenFailed {
            message: "m".into(),
        }),
    ];
    for r in [
        StopReason::UserQuit,
        StopReason::Signal,
        StopReason::Upgrade,
        StopReason::Fatal,
    ] {
        phases.push(Phase::Stopping { reason: r });
    }
    for code in ExitCode::ALL {
        phases.push(Phase::Exited {
            code,
            reason: None,
            message: None,
        });
    }
    for phase in phases {
        for disk in [Some(same), Some(other), None] {
            let s = status(phase.clone(), same);
            let want = match relaunchable(&phase) {
                Relaunchable::Yes | Relaunchable::NotUntilUserReturns => true,
                Relaunchable::NotWithThisProgram => disk != Some(same),
            };
            assert_eq!(should_launch(Some(&s), disk), want, "{phase:?} {disk:?}");
        }
    }
    assert!(should_launch(None, Some(same)), "读不到状态按「再拉有用」");
}

/// 卡验收 3a：协议主版本不符 → 诊断里有数据库与程序的两个主版本号和备份路径；不是「装回新版」那一类。
/// 对照：库被更新的版本写过 → 带「装回更新的那一版」提示。
#[test]
fn diagnostics_show_both_majors_and_backup() {
    let dir = tempfile::tempdir().unwrap();
    rendezvous::write_status(
        dir.path(),
        &status(
            Phase::Refused(Refusal::ProtocolMajorMismatch {
                database: 3,
                program: 2,
                backup: PathBuf::from("/data/backups/20261111-major-v3.db"),
            }),
            ExeStamp {
                len: 0,
                modified_unix_ns: 0,
            },
        ),
    )
    .unwrap();
    let d = diagnose("x", Some(dir.path()), Ok(PathBuf::from("/s")), None, None);
    let view = d.to_view();
    let get = |k: &str| {
        view.lines
            .iter()
            .find(|(l, _)| l == k)
            .map(|(_, v)| v.clone())
            .unwrap_or_else(|| panic!("缺 {k}：{:?}", view.lines))
    };
    assert_eq!(get("数据库主版本"), "3");
    assert_eq!(get("程序主版本"), "2");
    assert_eq!(get("备份"), "/data/backups/20261111-major-v3.db");
    assert_eq!(get("日志目录"), "/logs");
    assert!(!view.newer_schema);

    rendezvous::write_status(
        dir.path(),
        &status(
            Phase::Refused(Refusal::StoreOpenFailed {
                message: format!("这个库{}程序写过", rendezvous::WRITTEN_BY_NEWER),
            }),
            ExeStamp {
                len: 0,
                modified_unix_ns: 0,
            },
        ),
    )
    .unwrap();
    let d = diagnose("x", Some(dir.path()), Ok(PathBuf::from("/s")), None, None);
    assert!(d.to_view().newer_schema);
}

/// S14：run 目录权限被改宽 → Environment，带路径与原因；不自动修。
#[test]
fn insecure_run_dir_reports_environment_error() {
    use std::os::unix::fs::PermissionsExt;
    let dir = tempfile::Builder::new()
        .prefix("iwl-")
        .tempdir_in("/tmp")
        .unwrap();
    let run = dir.path().join("run");
    std::fs::create_dir(&run).unwrap();
    std::fs::set_permissions(&run, std::fs::Permissions::from_mode(0o777)).unwrap();
    let own = iwork_fake_daemon::self_trusting_identity().unwrap();
    match iwork_client_core::launcher::for_run_dir(
        &run,
        &own,
        ClientConfig::new(pb::ClientKind::Desktop, "t"),
        PathBuf::from("/x"),
    ) {
        Err(LaunchError::Environment(m)) => assert!(m.contains("权限"), "{m}"),
        Err(e) => panic!("{e:?}"),
        Ok(_) => panic!("不安全的目录应拒绝"),
    }
    let mode = std::fs::metadata(&run).unwrap().permissions().mode() & 0o777;
    assert_eq!(mode, 0o777, "没有自动修");
}
