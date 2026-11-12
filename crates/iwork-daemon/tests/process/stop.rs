//! 停机流程、监督器的整体退出、等锁实例对停机原因的反应（方案 2.8、2.10；S9、S10、S11c、S11d、S20）。

use std::time::{Duration, Instant};

use iwork_ipc::rendezvous::{ExitCode, Phase, StopReason};
use iwork_proto::pb;

use crate::seed;
use crate::support::{
    Sandbox, alive, code, group_alive, log_text, readies, shutdown_daemon, signal,
    wait_ready_logged,
};

const START: Duration = Duration::from_secs(20);

fn exited(sb: &Sandbox) -> Phase {
    sb.status().unwrap().phase
}

/// S9：SIGTERM → 退出码 0、状态 `Exited{Stopped, Signal}`、假工作的进程组都空了、库能再打开且 head 不变。
#[test]
fn sigterm_stops_gracefully() {
    let sb = Sandbox::new();
    let mut d = sb.spawn(&[], &[("IWORK_DEV_WORK", "turns=2")]);
    sb.wait_ready(d.pid(), START);
    let pgids = d.dev_pgids();
    assert_eq!(pgids.len(), 2);
    assert!(pgids.iter().all(|g| group_alive(*g)), "对照：进程组在跑");
    let head = seed::events_after(&sb, 0).len();
    signal(d.pid(), libc::SIGTERM);
    assert_eq!(code(d.wait_exit(START)), Some(0), "{}", d.stderr());
    assert_eq!(
        exited(&sb),
        Phase::Exited {
            code: ExitCode::Stopped,
            reason: Some(StopReason::Signal),
            message: None
        }
    );
    assert!(pgids.iter().all(|g| !group_alive(*g)), "子进程树为空");
    assert_eq!(seed::events_after(&sb, 0).len(), head, "head 不变");
    assert_eq!(sb.status().unwrap().not_cleared, 0);
    // 1/F-2：真实装配下第 6 步真的关了存储（没有漏掉的持有者）
    let log = log_text(&sb);
    assert!(log.contains("存储已关闭"), "{log}");
    assert!(
        !log.contains("关存储出错") && !log.contains("别的持有者"),
        "{log}"
    );
}

/// 用户完全退出：有成员在跑时先要确认；确认后走取消流程再关会话，进程组为空，退出码 0。
#[test]
fn abort_all_leaves_no_child_processes() {
    let sb = Sandbox::new();
    let mut d = sb.spawn(&[], &[("IWORK_DEV_WORK", "turns=2,cards=1")]);
    sb.wait_ready(d.pid(), START);
    let pgids = d.dev_pgids();
    let r = shutdown_daemon(&sb, false);
    match r.body {
        Some(pb::response::Body::Error(e)) => {
            assert_eq!(e.code, pb::ErrorCode::PreconditionFailed as i32);
            assert_eq!(e.details["running_turns"], "2");
            assert_eq!(e.details["executing_cards"], "1");
        }
        other => panic!("{other:?}"),
    }
    assert!(d.try_exit().is_none(), "没确认就不停");
    let r = shutdown_daemon(&sb, true);
    assert!(
        matches!(r.body, Some(pb::response::Body::Accepted(_))),
        "{r:?}"
    );
    assert_eq!(code(d.wait_exit(START)), Some(0), "{}", d.stderr());
    assert!(pgids.iter().all(|g| !group_alive(*g)));
    assert!(
        log_text(&sb).contains("dev work：cancel_all"),
        "走了取消流程"
    );
    assert!(matches!(
        exited(&sb),
        Phase::Exited {
            reason: Some(StopReason::UserQuit),
            ..
        }
    ));
}

/// S20：第 3 步报没清空 → 停机照常走完、状态文件 `not_cleared == 1`、日志有 `warn`；
/// 升级路径照常 `exec`（同一个 pid 再次就绪）。
#[test]
fn uncleared_tree_is_recorded_and_stop_proceeds() {
    let sb = Sandbox::new();
    let mut d = sb.spawn(&[], &[("IWORK_DEV_WORK", "turns=1,uncleared")]);
    sb.wait_ready(d.pid(), START);
    signal(d.pid(), libc::SIGTERM);
    assert_eq!(code(d.wait_exit(START)), Some(0), "{}", d.stderr());
    assert_eq!(sb.status().unwrap().not_cleared, 1);
    assert!(log_text(&sb).contains("到点没清空"), "{}", log_text(&sb));

    let trigger = sb.home.join("upgrade-now");
    let spec = format!("{}:upgrade", trigger.display());
    let launched = readies(&sb).len();
    let d = sb.spawn(
        &[],
        &[
            ("IWORK_DEV_WORK", "turns=1,uncleared"),
            ("IWORK_DEV_STOP_ON", &spec),
        ],
    );
    // 先等这一次启动自己的就绪日志落盘，再记升级前的基线：只等状态文件就记，迟到的这一行会被当成 exec 之后的
    // 就绪，没 exec 也能过（第 1 轮审查 1/F-2）
    wait_ready_logged(&sb, d.pid(), launched, START);
    let before = readies(&sb).len();
    std::fs::write(&trigger, b"").unwrap();
    let deadline = Instant::now() + START;
    while readies(&sb).len() < before + 1 {
        assert!(
            Instant::now() < deadline,
            "exec 后没再次就绪：{}",
            d.stderr()
        );
        std::thread::sleep(Duration::from_millis(50));
    }
    sb.wait_ready(d.pid(), START);
    assert!(alive(d.pid()), "同一个 pid，exec 过");
}

