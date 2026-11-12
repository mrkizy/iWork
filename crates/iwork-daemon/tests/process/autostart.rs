//! 登录自启（方案 2.12；S11a）。`launchctl` / `systemctl` 换成替身（`IWORK_DEV_AUTOSTART_TOOL`）：记下参数；
//! 有 `kill-on-unregister` 文件时，对 `bootout` / `disable` 给调用它的进程（后台）发 SIGTERM——真的服务管理器
//! 注销时就是这样结束它名下的实例（N1 第 7 项、N4）。

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use iwork_ipc::rendezvous::Phase;
use iwork_ports::read_model::ReadModelPort;
use iwork_proto::pb;

use crate::fixtures::{block, read_model};
use crate::seed;
use crate::support::{DAEMON, Sandbox, code, group_alive, log_text, rt, signal};

const START: Duration = Duration::from_secs(30);

struct Tool {
    dir: PathBuf,
}

impl Tool {
    fn new(sb: &Sandbox) -> Tool {
        let dir = sb.home.join("tool");
        std::fs::create_dir_all(&dir).unwrap();
        let script = format!(
            "#!/bin/sh\necho \"$@\" >> '{log}'\nfor a in \"$@\"; do\n  case \"$a\" in bootout|disable) [ -f '{flag}' ] && kill -TERM $PPID ;; esac\ndone\nexit 0\n",
            log = dir.join("calls.log").display(),
            flag = dir.join("kill-on-unregister").display(),
        );
        crate::support::write_script(&dir.join("tool"), &script);
        Tool { dir }
    }

    fn env(&self) -> String {
        self.dir.join("tool").display().to_string()
    }

    fn calls(&self) -> Vec<String> {
        std::fs::read_to_string(self.dir.join("calls.log"))
            .unwrap_or_default()
            .lines()
            .map(str::to_owned)
            .collect()
    }

    fn unregisters(&self) -> usize {
        self.calls()
            .iter()
            .filter(|c| c.contains("bootout") || c.contains("disable"))
            .count()
    }

    fn kill_on_unregister(&self) {
        std::fs::write(self.dir.join("kill-on-unregister"), b"").unwrap();
    }
}

/// 注册项文件（plist / systemd 单元）的路径。
fn registration(sb: &Sandbox) -> PathBuf {
    if cfg!(target_os = "macos") {
        sb.home.join("Library/LaunchAgents/com.iwork.daemon.plist")
    } else {
        sb.home.join(".config/systemd/user/iwork-daemon.service")
    }
}

fn write_registration(sb: &Sandbox, program: &Path) {
    let text = if cfg!(target_os = "macos") {
        iwork_host::autostart::macos::plist_text(program)
    } else {
        iwork_host::autostart::linux::unit_text(program)
    };
    let p = registration(sb);
    std::fs::create_dir_all(p.parent().unwrap()).unwrap();
    std::fs::write(p, text).unwrap();
}

fn set_autostart(sb: &Sandbox, enabled: bool) -> pb::Response {
    rt().block_on(async {
        let (client, _incoming) = sb.connect().await.expect("握手");
        client
            .request(pb::request::Body::Command(pb::Command {
                command_id: ulid::Ulid::generate().to_string(),
                kind: Some(pb::command::Kind::SetAutostart(pb::SetAutostart {
                    enabled,
                })),
            }))
            .await
            .expect("请求")
    })
}

fn wait_until(what: &str, limit: Duration, f: impl Fn() -> bool) {
    let deadline = Instant::now() + limit;
    while !f() {
        assert!(Instant::now() < deadline, "{limit:?} 内没等到：{what}");
        std::thread::sleep(Duration::from_millis(50));
    }
}

fn committed_intent(sb: &Sandbox) -> bool {
    let store = seed::open(sb);
    let v = block(read_model(&store).settings()).unwrap().autostart;
    store.close().unwrap();
    v
}

