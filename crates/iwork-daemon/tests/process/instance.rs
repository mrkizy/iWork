//! 单实例与托管实例的等锁接班（方案 2.3；S1、S2、S11c、S11d）。

use std::path::PathBuf;
use std::time::{Duration, Instant};

use iwork_ipc::rendezvous::{self, Phase};

use crate::support::{Sandbox, alive, code};

const START: Duration = Duration::from_secs(20);

/// 卡验收 1（S1）：同时起 10 个，恰好一个活着、能握手，其余 9 个退出码 0，库没被写过。
#[test]
fn ten_concurrent_starts_one_survives() {
    let sb = Sandbox::new();
    let mut ds: Vec<_> = (0..10).map(|_| sb.spawn(&[], &[])).collect();
    let deadline = Instant::now() + START;
    let mut exited = Vec::new();
    while exited.len() < 9 {
        assert!(Instant::now() < deadline, "9 个没在时限内让路");
        for (i, d) in ds.iter_mut().enumerate() {
            if !exited.contains(&i)
                && let Some(s) = d.try_exit()
            {
                assert_eq!(code(s), Some(0), "让路的实例应以 0 退出：{}", d.stderr());
                exited.push(i);
            }
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    let survivor = (0..10).find(|i| !exited.contains(i)).unwrap();
    let pid = ds[survivor].pid();
    sb.wait_ready(pid, START);
    std::thread::sleep(Duration::from_millis(200));
    assert!(ds[survivor].try_exit().is_none(), "活下来的那个不该退出");
    let welcome = crate::support::rt().block_on(async {
        let (c, _in) = sb.connect().await.expect("握手");
        c.welcome().clone()
    });
    assert_eq!(welcome.head_seq, 0, "库没被写过");
    assert_eq!(rendezvous::read_lock_info(&sb.run_dir()).unwrap().pid, pid);
}

/// S2：上次崩溃留下 socket 与锁文件，`kill -9` 后直接再起能到就绪、能连上。
#[test]
fn stale_socket_and_lockfile_do_not_block_start() {
    let sb = Sandbox::new();
    let mut a = sb.spawn(&[], &[]);
    sb.wait_ready(a.pid(), START);
    a.kill9();
    assert!(sb.run_dir().join("d.sock").exists(), "对照：残留 socket 在");
    assert!(sb.run_dir().join(rendezvous::LOCK_FILE).exists());
    let b = sb.spawn(&[], &[]);
    sb.wait_ready(b.pid(), START);
    crate::support::rt().block_on(async { sb.connect().await.expect("握手") });
}

/// 方案 7.2「对拍」：没设 `XDG_RUNTIME_DIR` 时，ipc 的 socket 目录就是数据目录的 `run/`（只有一把锁）。
#[test]
fn socket_dir_is_under_data_dir() {
    let sb = Sandbox::new();
    let a = sb.spawn(&[], &[]);
    sb.wait_ready(a.pid(), START);
    assert_eq!(sb.run_dir(), sb.data_root().join("run"));
    assert!(sb.db().exists(), "库在数据目录里");
    assert!(sb.run_dir().join("d.sock").exists());
}

/// S11c：先起非托管的，再起托管的 → 后者不退出、不碰 socket；前者被 `kill -9` → 后者接班到就绪（同一个 pid，exec 过）。
#[test]
fn standby_takes_over_after_kill9() {
    let sb = Sandbox::new();
    let mut a = sb.spawn(&[], &[]);
    sb.wait_ready(a.pid(), START);
    let mut b = sb.spawn(&["--service"], &[]);
    b.wait_stderr("等锁接班", START);
    std::thread::sleep(Duration::from_millis(300));
    assert!(b.try_exit().is_none(), "托管实例撞锁后应等锁，不退出");
    assert_eq!(sb.status().unwrap().pid, a.pid(), "等锁的实例不碰状态文件");
    a.kill9();
    sb.wait_ready(b.pid(), START);
    assert!(alive(b.pid()));
}

/// S3：前任被存储拒绝（同一个程序）→ 等锁的托管实例不接班、以 0 退出。
#[test]
fn standby_does_not_take_over_after_refusal() {
    let sb = Sandbox::new();
    crate::boot::make_mismatched_db(&sb);
    let a = sb.spawn(&[], &[("IWORK_DEV_PAUSE_AT", "before-exit")]);
    a.wait_stderr("IWORK-PAUSED before-exit", START);
    assert!(matches!(sb.status().unwrap().phase, Phase::Refused(_)));
    let mut b = sb.spawn(&["--service"], &[]);
    b.wait_stderr("等锁接班", START);
    let mut a = a;
    a.kill9();
    assert_eq!(code(b.wait_exit(START)), Some(0), "{}", b.stderr());
    assert_eq!(sb.status().unwrap().pid, a.pid(), "没接班");
}

/// S11d ①：前任绑不上 socket（`Exited{13}`，可重试）→ 等锁的托管实例接班到就绪。
#[test]
fn standby_takes_over_after_bind_failure() {
    let sb = Sandbox::new();
    let mut a = sb.spawn(
        &[],
        &[
            ("IWORK_DEV_FAIL_AT", "bind"),
            ("IWORK_DEV_PAUSE_AT", "before-exit"),
        ],
    );
    a.wait_stderr("IWORK-PAUSED before-exit", START);
    assert!(matches!(
        sb.status().unwrap().phase,
        Phase::Exited {
            code: rendezvous::ExitCode::BindFailed,
            ..
        }
    ));
    let b = sb.spawn(&["--service"], &[]);
    b.wait_stderr("等锁接班", START);
    a.kill9();
    sb.wait_ready(b.pid(), START);
}

/// 接班的进程是磁盘上现在的程序：等锁期间把程序文件换成一个脚本，接班时 `exec` 的就是这个脚本。
#[test]
fn standby_execs_before_boot() {
    let sb = Sandbox::new();
    let bin_dir = sb.home.join("bin");
    std::fs::create_dir_all(&bin_dir).unwrap();
    let exe = bin_dir.join("iwork-daemon");
    std::fs::copy(crate::support::DAEMON, &exe).unwrap();
    let mut a = sb.spawn_exe(&exe, &[], &[]);
    sb.wait_ready(a.pid(), START);
    let mut b = sb.spawn_exe(&exe, &["--service"], &[]);
    b.wait_stderr("等锁接班", START);

    // 换成脚本：被 exec 时把收到的参数写进标记文件后退出 7
    let marker: PathBuf = sb.home.join("execd");
    let script = format!("#!/bin/sh\necho \"$@\" > '{}'\nexit 7\n", marker.display());
    let tmp = bin_dir.join("next");
    std::fs::write(&tmp, script).unwrap();
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o755)).unwrap();
    std::fs::rename(&tmp, &exe).unwrap();

    a.kill9();
    assert_eq!(code(b.wait_exit(START)), Some(7), "{}", b.stderr());
    let got = std::fs::read_to_string(&marker).unwrap();
    assert_eq!(got.trim(), "--dev-trust --service", "原样的参数");
}
