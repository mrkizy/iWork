//! ACP 端的端到端（T06 方案 15.2、15.3；S6-3）：真实的 `iwork-agent-acp` 客户端 → 桥接 → 假 CLI → 真的 hook。
//!
//! | 测试 | 方案里的名字 |
//! | --- | --- |
//! | `text_turn_maps_to_updates` | 6.1 正常一轮 |
//! | `tool_call_goes_through_hook_and_backend` / `denied_tool_does_not_run` | 4.1 正常路径与拒绝 |
//! | `hook_rewrite_reaches_cli` | 同名（第 11 节） |
//! | `model_report_when_init_precedes_session_response`、`model_reported_every_turn_same_process` | 同名（6.2） |
//! | `init_with_*` 五条 | 同名（3.5 的会话内容核对，假 CLI 版） |
//! | `reconcile_*` 三条 | 同名（4.7） |
//! | `unregistered_is_error_result_fails_turn`、`unknown_result_subtype_is_error` | 同名（6.5） |
//! | `sandbox_status_is_per_session` | 同名（第 8 节） |
//! | `tool_call_outside_turn_is_denied` | 4.6 第 11 行的桥接一侧 |
#![cfg(unix)]

mod support;

use std::sync::Arc;

use iwork_domain::common::{ModelId, Usage};
use iwork_ports::agent::{
    AgentError, AgentSession, AgentUpdate, IsolationKind, PermissionDecision, RawToolInput,
    SandboxStatus, TurnOutcome,
};
use serde_json::{Value, json};
use support::acp::*;

fn script(steps: Vec<Value>) -> Value {
    let mut all = opening();
    all.extend(steps);
    all.push(wait_eof());
    Value::Array(all)
}

fn allow_all() -> Handler {
    Handler(Arc::new(Answers::default()))
}

#[tokio::test(flavor = "multi_thread")]
async fn text_turn_maps_to_updates() {
    let rig = Rig::new(script(vec![
        read_user(),
        init(),
        text("m1", "你好"),
        result_ok(),
    ]));
    let s = rig.open(rig.spec(), allow_all()).await.expect("开会话");
    let updates = turn(&s, "hi").await.unwrap();
    assert!(
        updates.iter().any(
            |u| matches!(u, AgentUpdate::ModelReported(m) if m.model_id == ModelId(MODEL.into()))
        ),
        "{updates:?}"
    );
    assert!(
        updates
            .iter()
            .any(|u| matches!(u, AgentUpdate::Text { text, .. } if text == "你好")),
        "{updates:?}"
    );
    assert!(updates.iter().any(|u| matches!(
        u,
        AgentUpdate::Usage(Usage {
            input_tokens: 3,
            output_tokens: 4,
            ..
        })
    )));
    assert_eq!(
        ends(&updates),
        [&AgentUpdate::Finished(TurnOutcome::Completed)]
    );
}

/// 4.1：工具调用经 hook 交给后台；后台允许，CLI 执行；`tool_call` 只报一次（CLI 输出与 hook 两边都有）。
#[tokio::test(flavor = "multi_thread")]
async fn tool_call_goes_through_hook_and_backend() {
    let dir = tempfile::tempdir().unwrap();
    let marker = dir.path().join("ran");
    let input = json!({ "command": "touch", "marker": marker.display().to_string() });
    let rig = Rig::new(script(vec![
        read_user(),
        init(),
        tool_use("toolu_1", "Bash", input.clone()),
        json!({ "step": "tool", "id": "toolu_1", "name": "Bash", "input": input }),
        tool_result("toolu_1", true, "ok"),
        result_ok(),
    ]));
    let answers = Arc::new(Answers::default());
    let s = rig
        .open(rig.spec(), Handler(answers.clone()))
        .await
        .unwrap();
    let updates = turn(&s, "跑一下").await.unwrap();
    assert_eq!(
        ends(&updates),
        [&AgentUpdate::Finished(TurnOutcome::Completed)]
    );
    assert!(marker.exists(), "允许了就执行");
    let seen = answers.seen.lock().unwrap();
    assert_eq!(seen.len(), 1);
    assert_eq!(seen[0].tool.0, "Bash");
    assert_eq!(seen[0].raw_input, RawToolInput(input.to_string()));
    assert!(seen[0].cwd.is_some(), "带了 hook 输入里的 cwd");
    let started = updates
        .iter()
        .filter(|u| matches!(u, AgentUpdate::ToolCallStarted { .. }))
        .count();
    assert_eq!(started, 1, "tool_call 只报一次：{updates:?}");
}

