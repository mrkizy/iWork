//! 开会话的次序（T06 方案 3.5，15.3 的诊断一组）：真的 hook 程序、假 CLI、真的桥接开会话代码。
//!
//! | 测试 | 方案里的名字 |
//! | --- | --- |
//! | `permission_mode_checked_before_first_message` | 同名 |
//! | `settings_ignored_is_detected` | 同名（假 CLI 版；回放版在 replay.rs） |
//! | `initialize_without_mode_blocks_session` | 同名（没有这个键、等不到响应两种） |
//! | `canary_missing_blocks_session`、`canary_required_on_resume` | 同名 |
//! | `initialize_response_account_never_logged` | 在 startup_log.rs（要装全局日志订阅者） |
//!
//! 另有：CLI 拿到的环境就是计划里那一份、开会话期间 CLI 退出、hook 程序不在、失败不留会话目录；
//! 审查 1：坏行与超长行（F-1）、对不上的控制响应不留原文（F-2）、开会话被取消也不留会话目录与 CLI（F-3）。
#![cfg(unix)]

mod support;

use std::time::{Duration, SystemTime, UNIX_EPOCH};

use iwork_bridge_claude::cli::events::{LineError, MAX_LINE_BYTES};
use iwork_bridge_claude::cli::facts::IsolationBroken;
use iwork_bridge_claude::cli::input::{InputBlock, user_line};
use iwork_bridge_claude::launch::Start;
use iwork_bridge_claude::startup::{INITIALIZE_REQUEST_ID, OpenError};
use serde_json::{Value, json};
use support::startup::*;

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_millis() as u64
}

fn at(log: &[Value], kind: &str) -> u64 {
    log.iter()
        .find(|v| v["kind"] == kind)
        .unwrap_or_else(|| panic!("记录里没有 {kind}：{log:#?}"))["at_ms"]
        .as_u64()
        .unwrap()
}

/// 报到 → initialize → 核对通过 → 才返回；第一条用户消息在这之后才到 CLI。开会话期间 CLI 吐的别的行留给映射。
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn permission_mode_checked_before_first_message() {
    let w = World::new(json!([
        { "step": "session_start" },
        { "step": "emit", "line": { "type": "system", "subtype": "hook_started" } },
        { "step": "read_line" },
        { "step": "sleep_ms", "ms": 300 },
        { "step": "answer_initialize", "mode": "dontAsk" },
        { "step": "wait_stdin_eof" },
    ]));
    let mut s = w.open().await.expect("开会话成功");
    let opened_at = now_ms();
    s.cli
        .write_line(&user_line(&[InputBlock::Text("你好".into())]).unwrap())
        .await
        .unwrap();
    w.wait_log(|v| v["kind"] == "stdin" && v["line"].as_str().unwrap().contains("你好"))
        .await;

    let log = w.log();
    let lines = stdin_lines(&log);
    assert_eq!(
        lines[0],
        json!({ "type": "control_request", "request_id": INITIALIZE_REQUEST_ID, "request": { "subtype": "initialize" } }),
        "CLI 收到的第一行是 initialize"
    );
    assert_eq!(lines[1]["type"], "user");
    let stdin_at: Vec<u64> = log
        .iter()
        .filter(|v| v["kind"] == "stdin")
        .map(|v| v["at_ms"].as_u64().unwrap())
        .collect();
    assert!(
        at(&log, "session_start") <= stdin_at[0],
        "报到在 initialize 之前"
    );
    assert!(
        at(&log, "answered_initialize") <= opened_at,
        "核对之后才返回"
    );
    assert!(
        at(&log, "answered_initialize") <= stdin_at[1],
        "用户消息在核对之后"
    );
    // 按 JSON 值比：假 CLI 单独构建时 serde_json 不一定开 preserve_order，键序会不同
    let early: Vec<Value> = s
        .early_lines
        .iter()
        .map(|l| serde_json::from_str(l).unwrap())
        .collect();
    assert_eq!(
        early,
        vec![json!({ "type": "system", "subtype": "hook_started" })]
    );
    assert_eq!(w.session_dirs(), 1);
    s.cli.kill().await.unwrap();
    s.dir.remove().unwrap();
}

