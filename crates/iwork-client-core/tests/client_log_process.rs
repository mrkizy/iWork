//! 客户端日志（F-1；T03 方案 2.12「两端都记」）：真进程里客户端拒绝身份不符的后台时，`client.log` 里有
//! `peer_rejected(role=client)`，启动时的清单审计也在；`IWORK_LOG=off` 也挡不住审计。
//!
//! 子进程就是本测试二进制自己（带 [`CHILD_ENV`] 只跑 `rejecting_child`）：照 `client/src-tauri/src/main.rs` 的
//! 顺序先 `logging::init()`，再用桌面客户端的真拨号器（`AppDialer<DesktopParts>`，`dev` 构建下是只连不拉的
//! `DirectDialer`）连当前用户的默认端点。占着端点的是父进程（测试二进制，不在任何安装清单里），客户端判定它不可信。
//! Tauri 壳本身要开窗口，不在这里起；壳里「先装日志、后拨号」的顺序由 main.rs 第一行保证。
//!
//! 另一条（审查 1/F-1）：客户端日志落盘以后，失败路径不许把用户的输入（搜索词）和后台给的说明文字写进 `client.log`。
#![cfg(unix)]

mod support;

use std::os::unix::net::UnixListener;
use std::path::Path;
use std::process::{Command, Stdio};

use iwork_client_core::{AppDialer, DesktopParts, Dialer};
use iwork_ipc::transport::Endpoint;

const CHILD_ENV: &str = "IWORK_CLIENT_LOG_CHILD";
const PRIVACY_CHILD_ENV: &str = "IWORK_CLIENT_LOG_PRIVACY_CHILD";
const HANDSHAKE_CHILD_ENV: &str = "IWORK_CLIENT_LOG_HANDSHAKE_CHILD";
/// 后台在握手时回的错误说明里带的虚构私有路径（审查 2/F-1）。
const PATH_CANARY: &str = "/Users/review/私有项目-CANARY-握手-5e2a/notes";
/// 拨号失败的那一行（对照）。
const DIAL_FAILED_LINE: &str = "连不上后台服务";
/// 虚构的搜索词：只会出现在对话列表的筛选里。
const SEARCH_CANARY: &str = "CANARY-搜索-7c1d93";
/// 假后台拒绝命令、查询时给的说明文字的开头（`iwork-fake-daemon` 里写死的）：后台的说明文字可能带路径，不许落盘。
const SERVER_MESSAGE: &str = "假后台：按测试设定";
/// 两条失败路径真的走过了的日志字样（对照）。
const QUERY_FAILED_LINE: &str = "视图查询失败";
const PRESENCE_REJECTED_LINE: &str = "报在看哪";

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

/// 子进程：装日志（`IWORK_LOG=debug`），连假后台；对话列表用虚构的搜索词筛选、且这条查询必出错；在看哪的命令一律被拒。
/// 等两条失败都进了日志才退出。
#[test]
fn privacy_child() {
    if std::env::var_os(PRIVACY_CHILD_ENV).is_none() {
        return;
    }
    use iwork_client_core::{App, DirectDialer, Options, ShellEvent};
    use iwork_fake_daemon::{FakeDaemon, scenario, self_trusting_identity};
    use iwork_proto::pb;
    use std::time::{Duration, Instant};

    let _log = iwork_client_core::logging::init();
    let log_file = std::path::PathBuf::from(std::env::var_os("IWORK_DATA_DIR").unwrap())
        .join("logs/client.log");
    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .unwrap();
    rt.block_on(async {
        let dir = tempfile::Builder::new()
            .prefix("iwp-")
            .tempdir_in("/tmp")
            .unwrap();
        let d = FakeDaemon::start(dir.path().join("d.sock")).await.unwrap();
        d.run_scenario("conversation-basic").await.unwrap();
        d.fail_queries(Some(Box::new(|q: &pb::Query| {
            matches!(&q.kind, Some(pb::query::Kind::ListConversations(l))
                if l.title_contains.as_deref() == Some(SEARCH_CANARY))
        })));
        d.fail_commands(Some(Box::new(|c: &pb::Command| {
            matches!(c.kind, Some(pb::command::Kind::ReportPresence(_)))
        })));
        let dialer = DirectDialer::new(d.endpoint(), self_trusting_identity().unwrap(), "test");
        let (app, effects) = App::start(dialer, Options::default());
        std::mem::forget(effects);
        let mut web = support::web::Web::new(app.attach().await.unwrap());
        app.intent(pb::UiIntent {
            kind: Some(pb::ui_intent::Kind::SetVisible(pb::SetVisible {
                sidebar: true,
                conversation_list: Some(pb::ConversationListQuery {
                    project_id: scenario::PROJECT.into(),
                    filter: SEARCH_CANARY.into(),
                }),
                ..Default::default()
            })),
        })
        .await;
        app.shell(ShellEvent::Focused(true));
        let deadline = Instant::now() + Duration::from_secs(20);
        loop {
            if let Ok(Ok(c)) =
                tokio::time::timeout(Duration::from_millis(300), app.next_frame(web.request()))
                    .await
            {
                web.accept(&c);
            }
            iwork_client_core::logging::flush_global();
            let log = std::fs::read_to_string(&log_file).unwrap_or_default();
            if log.contains(QUERY_FAILED_LINE) && log.contains(PRESENCE_REJECTED_LINE) {
                break;
            }
            assert!(Instant::now() < deadline, "两条失败路径没都进日志：\n{log}");
        }
    });
}