/// 后台拒绝：CLI 不执行，模型看到的是后台给的原因。
#[tokio::test(flavor = "multi_thread")]
async fn denied_tool_does_not_run() {
    let dir = tempfile::tempdir().unwrap();
    let marker = dir.path().join("ran");
    let input = json!({ "command": "touch", "marker": marker.display().to_string() });
    let rig = Rig::new(script(vec![
        read_user(),
        init(),
        json!({ "step": "tool", "id": "toolu_1", "name": "Bash", "input": input }),
        tool_result("toolu_1", false, "denied"),
        result_ok(),
    ]));
    let answers = Arc::new(Answers::default());
    *answers.decision.lock().unwrap() = Some(PermissionDecision::Deny {
        message: "规则不许".into(),
    });
    let s = rig.open(rig.spec(), Handler(answers)).await.unwrap();
    let updates = turn(&s, "跑一下").await.unwrap();
    assert_eq!(
        ends(&updates),
        [&AgentUpdate::Finished(TurnOutcome::Completed)]
    );
    assert!(!marker.exists());
    let tool = rig.log().into_iter().find(|v| v["kind"] == "tool").unwrap();
    assert_eq!(tool["decision"], "deny");
    assert_eq!(tool["reason"], "规则不许");
}

/// 第 11 节：后台给了改写后的输入，CLI 执行的是新的那一份。
#[tokio::test(flavor = "multi_thread")]
async fn hook_rewrite_reaches_cli() {
    let dir = tempfile::tempdir().unwrap();
    let original = dir.path().join("original");
    let rewritten = dir.path().join("rewritten");
    let input = json!({ "command": "touch", "marker": original.display().to_string() });
    let rig = Rig::new(script(vec![
        read_user(),
        init(),
        json!({ "step": "tool", "id": "toolu_1", "name": "Bash", "input": input }),
        tool_result("toolu_1", true, "ok"),
        result_ok(),
    ]));
    let answers = Arc::new(Answers::default());
    *answers.decision.lock().unwrap() = Some(PermissionDecision::AllowRewritten {
        new_input: RawToolInput(
            json!({ "command": "touch", "marker": rewritten.display().to_string() }).to_string(),
        ),
    });
    let s = rig.open(rig.spec(), Handler(answers)).await.unwrap();
    turn(&s, "跑一下").await.unwrap();
    assert!(rewritten.exists(), "执行的是改写后的输入");
    assert!(!original.exists());
}

/// 6.2：`init` 在开会话期间就到了（早于 `session/new` 的响应）：第一个回合照样报模型。
#[tokio::test(flavor = "multi_thread")]
async fn model_report_when_init_precedes_session_response() {
    let mut steps = vec![json!({ "step": "session_start" }), init()];
    steps.extend([
        json!({ "step": "read_line" }),
        json!({ "step": "answer_initialize", "mode": "dontAsk" }),
        read_user(),
        text("m1", "好"),
        result_ok(),
        wait_eof(),
    ]);
    let rig = Rig::new(Value::Array(steps));
    let s = rig.open(rig.spec(), allow_all()).await.unwrap();
    let updates = turn(&s, "hi").await.unwrap();
    assert!(
        updates
            .iter()
            .any(|u| matches!(u, AgentUpdate::ModelReported(_))),
        "{updates:?}"
    );
}