/// 设置文件被忽略时 CLI 报 `default`：`isolation_broken`，CLI 始终没收到用户消息，会话目录不留。
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn settings_ignored_is_detected() {
    let w = World::new(happy_script(Some("default")));
    let err = w.open().await.err().expect("开会话失败");
    assert!(
        matches!(&err, OpenError::IsolationBroken(IsolationBroken::PermissionMode { reported: Some(m) }) if m == "default"),
        "{err:?}"
    );
    assert_eq!(err.code(), "isolation_broken");
    let lines = stdin_lines(&w.log());
    assert_eq!(lines.len(), 1, "只收到了 initialize：{lines:?}");
    assert_eq!(w.session_dirs(), 0, "失败不留会话目录");
}

/// 响应里没有这个键、或时限内等不到响应：同样 `isolation_broken`，不当成通过。
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn initialize_without_mode_blocks_session() {
    let w = World::new(happy_script(None));
    let err = w.open().await.err().expect("没有这个键：失败");
    assert!(
        matches!(
            err,
            OpenError::IsolationBroken(IsolationBroken::PermissionMode { reported: None })
        ),
        "{err:?}"
    );

    let mut w = World::new(json!([
        { "step": "session_start" },
        { "step": "read_line" },
        { "step": "wait_stdin_eof" },
    ]));
    w.startup_limit = Duration::from_secs(1);
    let err = w.open().await.err().expect("等不到响应：失败");
    assert!(
        matches!(
            err,
            OpenError::IsolationBroken(IsolationBroken::PermissionMode { reported: None })
        ),
        "{err:?}"
    );
    assert_eq!(w.session_dirs(), 0);
}

/// 响应报的是错误（`subtype` 不是 `success`）：哪怕带着 `dontAsk` 也不算数。
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn initialize_error_response_blocks_session() {
    let w = World::new(json!([
        { "step": "session_start" },
        { "step": "read_line" },
        { "step": "answer_initialize", "mode": "dontAsk", "subtype": "error" },
        { "step": "wait_stdin_eof" },
    ]));
    let err = w.open().await.err().expect("失败");
    assert!(
        matches!(
            err,
            OpenError::IsolationBroken(IsolationBroken::PermissionMode { reported: None })
        ),
        "{err:?}"
    );
}

/// 报到没来：`hook_inactive`，连 initialize 都不发。
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn canary_missing_blocks_session() {
    let mut w = World::new(json!([{ "step": "wait_stdin_eof" }]));
    w.startup_limit = Duration::from_secs(1);
    let err = w.open().await.err().expect("失败");
    assert!(matches!(err, OpenError::HookInactive), "{err:?}");
    assert_eq!(err.code(), "hook_inactive");
    assert!(stdin_lines(&w.log()).is_empty(), "CLI 什么都没收到");
    assert_eq!(w.session_dirs(), 0);
}

/// 续接（`--resume`）同样要报到。
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn canary_required_on_resume() {
    let mut w = World::new(json!([{ "step": "wait_stdin_eof" }]));
    w.start = Start::Resume {
        session_id: NEW_SESSION.into(),
    };
    w.startup_limit = Duration::from_secs(1);
    assert!(matches!(w.open().await, Err(OpenError::HookInactive)));
    let args: Vec<String> =
        serde_json::from_str(&std::fs::read_to_string(w.record.path().join("args.json")).unwrap())
            .unwrap();
    assert!(
        args.windows(2)
            .any(|p| p[0] == "--resume" && p[1] == NEW_SESSION),
        "{args:?}"
    );

    // 对照：报到了的续接能开
    let mut w = World::new(happy_script(Some("dontAsk")));
    w.start = Start::Resume {
        session_id: NEW_SESSION.into(),
    };
    let mut s = w.open().await.expect("续接能开");
    s.cli.kill().await.unwrap();
    s.dir.remove().unwrap();
}