/// 审查 1/F-1：对话列表的搜索词、后台的说明文字不落盘；失败本身照样记（对照）。
#[test]
fn client_log_keeps_user_text_out() {
    let tmp = tempfile::Builder::new()
        .prefix("iwc-")
        .tempdir_in("/tmp")
        .unwrap();
    let data = tmp.path().join("data");
    let out = Command::new(std::env::current_exe().unwrap())
        .args(["privacy_child", "--exact", "--nocapture"])
        .env(PRIVACY_CHILD_ENV, "1")
        .env("IWORK_DATA_DIR", &data)
        // 连 debug 一起开：调试级别的日志也会落盘
        .env("IWORK_LOG", "debug")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "{}\n{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    let log = std::fs::read_to_string(data.join("logs/client.log")).unwrap();
    assert!(
        log.contains(QUERY_FAILED_LINE),
        "对照：查询失败记了：\n{log}"
    );
    assert!(
        log.contains(PRESENCE_REJECTED_LINE),
        "对照：命令被拒记了：\n{log}"
    );
    let leaks: Vec<&str> = log
        .lines()
        .filter(|l| l.contains(SEARCH_CANARY) || l.contains(SERVER_MESSAGE))
        .collect();
    assert!(
        leaks.is_empty(),
        "client.log 里有用户输入或后台说明文字：\n{}",
        leaks.join("\n")
    );
}

/// 子进程：起一个只会回握手错误的端点（错误说明里带虚构的私有路径），用真会话拨号（`dev` 构建 + `IWORK_DEV_ENDPOINT`：
/// 只连这个端点、走开发信任，所以能过身份判定、走到握手），等「连不上后台服务」进了日志再退出。
#[test]
fn handshake_child() {
    if std::env::var_os(HANDSHAKE_CHILD_ENV).is_none() {
        return;
    }
    use iwork_client_core::{App, AppDialer, DesktopParts, Options};
    use iwork_ipc::frame::{Frame, HEADER_LEN, encode_frame};
    use iwork_proto::pb;
    use std::io::{Read, Write};
    use std::time::{Duration, Instant};

    let _log = iwork_client_core::logging::init();
    let log_file = std::path::PathBuf::from(std::env::var_os("IWORK_DATA_DIR").unwrap())
        .join("logs/client.log");
    let sock = std::path::PathBuf::from(std::env::var_os("IWORK_DEV_ENDPOINT").unwrap());
    let listener = UnixListener::bind(&sock).unwrap();
    std::thread::spawn(move || {
        for conn in listener.incoming() {
            let Ok(mut conn) = conn else { continue };
            // 等客户端的 Hello 到了（读到帧头就够），回一个握手错误
            let mut head = [0u8; HEADER_LEN];
            if conn.read_exact(&mut head).is_err() {
                continue;
            }
            let mut buf = bytes::BytesMut::new();
            encode_frame(
                &Frame::Response(Box::new(pb::Response {
                    request_id: 0,
                    body: Some(pb::response::Body::Error(pb::Error {
                        code: pb::ErrorCode::Internal as i32,
                        message: format!("打不开 {PATH_CANARY}"),
                        retryable: true,
                        details: Default::default(),
                    })),
                })),
                &mut buf,
            );
            let _ = conn.write_all(&buf);
        }
    });
    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .unwrap();
    rt.block_on(async {
        let (_app, effects) = App::start(
            AppDialer::new(DesktopParts::new("test")),
            Options::default(),
        );
        std::mem::forget(effects);
        let deadline = Instant::now() + Duration::from_secs(20);
        loop {
            tokio::time::sleep(Duration::from_millis(100)).await;
            iwork_client_core::logging::flush_global();
            let log = std::fs::read_to_string(&log_file).unwrap_or_default();
            if log.contains(DIAL_FAILED_LINE) {
                break;
            }
            assert!(Instant::now() < deadline, "拨号失败没进日志：\n{log}");
        }
    });
}

/// 审查 2/F-1：后台在握手时回的错误说明（可能带路径）不落盘；拨号失败本身照样记（对照）。
#[test]
fn client_log_keeps_handshake_error_text_out() {
    let tmp = tempfile::Builder::new()
        .prefix("iwh-")
        .tempdir_in("/tmp")
        .unwrap();
    let data = tmp.path().join("data");
    let out = Command::new(std::env::current_exe().unwrap())
        .args(["handshake_child", "--exact", "--nocapture"])
        .env(HANDSHAKE_CHILD_ENV, "1")
        .env("IWORK_DATA_DIR", &data)
        .env("IWORK_DEV_ENDPOINT", tmp.path().join("d.sock"))
        .env("IWORK_LOG", "debug")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "{}\n{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    let log = std::fs::read_to_string(data.join("logs/client.log")).unwrap();
    assert!(
        log.contains(DIAL_FAILED_LINE),
        "对照：拨号失败记了：\n{log}"
    );
    let leaks: Vec<&str> = log.lines().filter(|l| l.contains("CANARY-握手")).collect();
    assert!(
        leaks.is_empty(),
        "client.log 里有握手错误的说明文字：\n{}",
        leaks.join("\n")
    );
}
