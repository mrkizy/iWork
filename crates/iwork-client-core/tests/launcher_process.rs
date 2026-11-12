//! 卡验收 2（真进程）：客户端退出后后台服务继续运行；客户端重开 1s 内连上。
//!
//! 用真的后台程序：`target/<profile>/iwork-daemon`，要求是 `dev` feature 构建的（后台要认 `--dev-trust`：
//! 测试进程不是受信映像）。先跑 `cargo build -p iwork-daemon --features dev`；也可以用环境变量
//! `IWORK_TEST_DAEMON` 指定。「拉起后台的客户端」就是本测试二进制自己：带 [`CHILD_ENV`] 只跑 `launcher_child`。
#![cfg(unix)]

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use iwork_client_core::launcher;
use iwork_ipc::client::ClientConfig;
use iwork_ipc::peer::{SelfIdentity, TrustPolicy};
use iwork_ipc::rendezvous::{self, Phase};
use iwork_proto::pb;

const CHILD_ENV: &str = "IWORK_LAUNCHER_CHILD";

fn daemon_exe() -> PathBuf {
    if let Some(p) = std::env::var_os("IWORK_TEST_DAEMON") {
        return PathBuf::from(p);
    }
    // 测试二进制在 target/<profile>/deps/ 下，后台在 target/<profile>/
    let exe = std::env::current_exe().unwrap();
    let p = exe.parent().unwrap().parent().unwrap().join("iwork-daemon");
    assert!(
        p.exists(),
        "找不到 {}：先跑 cargo build -p iwork-daemon --features dev（或设 IWORK_TEST_DAEMON）",
        p.display()
    );
    p
}

fn run_dir(home: &Path) -> PathBuf {
    iwork_ipc::endpoint::unix_run_dir(|_| None, Some(home.to_path_buf())).unwrap()
}

fn client_cfg() -> ClientConfig {
    let mut cfg = ClientConfig::new(pb::ClientKind::DevTool, "test");
    cfg.trust = TrustPolicy::DevTrust;
    cfg
}

fn launcher_for(home: &Path) -> launcher::Launcher<launcher::IpcConnect, launcher::DetachedSpawn> {
    let run = run_dir(home);
    std::fs::create_dir_all(run.parent().unwrap()).unwrap();
    let own = SelfIdentity::detect().unwrap();
    launcher::for_run_dir(&run, &own, client_cfg(), daemon_exe()).unwrap()
}

/// 子进程：连接或拉起，连上就打印后台 pid 并退出。
#[test]
fn launcher_child() {
    if std::env::var_os(CHILD_ENV).is_none() {
        return;
    }
    let home = PathBuf::from(std::env::var_os("HOME").unwrap());
    let rt = tokio::runtime::Runtime::new().unwrap();
    rt.block_on(async {
        let mut l = launcher_for(&home);
        // 冷启动：本机负载高时会超过 1s，就照 T09 的做法接着调同一个 Launcher
        let deadline = Instant::now() + Duration::from_secs(60);
        loop {
            match l.connect_or_launch().await {
                Ok(_) => break,
                Err(e) if Instant::now() < deadline => {
                    eprintln!("还没连上：{e:?}");
                    tokio::time::sleep(Duration::from_millis(200)).await;
                }
                Err(e) => panic!("连不上：{e:?}"),
            }
        }
    });
    let lock = rendezvous::read_lock_info(&run_dir(&home)).unwrap();
    println!("CONNECTED {}", lock.pid);
}

struct Sandbox {
    dir: tempfile::TempDir,
    daemon_pid: Option<u32>,
}

impl Drop for Sandbox {
    fn drop(&mut self) {
        if let Some(pid) = self.daemon_pid {
            // SAFETY：只结束本测试拉起的后台
            unsafe {
                libc::kill(pid as i32, libc::SIGTERM);
            }
            let deadline = Instant::now() + Duration::from_secs(20);
            while alive(pid) && Instant::now() < deadline {
                std::thread::sleep(Duration::from_millis(50));
            }
            if alive(pid) {
                unsafe {
                    libc::kill(pid as i32, libc::SIGKILL);
                }
            }
        }
    }
}

fn alive(pid: u32) -> bool {
    // SAFETY：信号 0 只探测
    unsafe { libc::kill(pid as i32, 0) == 0 }
}

fn sid(pid: u32) -> i32 {
    // SAFETY：只读
    unsafe { libc::getsid(pid as i32) }
}

/// 起子进程当「客户端」，它拉起后台、连上后退出；返回后台 pid。
fn launch_via_child(sb: &mut Sandbox) -> u32 {
    let home = sb.dir.path();
    let mut cmd = Command::new(std::env::current_exe().unwrap());
    cmd.args(["launcher_child", "--exact", "--nocapture"])
        .env(CHILD_ENV, "1")
        .env("HOME", home)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    for k in [
        "XDG_RUNTIME_DIR",
        "XDG_DATA_HOME",
        "XDG_CONFIG_HOME",
        "IWORK_DATA_DIR",
    ] {
        cmd.env_remove(k);
    }
    let out = cmd.output().unwrap();
    let stdout = String::from_utf8_lossy(&out.stdout);
    let pid: u32 = stdout
        .lines()
        .find_map(|l| l.strip_prefix("CONNECTED "))
        .unwrap_or_else(|| {
            panic!(
                "子进程没连上：{stdout}\n{}",
                String::from_utf8_lossy(&out.stderr)
            )
        })
        .trim()
        .parse()
        .unwrap();
    sb.daemon_pid = Some(pid);
    assert!(out.status.success());
    pid
}

fn sandbox() -> Sandbox {
    Sandbox {
        dir: tempfile::Builder::new()
            .prefix("iwl-")
            .tempdir_in("/tmp")
            .unwrap(),
        daemon_pid: None,
    }
}

/// 卡验收 2：拉起后台的客户端退出后，后台还在、就绪、不在客户端的会话里（`setsid` 脱离）。
#[test]
fn daemon_survives_launcher_exit() {
    let mut sb = sandbox();
    let pid = launch_via_child(&mut sb);
    std::thread::sleep(Duration::from_millis(500));
    assert!(alive(pid), "客户端退出后后台还在");
    assert_eq!(sid(pid), pid as i32, "后台自己是新会话的首进程（setsid）");
    assert_ne!(
        sid(pid),
        sid(std::process::id()),
        "不在测试（客户端一侧）的会话里"
    );
    let s = rendezvous::read_status(&run_dir(sb.dir.path())).unwrap();
    assert_eq!(s.pid, pid);
    assert_eq!(s.phase, Phase::Ready);
}

/// 卡验收 2：后台在跑时，客户端重开（新的 Launcher）1s 内连上，且不拉起新的后台。
#[test]
fn reconnect_within_1s() {
    let mut sb = sandbox();
    let pid = launch_via_child(&mut sb);
    let home = sb.dir.path().to_path_buf();
    let rt = tokio::runtime::Runtime::new().unwrap();
    let elapsed = rt.block_on(async {
        let mut l = launcher_for(&home);
        let t = Instant::now();
        l.connect_or_launch().await.expect("后台在跑，应直接连上");
        t.elapsed()
    });
    eprintln!("后台在跑时重连耗时 {elapsed:?}");
    assert!(elapsed < Duration::from_secs(1), "{elapsed:?}");
    assert_eq!(
        rendezvous::read_lock_info(&run_dir(&home)).unwrap().pid,
        pid,
        "没有拉起新的后台"
    );
}