/// CLI 拿到的环境就是计划里那一份（3.1a）：桥接给的原样都在，记忆开关与四个 hook 变量设上了，
/// 测试进程自己的别的变量（如 HOME）一个都没带过去。
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cli_gets_exactly_the_launch_env() {
    let w = World::new(happy_script(Some("dontAsk")));
    let mut s = w.open().await.expect("开会话成功");
    let env: serde_json::Map<String, Value> =
        serde_json::from_str(&std::fs::read_to_string(w.record.path().join("env.json")).unwrap())
            .unwrap();
    let mut names: Vec<&str> = env.keys().map(String::as_str).collect();
    names.sort_unstable();
    let mut want = vec![
        "CLAUDE_CODE_DISABLE_AUTO_MEMORY",
        "IWORK_FAKE_CLAUDE_RECORD",
        "IWORK_FAKE_CLAUDE_SCRIPT",
        "IWORK_HOOK_BRIDGE_PID",
        "IWORK_HOOK_DEADLINE_SECS",
        "IWORK_HOOK_SOCK",
        "IWORK_HOOK_TOKEN",
        "IWORK_TEST_PASSTHROUGH",
        "PATH",
    ];
    want.sort_unstable();
    // macOS 上 exec 时系统会补一个 __CF_USER_TEXT_ENCODING，不是桥接给的
    names.retain(|n| *n != "__CF_USER_TEXT_ENCODING");
    assert_eq!(names, want);
    assert_eq!(env["CLAUDE_CODE_DISABLE_AUTO_MEMORY"], "1");
    assert_eq!(env["IWORK_TEST_PASSTHROUGH"], "kept");
    assert_eq!(env["IWORK_HOOK_BRIDGE_PID"], std::process::id().to_string());
    s.cli.kill().await.unwrap();
    s.dir.remove().unwrap();
}

/// 开会话期间 CLI 自己退出：报 `cli_exited`，带退出码，不留会话目录。
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cli_exit_during_startup_fails_open() {
    let w = World::new(json!([{ "step": "exit", "code": 3 }]));
    let err = w.open().await.err().expect("失败");
    assert!(
        matches!(err, OpenError::CliExited { code: Some(3) }),
        "{err:?}"
    );
    assert_eq!(err.code(), "cli_exited");
    assert_eq!(w.session_dirs(), 0);
}

/// hook 程序不在：`channel_failed`，点名路径，连 CLI 都不起。
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn missing_hook_program_fails_before_starting_cli() {
    let mut w = World::new(happy_script(Some("dontAsk")));
    w.hook_program = w.work.path().join("no-such-hook");
    let err = w.open().await.err().expect("失败");
    assert_eq!(err.code(), "channel_failed");
    assert!(err.to_string().contains("no-such-hook"), "{err}");
    assert!(w.log().is_empty(), "CLI 没起");
    assert_eq!(w.session_dirs(), 0);
}