/// 6.2：同一个进程第二个回合没有新的 `init`，照样报模型（缓存的）。
#[tokio::test(flavor = "multi_thread")]
async fn model_reported_every_turn_same_process() {
    let rig = Rig::new(script(vec![
        read_user(),
        init(),
        text("m1", "一"),
        result_ok(),
        read_user(),
        text("m2", "二"),
        result_ok(),
    ]));
    let s = rig.open(rig.spec(), allow_all()).await.unwrap();
    for t in ["第一轮", "第二轮"] {
        let updates = turn(&s, t).await.unwrap();
        assert!(
            updates
                .iter()
                .any(|u| matches!(u, AgentUpdate::ModelReported(_))),
            "{t}：{updates:?}"
        );
    }
}

/// 回合里 `init` 核对不过 → 这一轮以 `isolation_broken` 失败（后台映射成协议错误）。
async fn isolation_case(spec_dirs: Vec<std::path::PathBuf>, init_line: Value) -> Vec<AgentUpdate> {
    let rig = Rig::new(script(vec![
        read_user(),
        init_line,
        text("m1", "好"),
        result_ok(),
    ]));
    let mut spec = rig.spec();
    spec.plugin_dirs = spec_dirs;
    let s = rig.open(spec, allow_all()).await.unwrap();
    turn(&s, "hi").await.unwrap()
}

fn isolation_failed(updates: &[AgentUpdate]) -> bool {
    matches!(
        ends(updates)[..],
        [AgentUpdate::Failed(AgentError::IsolationBroken { .. })]
    )
}

