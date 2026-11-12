//! 真的 hook 程序对着真的桥接服务端（T06 方案 2.4、4.4、4.6，验收 3 与 3a）。
//!
//! 4.6 各行在哪测：
//!
//! | 行 | 测试 |
//! | --- | --- |
//! | 1 环境变量缺 | hook 一侧：`iwork-claude-hook/tests/hook_process.rs::missing_env_denies` |
//! | 2 socket 不存在 | 同上 `no_socket_denies` |
//! | 3 对端进程号不是桥接 | `wrong_peer_pid_denies` |
//! | 4 令牌错或缺 | `bad_token_denies`、`missing_token_denies`、`header_pid_must_be_the_peer` |
//! | 5 别的用户连进来 | `other_user_rejected`（只 Linux，要另一个系统用户，CI 里跑） |
//! | 6 路径启动前已被占 | `preexisting_path_aborts` |
//! | 7 桥接等待时没了 | `bridge_dies_mid_wait_denies` |
//! | 8 答复不是合法帧 | 桥接自己不会写坏帧；hook 一侧 `hook_process.rs::garbage_response_denies` |
//! | 9 改写后的输入解析不了 | `unparseable_rewrite_denies` |
//! | 10 工具输入超过 4 MiB | hook 一侧 `input_too_large_denies_without_connecting`；桥接一侧 `oversize_body_closes_without_event` |
//! | 11 后台答拒绝 | `backend_reject_is_relayed`（17 行「工具未启用」是同一条路，只是原因不同） |
//! | 12 后台答不认识的 | `unrecognized_answer_denies` |
//! | 13 桥接兜底到时 | `backstop_denies_and_reports`、`stalled_write_still_hits_the_backstop`（写卡住也算期限） |
//! | 14 hook 自己到期 | `hook_deadline_first_makes_answer_late`、`hook_gone_before_answer_is_reported` |
//! | 15 hook panic、16 stdout 写不出去 | hook 一侧 `panic_denies.rs`、`hook_process.rs::stdout_write_failure_exits_2` |
//!
//! 4.4 的「没送到一律报失败」：`eof_and_allow_ready_together_reports_undelivered_allow`；先看到断开、信箱里已有
//! 允许的那一种交错在 `hook_server` 的单测 `hook_gone_with_allow_already_in_the_mailbox`。
//!
//! 另有：回合结束时还在等的全部拒绝、头没在时限内读完、令牌不进日志、对端凭据与 `iwork-ipc` 对拍。
#![cfg(unix)]

mod support;

use std::time::Duration;

use iwork_bridge_claude::hook_server::pending::{BackendAnswer, Delivery};
use iwork_bridge_claude::hook_server::{
    HEADER_READ_LIMIT, HookEvent, HookServer, REASON_TOKEN_REJECTED, Token,
};
use iwork_claude_hook::client::{ENV_BRIDGE_PID, ENV_DEADLINE, ENV_SOCK, ENV_TOKEN, Failure};
use iwork_claude_hook::wire::{self, Decision};
use serde_json::{Value, json};
use support::channel::*;