/// 审查 1/F-1：开会话期间 CLI 吐出不合协议的行（不是 JSON、不是对象），两个阶段都要失败，不能吞掉。
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn bad_line_while_opening_fails() {
    // 等报到时
    let w = World::new(json!([
        { "step": "emit_raw", "text": "not json" },
        { "step": "session_start" },
        { "step": "read_line" },
        { "step": "answer_initialize", "mode": "dontAsk" },
        { "step": "wait_stdin_eof" },
    ]));
    let err = w.open().await.err().expect("失败");
    assert!(
        matches!(err, OpenError::BadLine(LineError::NotJson)),
        "{err:?}"
    );
    assert_eq!(err.code(), "cli_exited");
    assert_eq!(w.session_dirs(), 0);

    // 等 initialize 响应时；前面先来一千多行合法的（超过留给映射的上限）也一样
    let mut steps = vec![
        json!({ "step": "session_start" }),
        json!({ "step": "read_line" }),
    ];
    for _ in 0..1001 {
        steps.push(json!({ "step": "emit", "line": { "type": "system", "subtype": "noise" } }));
    }
    steps.push(json!({ "step": "emit_raw", "text": "[1,2,3]" }));
    steps.push(json!({ "step": "answer_initialize", "mode": "dontAsk" }));
    steps.push(json!({ "step": "wait_stdin_eof" }));
    let w = World::new(Value::Array(steps));
    let err = w.open().await.err().expect("失败");
    assert!(
        matches!(err, OpenError::BadLine(LineError::NotJson)),
        "{err:?}"
    );
}

/// 审查 1/F-1：超过 8 MiB 的一行，读到上限就停、开会话失败，不把整行读进来。
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn oversize_line_while_opening_fails() {
    let w = World::new(json!([
        { "step": "session_start" },
        { "step": "read_line" },
        { "step": "emit_big", "bytes": 9 * 1024 * 1024 },
        { "step": "answer_initialize", "mode": "dontAsk" },
        { "step": "wait_stdin_eof" },
    ]));
    let err = w.open().await.err().expect("失败");
    assert!(
        matches!(err, OpenError::BadLine(LineError::TooLong { bytes }) if bytes <= MAX_LINE_BYTES + 64 * 1024),
        "读到上限附近就停：{err:?}"
    );
}

/// 审查 1/F-2：对不上编号的控制响应（里面带 account）不留原文，也不进留给映射的行。
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn unmatched_control_response_is_not_kept() {
    let email = "t06-unmatched@example.invalid";
    let w = World::new(json!([
        { "step": "session_start" },
        { "step": "emit", "line": { "type": "control_response", "response": {
            "subtype": "success", "request_id": "someone-else",
            "response": { "account": { "email": email } } } } },
        { "step": "read_line" },
        { "step": "emit", "line": { "type": "control_response", "response": {
            "subtype": "success", "request_id": "another",
            "response": { "account": { "email": email } } } } },
        { "step": "answer_initialize", "mode": "dontAsk" },
        { "step": "wait_stdin_eof" },
    ]));
    let mut s = w.open().await.expect("开会话成功");
    assert!(
        s.early_lines
            .iter()
            .all(|l| !l.contains(email) && !l.contains("control_response")),
        "{:?}",
        s.early_lines
    );
    s.cli.kill().await.unwrap();
    s.dir.remove().unwrap();
}

/// 审查 1/F-3：开会话的 future 被丢掉（调用方超时、ACP 请求被取消），会话目录照样删掉、CLI 照样结束。
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cancelled_open_leaves_nothing() {
    let mut w = World::new(json!([{ "step": "wait_stdin_eof" }]));
    w.startup_limit = Duration::from_secs(60);
    let pid = {
        let opening = w.open();
        tokio::pin!(opening);
        // 等假 CLI 起来、会话目录建好，再丢掉开会话的 future
        let started = tokio::select! {
            _ = &mut opening => panic!("报到不来，开会话不该结束"),
            v = w.wait_log(|v| v["kind"] == "started") => v,
        };
        assert_eq!(w.session_dirs(), 1);
        started["pid"].as_u64().unwrap() as i32
    };
    for _ in 0..100 {
        // SAFETY: 信号 0 只探测这个进程号在不在，不发真信号
        let alive = unsafe { libc::kill(pid, 0) } == 0;
        if !alive && w.session_dirs() == 0 {
            return;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    // SAFETY: 同上
    let alive = unsafe { libc::kill(pid, 0) } == 0;
    panic!(
        "5 秒后还有残留：会话目录 {} 个，CLI 还在：{alive}",
        w.session_dirs()
    );
}