#[tokio::test(flavor = "multi_thread")]
async fn init_with_unexpected_plugin_breaks_isolation() {
    let updates = isolation_case(
        vec![],
        init_with(json!([{ "name": "mine", "path": "/home/u/.claude/plugins/mine", "source": "mine@inline" }]), None),
    )
    .await;
    // 种类与插件名一路带到后台（审查 1/F-6）：只名字，不带路径
    assert_eq!(
        ends(&updates),
        [&AgentUpdate::Failed(AgentError::IsolationBroken {
            kind: IsolationKind::Plugin,
            plugin: Some("mine".into())
        })],
        "{updates:?}"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn init_with_given_plugin_dir_passes() {
    let dir = tempfile::tempdir().unwrap();
    let plugin = dir.path().join("p1");
    let updates = isolation_case(
        vec![dir.path().to_path_buf()],
        init_with(
            json!([{ "name": "p1", "path": plugin.display().to_string(), "source": "p1@inline" }]),
            None,
        ),
    )
    .await;
    assert_eq!(
        ends(&updates),
        [&AgentUpdate::Finished(TurnOutcome::Completed)],
        "{updates:?}"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn init_without_plugins_key_breaks_isolation() {
    let mut line = init();
    line["line"].as_object_mut().unwrap().remove("plugins");
    let updates = isolation_case(vec![], line).await;
    assert!(isolation_failed(&updates), "{updates:?}");
}

#[tokio::test(flavor = "multi_thread")]
async fn init_with_foreign_memory_path_breaks_isolation() {
    let updates = isolation_case(
        vec![],
        init_with(json!([]), Some("/home/u/.claude/projects/-w-/memory")),
    )
    .await;
    assert!(isolation_failed(&updates), "{updates:?}");
}

#[tokio::test(flavor = "multi_thread")]
async fn init_with_redirected_or_absent_memory_path_passes() {
    // 没有这个键
    let updates = isolation_case(vec![], init_with(json!([]), None)).await;
    assert_eq!(
        ends(&updates),
        [&AgentUpdate::Finished(TurnOutcome::Completed)]
    );
    // 等于桥接给的改道目录（假 CLI 从设置文件里取出来填进去）
    let updates = isolation_case(vec![], init_with(json!([]), Some("$autoMemoryDirectory"))).await;
    assert_eq!(
        ends(&updates),
        [&AgentUpdate::Finished(TurnOutcome::Completed)],
        "{updates:?}"
    );
}

/// 4.7：一个没标错误的结果，它的调用没得到过允许（CLI 没问 hook 就执行了）→ 会话停下，`hook_inactive`。
#[tokio::test(flavor = "multi_thread")]
async fn reconcile_fires_on_unapproved_success() {
    let rig = Rig::new(script(vec![
        read_user(),
        init(),
        tool_use("toolu_9", "Bash", json!({ "command": "rm x" })),
        tool_result("toolu_9", true, "done"),
        result_ok(),
    ]));
    let s = rig.open(rig.spec(), allow_all()).await.unwrap();
    let updates = turn(&s, "hi").await.unwrap();
    assert!(
        matches!(ends(&updates)[..], [AgentUpdate::Failed(AgentError::Protocol { message })] if message.contains("审批 hook 没有生效")),
        "{updates:?}"
    );
}

/// 4.7：标了错误的结果不看（被拒、校验失败的结果都这样），回合照常。
#[tokio::test(flavor = "multi_thread")]
async fn reconcile_ignores_error_results() {
    let rig = Rig::new(script(vec![
        read_user(),
        init(),
        tool_use("toolu_9", "Bash", json!({ "command": "nope" })),
        tool_result("toolu_9", false, "No such tool"),
        result_ok(),
    ]));
    let s = rig.open(rig.spec(), allow_all()).await.unwrap();
    let updates = turn(&s, "hi").await.unwrap();
    assert_eq!(
        ends(&updates),
        [&AgentUpdate::Finished(TurnOutcome::Completed)]
    );
}

/// 4.7、13 节 S22：B 类工具（工作目录内的 Read）不经 hook 回了成功结果 → 同样停下。
#[tokio::test(flavor = "multi_thread")]
async fn reconcile_fires_on_unprompted_read() {
    let rig = Rig::new(script(vec![
        read_user(),
        init(),
        tool_use("toolu_r", "Read", json!({ "file_path": "/w/a.txt" })),
        tool_result("toolu_r", true, "secret"),
        result_ok(),
    ]));
    let s = rig.open(rig.spec(), allow_all()).await.unwrap();
    let updates = turn(&s, "hi").await.unwrap();
    assert!(
        matches!(ends(&updates)[..], [AgentUpdate::Failed(AgentError::Protocol { message })] if message.contains("审批 hook 没有生效")),
        "{updates:?}"
    );
}

/// 6.5：没登记的错误结局（`is_error` 为真、别的对不上）→ 回合失败，不当成完成。
#[tokio::test(flavor = "multi_thread")]
async fn unregistered_is_error_result_fails_turn() {
    let rig = Rig::new(script(vec![
        read_user(),
        init(),
        result(json!({ "subtype": "success", "is_error": true, "api_error_status": 418 })),
    ]));
    let s = rig.open(rig.spec(), allow_all()).await.unwrap();
    let updates = turn(&s, "hi").await.unwrap();
    assert!(
        matches!(ends(&updates)[..], [AgentUpdate::Failed(AgentError::Protocol { message })] if message.contains("运行时报告了错误")),
        "{updates:?}"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn unknown_result_subtype_is_error() {
    let rig = Rig::new(script(vec![
        read_user(),
        init(),
        result(json!({ "subtype": "brand_new", "is_error": false })),
    ]));
    let s = rig.open(rig.spec(), allow_all()).await.unwrap();
    let updates = turn(&s, "hi").await.unwrap();
    assert!(
        matches!(ends(&updates)[..], [AgentUpdate::Failed(_)]),
        "{updates:?}"
    );
}

/// 第 8 节：平台报支持，但这条会话没有证据说沙箱真的起来了（P4 未定，没有 `failIfUnavailable`），
/// 会话的状态报 `unavailable`，不报 `enforced`。
#[tokio::test(flavor = "multi_thread")]
async fn sandbox_status_is_per_session() {
    let rig = Rig::new(script(vec![]));
    let s = rig.open(rig.spec(), allow_all()).await.unwrap();
    assert_eq!(s.sandbox(), SandboxStatus::Unavailable);
}

/// 回合之外来问的工具调用（主回合结束后 CLI 自己又起的一轮）：桥接答拒绝，不交给后台。
#[tokio::test(flavor = "multi_thread")]
async fn tool_call_outside_turn_is_denied() {
    let dir = tempfile::tempdir().unwrap();
    let marker = dir.path().join("ran");
    let input = json!({ "command": "touch", "marker": marker.display().to_string() });
    let rig = Rig::new(script(vec![
        read_user(),
        init(),
        result_ok(),
        json!({ "step": "tool", "id": "toolu_late", "name": "Bash", "input": input }),
    ]));
    let answers = Arc::new(Answers::default());
    let s = rig
        .open(rig.spec(), Handler(answers.clone()))
        .await
        .unwrap();
    turn(&s, "hi").await.unwrap();
    for _ in 0..100 {
        if rig.log().iter().any(|v| v["kind"] == "tool") {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }
    let tool = rig
        .log()
        .into_iter()
        .find(|v| v["kind"] == "tool")
        .expect("hook 答了");
    assert_eq!(tool["decision"], "deny");
    assert!(!marker.exists());
    assert!(answers.seen.lock().unwrap().is_empty(), "没交给后台");
}

/// 审查 1/F-1：CLI 往 stdout 写一大行、暂时不读 stdin，同时后台发来一条大 prompt：桥接不能在写 stdin 上停住，
/// 要照样读 stdout，双方都往下走。假 CLI 先停半秒（让桥接开始写 2 MiB 的用户消息、写满管道），再写 2 MiB 的
/// 一行，然后才去读 stdin。修之前桥接卡在写上、假 CLI 卡在写上，这一轮永远不结束。
#[tokio::test(flavor = "multi_thread")]
async fn big_output_and_big_prompt_do_not_deadlock() {
    let rig = Rig::new(script(vec![
        json!({ "step": "sleep_ms", "ms": 500 }),
        json!({ "step": "emit_big", "bytes": 2 * 1024 * 1024 }),
        read_user(),
        init(),
        text("m1", "收到"),
        result_ok(),
    ]));
    let s = rig.open(rig.spec(), allow_all()).await.unwrap();
    let big = "大".repeat(700 * 1024);
    let updates = tokio::time::timeout(std::time::Duration::from_secs(20), turn(&s, &big))
        .await
        .expect("20 秒内这一轮结束，没有互等")
        .unwrap();
    assert_eq!(
        ends(&updates),
        [&AgentUpdate::Finished(TurnOutcome::Completed)]
    );
}

/// 审查 1/F-2：后台选了允许、改写后的输入却不合法：交给 hook 的是拒绝，对账也按拒绝记。CLI 之后照样回了
/// 这个编号的成功结果（没经允许就执行了）→ `hook_inactive`。修之前对账按后台的「允许」记，这一轮当成完成。
#[tokio::test(flavor = "multi_thread")]
async fn invalid_rewrite_is_denied_and_reconciled_as_denied() {
    let input = json!({ "command": "ls" });
    let rig = Rig::new(script(vec![
        read_user(),
        init(),
        json!({ "step": "tool", "id": "toolu_1", "name": "Bash", "input": input }),
        tool_result("toolu_1", true, "ran anyway"),
        result_ok(),
    ]));
    let answers = Arc::new(Answers::default());
    *answers.decision.lock().unwrap() = Some(PermissionDecision::AllowRewritten {
        new_input: RawToolInput("not json".into()),
    });
    let s = rig.open(rig.spec(), Handler(answers)).await.unwrap();
    let updates = turn(&s, "跑一下").await.unwrap();
    assert!(
        matches!(ends(&updates)[..], [AgentUpdate::Failed(AgentError::Protocol { message })] if message.contains("审批 hook 没有生效")),
        "{updates:?}"
    );
    let tool = rig.log().into_iter().find(|v| v["kind"] == "tool").unwrap();
    assert_eq!(tool["decision"], "deny", "交给 hook 的是拒绝");
}

/// 审查 1/F-3、F-5：回合之外 CLI 回了一个没经允许的成功结果：会话当场作废（`hook_inactive`），下一次 prompt
/// 拿到的是这个原因，不是泛泛的「CLI 退出了」。修之前回合外的结果不对账，下一轮照常完成。
#[tokio::test(flavor = "multi_thread")]
async fn unapproved_result_outside_turn_ends_session_with_its_reason() {
    let rig = Rig::new(script(vec![
        read_user(),
        init(),
        result_ok(),
        tool_use("toolu_9", "Bash", json!({ "command": "rm x" })),
        tool_result("toolu_9", true, "done"),
        read_user(),
        init(),
        result_ok(),
    ]));
    let s = rig.open(rig.spec(), allow_all()).await.unwrap();
    let first = turn(&s, "一").await.unwrap();
    assert_eq!(
        ends(&first),
        [&AgentUpdate::Finished(TurnOutcome::Completed)]
    );
    // 等回合外那两行到桥接
    for _ in 0..100 {
        if rig.log().iter().filter(|v| v["kind"] == "emitted").count() >= 5 {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }
    tokio::time::sleep(std::time::Duration::from_millis(200)).await;
    let err = match s
        .prompt(vec![iwork_ports::agent::ContentBlock::Text("二".into())])
        .await
    {
        Err(e) => e,
        Ok(mut u) => {
            use iwork_ports::agent::UpdateStream;
            let mut last = None;
            while let Some(x) = u.next().await {
                last = Some(x);
            }
            match last {
                Some(AgentUpdate::Failed(e)) => e,
                other => panic!("第二轮应当失败：{other:?}"),
            }
        }
    };
    assert!(
        matches!(&err, AgentError::Protocol { message } if message.contains("审批 hook 没有生效")),
        "{err:?}"
    );
}

/// 审查 1/F-5：开会话期间就到了的 `init` 带着外来的插件：开会话直接失败，原因是隔离失效（带插件名），
/// 不留到第一轮。
#[tokio::test(flavor = "multi_thread")]
async fn isolation_failure_while_opening_fails_the_open() {
    let rig = Rig::new(Value::Array(vec![
        json!({ "step": "session_start" }),
        init_with(
            json!([{ "name": "mine", "path": "/home/u/.claude/plugins/mine", "source": "mine@inline" }]),
            None,
        ),
        json!({ "step": "read_line" }),
        json!({ "step": "answer_initialize", "mode": "dontAsk" }),
        wait_eof(),
    ]));
    let err = rig
        .open(rig.spec(), allow_all())
        .await
        .err()
        .expect("开会话失败");
    assert_eq!(
        err,
        AgentError::IsolationBroken {
            kind: IsolationKind::Plugin,
            plugin: Some("mine".into())
        }
    );
    assert_eq!(
        std::fs::read_dir(rig.data.path().join("run").join("h"))
            .map(|d| d.count())
            .unwrap_or(0),
        0,
        "会话目录删了"
    );
}

/// 审查 2/F-1：握手时（`initialize` 响应）权限模式不符、或响应里没有那个键：开会话失败，种类是
/// `PermissionMode`，不是 `Unknown`。等不到响应的那一格要等满 20 秒的 `STARTUP_LIMIT`，在这一层不跑：
/// 它在开会话里得到的是同一个 `PermissionMode { reported: None }`（`tests/startup.rs` 的
/// `initialize_without_mode_blocks_session`），转成错误时和「没有那个键」走同一条路（`acp.rs` 的
/// `open_errors_keep_their_reason`）。
#[tokio::test(flavor = "multi_thread")]
async fn startup_permission_mismatch_preserves_kind() {
    for mode in [Some("default"), None] {
        let rig = Rig::new(Value::Array(vec![
            json!({ "step": "session_start" }),
            json!({ "step": "read_line" }),
            json!({ "step": "answer_initialize", "mode": mode }),
            wait_eof(),
        ]));
        let err = rig
            .open(rig.spec(), allow_all())
            .await
            .err()
            .expect("开会话失败");
        assert_eq!(
            err,
            AgentError::IsolationBroken {
                kind: IsolationKind::PermissionMode,
                plugin: None
            },
            "{mode:?}"
        );
    }
}