fn allow() -> BackendAnswer {
    BackendAnswer::Allow {
        updated_input: None,
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn allow_round_trip_carries_hook_input() {
    let mut b = Bridge::start(Duration::from_secs(60));
    let hook = spawn_hook("pre-tool-use", &b.env(), INPUT);
    let (id, input) = b.next_call().await;
    assert_eq!(input, INPUT.as_bytes(), "stdin 原文交给会话一侧");
    assert_eq!(b.pending.answer(id, allow()), Delivery::Delivered);
    let ran = hook.await.unwrap();
    assert_eq!(ran.code, Some(0));
    assert_eq!(ran.decision().0, "allow");
    assert_eq!(b.pending.waiting(), 0);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn rewrite_reaches_hook() {
    let mut b = Bridge::start(Duration::from_secs(60));
    let hook = spawn_hook("pre-tool-use", &b.env(), INPUT);
    let (id, _) = b.next_call().await;
    b.pending.answer(
        id,
        BackendAnswer::Allow {
            updated_input: Some(r#"{"command":"true"}"#.into()),
        },
    );
    let ran = hook.await.unwrap();
    let v: Value = serde_json::from_str(ran.stdout.trim()).unwrap();
    assert_eq!(v["hookSpecificOutput"]["permissionDecision"], "allow");
    assert_eq!(
        v["hookSpecificOutput"]["updatedInput"],
        json!({"command":"true"})
    );
}

/// 第 11 行（第 17 行同一条路）：后台的拒绝原因原样交给模型。
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn backend_reject_is_relayed() {
    let mut b = Bridge::start(Duration::from_secs(60));
    let hook = spawn_hook("pre-tool-use", &b.env(), INPUT);
    let (id, _) = b.next_call().await;
    b.pending.answer(
        id,
        BackendAnswer::Reject {
            message: "工具未启用".into(),
        },
    );
    assert_eq!(hook.await.unwrap().denied(), "工具未启用");
}

/// 第 12 行。
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn unrecognized_answer_denies() {
    let mut b = Bridge::start(Duration::from_secs(60));
    let hook = spawn_hook("pre-tool-use", &b.env(), INPUT);
    let (id, _) = b.next_call().await;
    b.pending.answer(id, BackendAnswer::Unrecognized);
    assert_eq!(hook.await.unwrap().denied(), Failure::BadResponse.reason());
}

/// 第 9 行。
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn unparseable_rewrite_denies() {
    let mut b = Bridge::start(Duration::from_secs(60));
    let hook = spawn_hook("pre-tool-use", &b.env(), INPUT);
    let (id, _) = b.next_call().await;
    b.pending.answer(
        id,
        BackendAnswer::Allow {
            updated_input: Some("rm -rf /".into()),
        },
    );
    assert_eq!(hook.await.unwrap().denied(), Failure::BadResponse.reason());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn session_start_reports() {
    let mut b = Bridge::start(Duration::from_secs(60));
    let hook = spawn_hook(
        "session-start",
        &b.env(),
        r#"{"hook_event_name":"SessionStart"}"#,
    );
    assert!(matches!(b.next_event().await, HookEvent::SessionStart));
    let ran = hook.await.unwrap();
    assert_eq!(ran.code, Some(0));
    assert_eq!(b.pending.waiting(), 0, "报到不登记等待项");
}

/// 第 6 行：路径已经有东西（哪怕是普通文件）就绑定失败，原样不动。
#[tokio::test]
async fn preexisting_path_aborts() {
    let dir = tempfile::tempdir().unwrap();
    let sock = dir.path().join("s");
    std::fs::write(&sock, b"someone else's").unwrap();
    let err = HookServer::bind(&sock, Token::from_bytes(TOKEN), Duration::from_secs(60))
        .err()
        .expect("绑定失败");
    assert!(
        err.0.contains(&sock.display().to_string()),
        "点名路径：{err}"
    );
    assert_eq!(
        std::fs::read(&sock).unwrap(),
        b"someone else's",
        "不删、不改"
    );
}

/// 第 3 行：hook 发现对端进程号不是给的桥接，什么都不发就拒绝（令牌不交出去）。
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn wrong_peer_pid_denies() {
    let mut b = Bridge::start(Duration::from_secs(60));
    let env = with(&b.env(), ENV_BRIDGE_PID, "1");
    let ran = spawn_hook("pre-tool-use", &env, INPUT).await.unwrap();
    assert_eq!(ran.denied(), Failure::PeerMismatch.reason());
    assert!(b.no_event().await, "桥接这边没收到请求");
}

/// 第 4 行：令牌不对，桥接答拒绝、不登记。
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn bad_token_denies() {
    let mut b = Bridge::start(Duration::from_secs(60));
    let env = with(&b.env(), ENV_TOKEN, &"ab".repeat(32));
    let ran = spawn_hook("pre-tool-use", &env, INPUT).await.unwrap();
    assert_eq!(ran.denied(), REASON_TOKEN_REJECTED);
    assert!(b.no_event().await);
    assert_eq!(b.pending.waiting(), 0);
    // 报到也一样要令牌
    let env = with(&b.env(), ENV_TOKEN, &"ab".repeat(32));
    spawn_hook("session-start", &env, "{}").await.unwrap();
    assert!(b.no_event().await, "令牌不对的报到不算数");
}

/// 第 4 行：令牌缺（空串）同样拒绝。
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn missing_token_denies() {
    let mut b = Bridge::start(Duration::from_secs(60));
    let sock = b.sock.clone();
    let resp = tokio::task::spawn_blocking(move || {
        raw_request(&sock, &header(String::new(), std::process::id()), b"{}")
    })
    .await
    .unwrap()
    .expect("有答复");
    assert_eq!(resp.decision, Decision::Deny);
    assert_eq!(resp.message, REASON_TOKEN_REJECTED);
    assert!(b.no_event().await);
}

/// 令牌对，但请求头里的进程号不是连接的对端：同样拒绝。
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn header_pid_must_be_the_peer() {
    let mut b = Bridge::start(Duration::from_secs(60));
    let sock = b.sock.clone();
    let resp = tokio::task::spawn_blocking(move || {
        raw_request(&sock, &header(wire::token_hex(&TOKEN), 1), b"{}")
    })
    .await
    .unwrap()
    .expect("有答复");
    assert_eq!(resp.decision, Decision::Deny);
    assert!(b.no_event().await);
}

/// 第 10 行的桥接一侧：正文长度超上限，不读、不登记、关连接。
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn oversize_body_closes_without_event() {
    let mut b = Bridge::start(Duration::from_secs(60));
    let sock = b.sock.clone();
    let got = tokio::task::spawn_blocking(move || {
        use std::io::Write;
        let mut conn = std::os::unix::net::UnixStream::connect(&sock).unwrap();
        conn.set_read_timeout(Some(Duration::from_secs(10)))
            .unwrap();
        let h = serde_json::to_vec(&header(wire::token_hex(&TOKEN), std::process::id())).unwrap();
        let mut bytes = wire::MAGIC.to_vec();
        bytes.extend_from_slice(&(h.len() as u32).to_le_bytes());
        bytes.extend_from_slice(&h);
        bytes.extend_from_slice(&(wire::MAX_BODY_BYTES + 1).to_le_bytes());
        conn.write_all(&bytes).unwrap();
        wire::read_response(&mut conn).ok()
    })
    .await
    .unwrap();
    assert!(got.is_none(), "没有答复，连接被关");
    assert!(b.no_event().await);
}

/// 第 7 行：桥接在 hook 等答复时没了，hook 读到 EOF，按拒绝输出。
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn bridge_dies_mid_wait_denies() {
    let mut b = Bridge::start(Duration::from_secs(60));
    let hook = spawn_hook("pre-tool-use", &b.env(), INPUT);
    b.next_call().await;
    b.server.abort();
    assert_eq!(hook.await.unwrap().denied(), Failure::Disconnected.reason());
}

/// 第 13 行：后台在兜底期限内没答，桥接答拒绝并报出来；之后才到的允许是迟到的允许（调用方按兜底处理）。
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn backstop_denies_and_reports() {
    let mut b = Bridge::start(Duration::from_secs(1));
    let hook = spawn_hook("pre-tool-use", &b.env(), INPUT);
    let (id, _) = b.next_call().await;
    assert_eq!(hook.await.unwrap().denied(), Failure::Deadline.reason());
    assert!(matches!(b.next_event().await, HookEvent::Backstop { id: got } if got == id));
    assert_eq!(
        b.pending.answer(id, allow()),
        Delivery::Late { was_allow: true }
    );
}

/// 第 14 行：hook 自己先到期（桥接卡住时的最后一道），桥接看到连接断了；后台的允许随后到，是迟到的允许。
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn hook_deadline_first_makes_answer_late() {
    let mut b = Bridge::start(Duration::from_secs(60));
    let env = with(&b.env(), ENV_DEADLINE, "1");
    let hook = spawn_hook("pre-tool-use", &env, INPUT);
    let (id, _) = b.next_call().await;
    assert_eq!(hook.await.unwrap().denied(), Failure::Deadline.reason());
    assert!(
        matches!(b.next_event().await, HookEvent::HookLost { id: got } if got == id),
        "桥接看到 hook 走了，报会话失败"
    );
    assert_eq!(b.pending.waiting(), 0);
    assert_eq!(
        b.pending.answer(id, allow()),
        Delivery::Late { was_allow: true }
    );
}

/// 发完请求的原始客户端（不经 hook 程序）：返回连接本身，由测试决定什么时候关、读不读。
fn sent_request(sock: &std::path::Path) -> std::os::unix::net::UnixStream {
    let mut conn = std::os::unix::net::UnixStream::connect(sock).unwrap();
    wire::write_request(
        &mut conn,
        &header(wire::token_hex(&TOKEN), std::process::id()),
        INPUT.as_bytes(),
    )
    .unwrap();
    conn
}

/// 审查 1/F-1：hook 在答复之前断开（被 CLI 杀掉、自己到期），桥接马上报会话失败，不等以后的后台答复。
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn hook_gone_before_answer_is_reported() {
    let mut b = Bridge::start(Duration::from_secs(60));
    let conn = sent_request(&b.sock);
    let (id, _) = b.next_call().await;
    drop(conn);
    assert!(matches!(b.next_event().await, HookEvent::HookLost { id: got } if got == id));
    assert_eq!(b.pending.waiting(), 0);
}

/// 审查 1/F-1：后台的允许已经交进连接任务的信箱、hook 也已经断开，两样同时就绪。不管先处理哪一样，
/// 这条允许都没送到，要报 `AllowUndelivered`。单线程运行时：关连接与答复都在服务端任务被轮询之前做完。
#[tokio::test(flavor = "current_thread")]
async fn eof_and_allow_ready_together_reports_undelivered_allow() {
    let mut b = Bridge::start(Duration::from_secs(60));
    let conn = sent_request(&b.sock);
    let (id, _) = b.next_call().await;
    drop(conn);
    assert_eq!(b.pending.answer(id, allow()), Delivery::Delivered);
    assert!(matches!(b.next_event().await, HookEvent::AllowUndelivered { id: got } if got == id));
}

/// 审查 1/F-2：后台答了一条很大的允许，hook 不读答复：写卡住时兜底期限照样算，到期关连接并报出来，
/// 不因为后台答过了就撤掉期限。
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn stalled_write_still_hits_the_backstop() {
    let mut b = Bridge::start(Duration::from_secs(1));
    let _conn = sent_request(&b.sock);
    let (id, _) = b.next_call().await;
    let big = json!({ "command": "x".repeat(2 * 1024 * 1024) }).to_string();
    assert_eq!(
        b.pending.answer(
            id,
            BackendAnswer::Allow {
                updated_input: Some(big)
            }
        ),
        Delivery::Delivered
    );
    assert!(matches!(b.next_event().await, HookEvent::AllowUndelivered { id: got } if got == id));
}

/// 回合结束（取消、CLI 退出）：还在等的全部答拒绝（4.4）。
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn end_of_turn_denies_every_waiting_call() {
    let mut b = Bridge::start(Duration::from_secs(60));
    let first = spawn_hook("pre-tool-use", &b.env(), INPUT);
    let second = spawn_hook("pre-tool-use", &b.env(), INPUT);
    b.next_call().await;
    b.next_call().await;
    assert_eq!(b.pending.deny_all("回合已结束，权限请求作废"), 2);
    assert_eq!(first.await.unwrap().denied(), "回合已结束，权限请求作废");
    assert_eq!(second.await.unwrap().denied(), "回合已结束，权限请求作废");
}

/// 2.4 第 4 步：连上不发头，时限一到桥接关连接，不登记。
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn silent_connection_is_closed_after_header_limit() {
    let mut b = Bridge::start(Duration::from_secs(60));
    let sock = b.sock.clone();
    let closed_after = tokio::task::spawn_blocking(move || {
        use std::io::Read;
        let started = std::time::Instant::now();
        let mut conn = std::os::unix::net::UnixStream::connect(&sock).unwrap();
        conn.set_read_timeout(Some(HEADER_READ_LIMIT * 3)).unwrap();
        let n = conn.read(&mut [0u8; 1]).expect("连接被关而不是读超时");
        assert_eq!(n, 0);
        started.elapsed()
    })
    .await
    .unwrap();
    assert!(closed_after >= HEADER_READ_LIMIT, "{closed_after:?}");
    assert!(b.no_event().await);
}

/// 方案 2.1：同一条连接上，hook crate 的 `peer_cred` 与 `iwork-ipc` 的 `Connection::peer()` 取到同一个
/// 对端（另一个进程：真的 hook）。
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn peer_cred_agrees_with_ipc() {
    use iwork_ipc::LocalListener;
    use iwork_ipc::peer::UserId;
    use iwork_ipc::transport::{Connection, Listener};

    let dir = tempfile::tempdir().unwrap();
    let sock = dir.path().join("s");
    let mut listener = LocalListener::bind(&sock).unwrap();
    let env = vec![
        (ENV_SOCK, sock.display().to_string()),
        (ENV_TOKEN, wire::token_hex(&TOKEN)),
        (ENV_BRIDGE_PID, std::process::id().to_string()),
        (ENV_DEADLINE, "60".into()),
    ];
    let hook = spawn_hook("pre-tool-use", &env, INPUT);
    let conn = tokio::time::timeout(Duration::from_secs(10), listener.accept())
        .await
        .unwrap()
        .unwrap();
    let ipc = conn.peer().await.expect("iwork-ipc 取得到对端");
    let (read, write) = conn.into_split();
    let ours = iwork_claude_hook::peer::peer_cred(read.as_ref()).unwrap();
    assert_eq!(ipc.pid, ours.pid);
    assert_eq!(ipc.user, UserId::Uid(ours.user));
    assert_ne!(ours.pid, std::process::id(), "对端是另一个进程");
    drop((read, write));
    // 连接断了，hook 按拒绝收场
    hook.await.unwrap().denied();
}

/// 第 5 行：另一个系统用户连进来，桥接不读不写直接关连接（只 Linux）。
///
/// 需要另一个用户（CI 里 `sudo useradd -m iwork-other`）与 `IWORK_TEST_OTHER_USER=1`；`IWORK_RUN_AS` 改「以另一用户
/// 运行」的命令前缀（默认 `sudo -n -u iwork-other`）。另一个用户跑的是这个测试程序自己的一份副本（放进 /tmp，
/// 它进不了仓库目录），由 [`other_user_client`] 这条测试扮客户端。socket 放在 0777 的临时目录里，绕过目录权限
/// 这道门，测的是 accept 之后的用户判定本身。
#[cfg(target_os = "linux")]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "要另一个系统用户，CI 的 Linux job 里跑"]
async fn other_user_rejected() {
    use std::os::unix::fs::PermissionsExt;
    assert_eq!(
        std::env::var("IWORK_TEST_OTHER_USER").as_deref(),
        Ok("1"),
        "要先建好另一个用户再跑"
    );
    let open = tempfile::Builder::new()
        .prefix("iwork-hook-other")
        .tempdir_in("/tmp")
        .unwrap();
    std::fs::set_permissions(open.path(), std::fs::Permissions::from_mode(0o777)).unwrap();
    let sock = open.path().join("s");
    let (server, mut events) =
        HookServer::bind(&sock, Token::from_bytes(TOKEN), Duration::from_secs(60)).unwrap();
    std::fs::set_permissions(&sock, std::fs::Permissions::from_mode(0o777)).unwrap();
    let serving = tokio::spawn(server.serve());

    let exe = open.path().join("client");
    std::fs::copy(std::env::current_exe().unwrap(), &exe).unwrap();
    std::fs::set_permissions(&exe, std::fs::Permissions::from_mode(0o755)).unwrap();
    let prefix: Vec<String> = std::env::var("IWORK_RUN_AS")
        .unwrap_or_else(|_| "sudo -n -u iwork-other".into())
        .split_whitespace()
        .map(str::to_owned)
        .collect();
    let out = tokio::process::Command::new(&prefix[0])
        .args(&prefix[1..])
        .arg("env")
        .arg(format!("IWORK_OTHER_USER_SOCK={}", sock.display()))
        .arg(&exe)
        .args(["other_user_client", "--exact", "--ignored", "--nocapture"])
        .output()
        .await
        .unwrap();
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        out.status.success(),
        "{stdout}\n{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(stdout.contains("CLOSED_WITHOUT_REPLY"), "{stdout}");
    assert!(
        tokio::time::timeout(Duration::from_millis(300), events.recv())
            .await
            .is_err(),
        "没有任何事件"
    );
    serving.abort();
}

/// [`other_user_rejected`] 的客户端一半：以另一个用户跑，带对的令牌发请求，报告有没有答复。
#[cfg(target_os = "linux")]
#[test]
#[ignore = "只由 other_user_rejected 以另一个用户调起"]
fn other_user_client() {
    let Ok(sock) = std::env::var("IWORK_OTHER_USER_SOCK") else {
        return;
    };
    let reply = raw_request(
        std::path::Path::new(&sock),
        &header(wire::token_hex(&TOKEN), std::process::id()),
        INPUT.as_bytes(),
    );
    match reply {
        None => println!("CLOSED_WITHOUT_REPLY"),
        Some(r) => println!("GOT_REPLY {:?}", r.decision),
    }
}