/// S10：停机卡在第 3 步 → 15s 硬上限强退；退出状态按 `Stopping{Signal}` 算（0）；随后能正常再起。
#[test]
fn stuck_stop_is_forced_at_deadline() {
    let sb = Sandbox::new();
    let mut d = sb.spawn(&[], &[("IWORK_DEV_PAUSE_AT", "stop-abort")]);
    sb.wait_ready(d.pid(), START);
    let t = Instant::now();
    signal(d.pid(), libc::SIGTERM);
    d.wait_stderr("IWORK-PAUSED stop-abort", START);
    let status = d.wait_exit(Duration::from_secs(30));
    let took = t.elapsed();
    assert!(
        took >= Duration::from_secs(14) && took <= Duration::from_secs(16),
        "{took:?}"
    );
    assert_eq!(code(status), Some(0));
    assert!(d.stderr().contains("强制退出"));
    assert!(log_text(&sb).contains("停机超过硬上限"), "强退前排空了日志");
    assert_eq!(
        exited(&sb),
        Phase::Stopping {
            reason: StopReason::Signal
        },
        "状态停在第 1 步写下的那一格"
    );
    let again = sb.spawn(&[], &[]);
    sb.wait_ready(again.pid(), START);
}

/// S11d ②：用户完全退出、停机卡住被硬上限强退（磁盘上是 `Stopping{UserQuit}`）→ 它自己报 0，
/// 等锁的托管实例也以 0 退出、不接班。
#[test]
fn standby_stays_down_after_user_quit_hard_timeout() {
    let sb = Sandbox::new();
    let mut a = sb.spawn(&[], &[("IWORK_DEV_PAUSE_AT", "stop-abort")]);
    sb.wait_ready(a.pid(), START);
    let mut b = sb.spawn(&["--service"], &[]);
    b.wait_stderr("等锁接班", START);
    let r = shutdown_daemon(&sb, true);
    assert!(matches!(r.body, Some(pb::response::Body::Accepted(_))));
    assert_eq!(code(a.wait_exit(Duration::from_secs(30))), Some(0));
    assert_eq!(
        exited(&sb),
        Phase::Stopping {
            reason: StopReason::UserQuit
        }
    );
    assert_eq!(code(b.wait_exit(START)), Some(0), "{}", b.stderr());
    assert_eq!(sb.status().unwrap().pid, a.pid(), "没接班");
}

/// S11c：前任收到 `ShutdownDaemon`（用户退出）→ 等锁的托管实例以 0 退出。
#[test]
fn standby_exits_after_user_quit() {
    let sb = Sandbox::new();
    let mut a = sb.spawn(&[], &[]);
    sb.wait_ready(a.pid(), START);
    let mut b = sb.spawn(&["--service"], &[]);
    b.wait_stderr("等锁接班", START);
    let r = shutdown_daemon(&sb, false);
    assert!(matches!(r.body, Some(pb::response::Body::Accepted(_))));
    assert_eq!(code(a.wait_exit(START)), Some(0));
    assert_eq!(code(b.wait_exit(START)), Some(0), "{}", b.stderr());
    assert_eq!(sb.status().unwrap().pid, a.pid(), "没接班");
}

/// S11c：先起非托管的，再起托管的；前者运行期故障以 20 退出 → 后者接班到就绪。
#[test]
fn standby_takes_over_after_fatal_exit() {
    let sb = Sandbox::new();
    let trigger = sb.home.join("fatal-now");
    let spec = format!("{}:fatal", trigger.display());
    let mut a = sb.spawn(&[], &[("IWORK_DEV_STOP_ON", &spec)]);
    sb.wait_ready(a.pid(), START);
    let b = sb.spawn(&["--service"], &[]);
    b.wait_stderr("等锁接班", START);
    std::fs::write(&trigger, b"").unwrap();
    assert_eq!(code(a.wait_exit(START)), Some(20), "{}", a.stderr());
    sb.wait_ready(b.pid(), START);
}

