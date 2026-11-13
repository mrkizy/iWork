//! 客户端日志（F-1；T03 方案 2.12「两端都记」）：真进程里客户端拒绝身份不符的后台时，`client.log` 里有
//! `peer_rejected(role=client)`，启动时的清单审计也在；`IWORK_LOG=off` 也挡不住审计。
//!
//! 子进程就是本测试二进制自己（带 [`CHILD_ENV`] 只跑 `rejecting_child`）：照 `client/src-tauri/src/main.rs` 的
//! 顺序先 `logging::init()`，再用桌面客户端的真拨号器（`AppDialer<DesktopParts>`，`dev` 构建下是只连不拉的
//! `DirectDialer`）连当前用户的默认端点。占着端点的是父进程（测试二进制，不在任何安装清单里），客户端判定它不可信。
//! Tauri 壳本身要开窗口，不在这里起；壳里「先装日志、后拨号」的顺序由 main.rs 第一行保证。
#![cfg(unix)]

use std::os::unix::net::UnixListener;
use std::path::Path;
use std::process::{Command, Stdio};

use iwork_client_core::{AppDialer, DesktopParts, Dialer};
use iwork_ipc::transport::Endpoint;

const CHILD_ENV: &str = "IWORK_CLIENT_LOG_CHILD";

/// 子进程：装日志、拨一次号、打印结果后退出（日志在退出前排空）。
#[test]
fn rejecting_child() {
    if std::env::var_os(CHILD_ENV).is_none() {
        return;
    }
    let _log = iwork_client_core::logging::init();
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let result = rt.block_on(async {
        let mut dialer = AppDialer::new(DesktopParts::new("test"));
        dialer.dial().await.map(|_| ())
    });
    println!("DIAL {result:?}");
}

/// 父进程占着客户端的默认端点（接受连接、什么都不发），起子进程当客户端，读它写下的 `client.log`。
fn run_client_against_untrusted_daemon(home: &Path, data: &Path) -> (String, String) {
    let run = iwork_ipc::endpoint::unix_run_dir(|_| None, Some(home.to_path_buf())).unwrap();
    let ep = iwork_ipc::LocalEndpoint::in_run_dir(&run).unwrap();
    let Endpoint::Unix(socket) = ep.endpoint().clone() else {
        panic!("Unix 上端点是 socket");
    };
    let listener = UnixListener::bind(&socket).unwrap();
    std::thread::spawn(move || {
        let mut held = Vec::new();
        for conn in listener.incoming() {
            held.push(conn);
        }
    });
    let mut cmd = Command::new(std::env::current_exe().unwrap());
    cmd.args(["rejecting_child", "--exact", "--nocapture"])
        .env(CHILD_ENV, "1")
        .env("HOME", home)
        .env("IWORK_DATA_DIR", data)
        // 级别全关：审计照样要记
        .env("IWORK_LOG", "off")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    for k in ["XDG_RUNTIME_DIR", "XDG_DATA_HOME", "IWORK_DEV_ENDPOINT"] {
        cmd.env_remove(k);
    }
    let out = cmd.output().unwrap();
    let stdout = String::from_utf8_lossy(&out.stdout).into_owned();
    assert!(
        out.status.success(),
        "{stdout}\n{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let log = std::fs::read_to_string(data.join("logs/client.log")).unwrap_or_default();
    (stdout, log)
}

#[test]
fn client_log_records_peer_rejected_by_client() {
    let tmp = tempfile::Builder::new()
        .prefix("iwc-")
        .tempdir_in("/tmp")
        .unwrap();
    let home = tmp.path().join("home");
    let data = tmp.path().join("data");
    std::fs::create_dir_all(&home).unwrap();
    let (stdout, log) = run_client_against_untrusted_daemon(&home, &data);
    let dial = stdout
        .lines()
        .find(|l| l.starts_with("DIAL "))
        .unwrap_or_else(|| panic!("子进程没拨号：{stdout}"));
    assert!(dial.contains("PeerRejected"), "对照：拨号被拒：{dial}");

    let lines: Vec<serde_json::Value> = log
        .lines()
        .map(|l| serde_json::from_str(l).unwrap_or_else(|e| panic!("不是 JSON 行（{e}）：{l}")))
        .collect();
    let audits: Vec<&serde_json::Value> = lines
        .iter()
        .filter(|v| v["target"] == "iwork.ipc.audit")
        .map(|v| &v["fields"])
        .collect();
    let rejected = audits
        .iter()
        .find(|f| f["event"] == "peer_rejected")
        .unwrap_or_else(|| panic!("client.log 里没有 peer_rejected：\n{log}"));
    assert_eq!(rejected["role"], "client", "{log}");
    assert_eq!(
        rejected["pid"],
        std::process::id(),
        "被拒的是占着端点的父进程：{log}"
    );
    assert!(
        rejected["reason"].as_str().is_some_and(|r| !r.is_empty()),
        "{log}"
    );
    assert!(
        audits
            .iter()
            .any(|f| f["event"] == "manifest_unavailable" || f["event"] == "manifest_loaded"),
        "启动时的清单审计也在客户端日志里：\n{log}"
    );
    // 对照：IWORK_LOG=off 生效，审计以外一行都没有
    assert_eq!(audits.len(), lines.len(), "{log}");
}