/// S11a：服务管理器起的后台（托管实例）上关自启 → 受理成功；进程还在、回合没变；运行中一次都不注销；
/// 设置已提交（停掉后读库是「关」）；日志写明注册项留到下次启动时移除。
#[test]
fn managed_disable_keeps_running_and_persists() {
    let sb = Sandbox::new();
    let tool = Tool::new(&sb);
    // 先用非托管实例把自启打开（对齐任务注册一次）
    let mut a = sb.spawn(&[], &[("IWORK_DEV_AUTOSTART_TOOL", &tool.env())]);
    sb.wait_ready(a.pid(), START);
    let r = set_autostart(&sb, true);
    assert!(
        matches!(r.body, Some(pb::response::Body::Accepted(_))),
        "{r:?}"
    );
    wait_until("注册", START, || {
        registration(&sb).exists() && !tool.calls().is_empty()
    });
    signal(a.pid(), libc::SIGTERM);
    assert_eq!(code(a.wait_exit(START)), Some(0));
    let registered_calls = tool.calls().len();

    // 托管实例：有回合在跑
    let mut b = sb.spawn(
        &["--service"],
        &[
            ("IWORK_DEV_AUTOSTART_TOOL", &tool.env()),
            ("IWORK_DEV_WORK", "turns=1"),
        ],
    );
    sb.wait_ready(b.pid(), START);
    let pgids = b.dev_pgids();
    let r = set_autostart(&sb, false);
    assert!(
        matches!(r.body, Some(pb::response::Body::Accepted(_))),
        "{r:?}"
    );
    wait_until("对齐任务读到关", START, || {
        log_text(&sb).contains("注册项留到本进程下次启动时移除")
    });
    std::thread::sleep(Duration::from_millis(500));
    assert!(b.try_exit().is_none(), "进程还在");
    assert!(pgids.iter().all(|g| group_alive(*g)), "回合没被打断");
    assert_eq!(
        tool.unregisters(),
        0,
        "运行中不注销自己：{:?}",
        tool.calls()
    );
    assert_eq!(tool.calls().len(), registered_calls);
    assert!(registration(&sb).exists());
    signal(b.pid(), libc::SIGTERM);
    assert_eq!(code(b.wait_exit(START)), Some(0));
    assert!(!committed_intent(&sb), "设置已提交为关");
}

/// S11a：托管实例启动时，库里的设置是关而它还注册着 → 在 bind 之前注销恰好一次；服务管理器随即结束它 →
/// socket 文件从没出现过，状态停在 `Starting{autostart-off}`。
#[test]
fn managed_boot_with_intent_off_unregisters_before_bind() {
    let sb = Sandbox::new();
    let tool = Tool::new(&sb);
    tool.kill_on_unregister();
    write_registration(&sb, Path::new(DAEMON));
    let mut d = sb.spawn(&["--service"], &[("IWORK_DEV_AUTOSTART_TOOL", &tool.env())]);
    let status = d.wait_exit(START);
    assert_eq!(
        code(status),
        None,
        "被服务管理器（替身）用 SIGTERM 结束：{}",
        d.stderr()
    );
    assert_eq!(tool.unregisters(), 1, "{:?}", tool.calls());
    assert!(!sb.run_dir().join("d.sock").exists(), "socket 从没出现过");
    assert_eq!(
        sb.status().unwrap().phase,
        Phase::Starting {
            step: "autostart-off".into()
        }
    );
}

/// 对照：非托管实例启动时不做 9a（设置是关、注册着也不注销），由对齐任务在运行中注销（它不是被注销的那个实例）。
#[test]
fn unmanaged_instance_unregisters_while_running() {
    let sb = Sandbox::new();
    let tool = Tool::new(&sb);
    write_registration(&sb, Path::new(DAEMON));
    let d = sb.spawn(&[], &[("IWORK_DEV_AUTOSTART_TOOL", &tool.env())]);
    sb.wait_ready(d.pid(), START);
    wait_until("对齐任务注销", START, || !registration(&sb).exists());
    assert_eq!(tool.unregisters(), 1, "{:?}", tool.calls());
}