/// S11c：托管实例运行期故障 → 20；自行重启时 `exec` 失败 → 21（服务管理器据此再拉）。
#[test]
fn service_mode_fatal_exits_nonzero() {
    let sb = Sandbox::new();
    let trigger = sb.home.join("now");
    let spec = format!("{}:fatal", trigger.display());
    let mut d = sb.spawn(&["--service"], &[("IWORK_DEV_STOP_ON", &spec)]);
    sb.wait_ready(d.pid(), START);
    std::fs::write(&trigger, b"").unwrap();
    assert_eq!(code(d.wait_exit(START)), Some(20), "{}", d.stderr());

    // exec 失败：程序文件在升级时不见了
    let exe = sb.home.join("bin/iwork-daemon");
    std::fs::create_dir_all(exe.parent().unwrap()).unwrap();
    crate::support::copy_exe(std::path::Path::new(crate::support::DAEMON), &exe);
    let spec = format!("{}:upgrade", trigger.display());
    let mut d = sb.spawn_exe(&exe, &["--service"], &[("IWORK_DEV_STOP_ON", &spec)]);
    sb.wait_ready(d.pid(), START);
    std::fs::remove_file(&exe).unwrap();
    std::fs::write(&trigger, b"").unwrap();
    assert_eq!(code(d.wait_exit(START)), Some(21), "{}", d.stderr());
    assert!(d.stderr().contains("exec 失败"));
}

/// S11e：客户端拉起的实例在启动阶段（恢复这一步）失败、以 20 退出 → 等锁的托管实例接班。
#[test]
fn startup_fatal_is_relaunched_by_standby() {
    let sb = Sandbox::new();
    let mut a = sb.spawn(
        &[],
        &[
            ("IWORK_DEV_FAIL_AT", "recovery"),
            ("IWORK_DEV_PAUSE_AT", "before-exit"),
        ],
    );
    a.wait_stderr("IWORK-PAUSED before-exit", START);
    assert!(matches!(
        exited(&sb),
        Phase::Exited {
            code: ExitCode::Fatal,
            reason: None,
            ..
        }
    ));
    let b = sb.spawn(&["--service"], &[]);
    b.wait_stderr("等锁接班", START);
    a.kill9();
    sb.wait_ready(b.pid(), START);
}

/// S20（记录型，不设通过条件）：假运行时收到 stdin 结束后 2s 才退出；`kill -9` 后台并立刻重启，
/// 记录新后台就绪时旧进程组是否还在——给 T06 的桥接清空上界实测提供对照数据。
#[test]
#[ignore = "记录型用例：本机与夜间跑，看输出"]
fn old_tree_may_outlive_restart() {
    let sb = Sandbox::new();
    let mut a = sb.spawn(&[], &[("IWORK_DEV_WORK", "turns=1,eof-delay")]);
    sb.wait_ready(a.pid(), START);
    let pgids = a.dev_pgids();
    a.kill9();
    let t = Instant::now();
    let b = sb.spawn(&[], &[]);
    sb.wait_ready(b.pid(), START);
    let ready_after = t.elapsed();
    // 组长（模拟的桥接）读到 stdin 结束后 2s 才退出
    let leader_alive_at_ready = pgids.iter().any(|g| alive(*g));
    while pgids.iter().any(|g| alive(*g)) && t.elapsed() < Duration::from_secs(10) {
        std::thread::sleep(Duration::from_millis(20));
    }
    eprintln!(
        "kill -9 后 {ready_after:?} 新后台就绪，那时旧成员{}；旧成员在 kill -9 后 {:?} 退出",
        if leader_alive_at_ready {
            "还在"
        } else {
            "已经没了"
        },
        t.elapsed()
    );
    // 后台 sleep 不随 stdin 结束退出：由 `Daemon` 丢弃时按 pgid 收掉
    drop(a);
}

/// 方案 S5 / R10（2/F-1）：真实装配里存储写线程失效 → 真的 `SqliteStore::failed()` → store-health 结束 →
/// 监督器整体退出 → 停机「故障」列、跳过关存储 → 退出码 20、状态 `Exited{Fatal, Fatal}`。托管实例同样报 20。
#[test]
fn store_failure_exits_fatal() {
    for managed in [false, true] {
        let sb = Sandbox::new();
        let trigger = sb.home.join("break-writer");
        let env = trigger.display().to_string();
        let args: &[&str] = if managed { &["--service"] } else { &[] };
        let mut d = sb.spawn(args, &[("IWORK_DEV_BREAK_WRITER_ON", &env)]);
        sb.wait_ready(d.pid(), START);
        assert!(d.try_exit().is_none(), "对照：触发之前一直在跑");
        std::fs::write(&trigger, b"").unwrap();
        assert_eq!(code(d.wait_exit(START)), Some(20), "{}", d.stderr());
        assert_eq!(
            exited(&sb),
            Phase::Exited {
                code: ExitCode::Fatal,
                reason: Some(StopReason::Fatal),
                message: None
            }
        );
        let log = log_text(&sb);
        assert!(log.contains("存储不可用，整体退出"), "{log}");
        assert!(log.contains("存储已失效，跳过关存储"), "{log}");
        assert!(!log.contains("存储已关闭"), "{log}");
    }
}
