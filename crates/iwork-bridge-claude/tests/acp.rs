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

/// 发一条消息，等到第一段正文出来，返回更新流（之后由测试决定取消）。
async fn started(
    s: &impl AgentSession,
) -> (impl iwork_ports::agent::UpdateStream, Vec<AgentUpdate>) {
    use iwork_ports::agent::{ContentBlock, UpdateStream};
    let mut updates = s
        .prompt(vec![ContentBlock::Text("hi".into())])
        .await
        .unwrap();
    let mut seen = Vec::new();
    loop {
        let u = tokio::time::timeout(std::time::Duration::from_secs(10), updates.next())
            .await
            .expect("10 秒内有更新")
            .expect("还没结束");
        let text = matches!(u, AgentUpdate::Text { .. });
        seen.push(u);
        if text {
            return (updates, seen);
        }
    }
}

async fn rest(mut updates: impl iwork_ports::agent::UpdateStream) -> Vec<AgentUpdate> {
    let mut all = Vec::new();
    while let Some(u) = tokio::time::timeout(std::time::Duration::from_secs(15), updates.next())
        .await
        .expect("15 秒内有更新或结束")
    {
        all.push(u);
    }
    all
}

/// 5.1：取消 → 桥接发 `interrupt` → CLI 给出这一轮的 `result` → 桥接马上回 prompt。客户端（T05）在 prompt 结束后
/// 总会结束进程树、报 `Cancelled`（会话靠续接接着用），所以这里看的是：CLI 收到了 `interrupt`；从取消到收尾
/// 远小于 3 秒（客户端等 prompt 结束才清树，桥接不回 prompt 要等满 3 秒）；没发 SIGTERM。
#[tokio::test(flavor = "multi_thread")]
async fn cancel_interrupts_and_answers_the_prompt() {
    let rig = Rig::new(script(vec![
        read_user(),
        init(),
        text("m1", "开始"),
        json!({ "step": "read_line" }),
        result(
            json!({ "subtype": "error_during_execution", "is_error": true, "terminal_reason": "aborted_streaming" }),
        ),
    ]));
    let s = rig.open(rig.spec(), allow_all()).await.unwrap();
    let (updates, _) = started(&s).await;
    let cancelled_at = std::time::Instant::now();
    s.cancel().await.unwrap();
    let after = rest(updates).await;
    assert_eq!(
        ends(&after),
        [&AgentUpdate::Finished(TurnOutcome::Cancelled)],
        "{after:?}"
    );
    assert!(
        cancelled_at.elapsed() < std::time::Duration::from_millis(2500),
        "桥接回了 prompt，客户端没有等满 3 秒：{:?}",
        cancelled_at.elapsed()
    );
    let log = rig.log();
    assert!(
        !log.iter().any(|v| v["kind"] == "got_signal"),
        "等到了 result，不发 SIGTERM"
    );
    let stdin: Vec<Value> = log
        .iter()
        .filter(|v| v["kind"] == "stdin")
        .map(|v| serde_json::from_str(v["line"].as_str().unwrap()).unwrap())
        .collect();
    assert_eq!(stdin[2]["type"], "control_request");
    assert_eq!(stdin[2]["request"]["subtype"], "interrupt", "{stdin:?}");
}

/// 5.1 第 1 步：取消时还在等的 hook 全部答拒绝（和后台撤票同一句），命令不执行。
#[tokio::test(flavor = "multi_thread")]
async fn cancel_denies_waiting_hooks() {
    struct Pending(Arc<Answers>);
    impl iwork_ports::agent::PermissionHandler for Pending {
        async fn on_permission(
            &self,
            req: iwork_ports::agent::PermissionRequest,
        ) -> PermissionDecision {
            self.0.seen.lock().unwrap().push(req);
            std::future::pending().await
        }
    }
    let dir = tempfile::tempdir().unwrap();
    let marker = dir.path().join("ran");
    let input = json!({ "command": "touch", "marker": marker.display().to_string() });
    let rig = Rig::new(script(vec![
        read_user(),
        init(),
        text("m1", "开始"),
        json!({ "step": "tool", "id": "toolu_1", "name": "Bash", "input": input }),
        json!({ "step": "read_line" }),
        result(
            json!({ "subtype": "error_during_execution", "is_error": true, "terminal_reason": "aborted_tools" }),
        ),
    ]));
    let answers = Arc::new(Answers::default());
    let s = rig
        .open(rig.spec(), Pending(answers.clone()))
        .await
        .unwrap();
    let (updates, _) = started(&s).await;
    for _ in 0..200 {
        if !answers.seen.lock().unwrap().is_empty() {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(25)).await;
    }
    assert_eq!(answers.seen.lock().unwrap().len(), 1, "权限请求到了后台");
    s.cancel().await.unwrap();
    let after = rest(updates).await;
    assert_eq!(
        ends(&after),
        [&AgentUpdate::Finished(TurnOutcome::Cancelled)],
        "{after:?}"
    );
    let tool = rig.log().into_iter().find(|v| v["kind"] == "tool").unwrap();
    assert_eq!(tool["decision"], "deny");
    assert_eq!(tool["reason"], "回合已结束，权限请求作废");
    assert!(!marker.exists());
}

/// 5.1 第 2 步：发了 `interrupt` 1.5 秒还没有 `result` → 只给 CLI 发 SIGTERM（假 CLI 收到的是 15，不是被 SIGKILL）。
/// CLI 退了，桥接照「CLI 退出」那条路收；客户端（T05）在取消后总会结束进程树，那一层照样报 `Cancelled`。
#[tokio::test(flavor = "multi_thread")]
async fn cancel_sends_sigterm_after_interrupt_gets_no_result() {
    let rig = Rig::new(Value::Array({
        let mut v = opening();
        v.extend([
            read_user(),
            init(),
            text("m1", "开始"),
            json!({ "step": "wait_for_sigterm" }),
        ]);
        v
    }));
    let s = rig.open(rig.spec(), allow_all()).await.unwrap();
    let (updates, _) = started(&s).await;
    let cancelled_at = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_millis() as u64;
    s.cancel().await.unwrap();
    let after = rest(updates).await;
    // 客户端在取消后总会结束进程树，这一层照样报 Cancelled；桥接这一段看的是下面的 SIGTERM
    assert_eq!(
        ends(&after),
        [&AgentUpdate::Finished(TurnOutcome::Cancelled)],
        "{after:?}"
    );
    let got = rig
        .log()
        .into_iter()
        .find(|v| v["kind"] == "got_signal")
        .expect("假 CLI 收到了信号");
    assert_eq!(got["signal"], 15, "SIGTERM");
    let waited = got["at_ms"].as_u64().unwrap() - cancelled_at;
    assert!(
        (1400..2500).contains(&waited),
        "取消后约 1.5 秒：{waited}ms"
    );
}

/// 3.1a（D5）：后台任务的两个信号，看到任何一样这一轮按 `cli_error`（`background_task`）失败，会话收掉。
#[tokio::test(flavor = "multi_thread")]
async fn background_task_event_fails_the_turn() {
    for signal in [
        json!({ "type": "system", "subtype": "task_started", "task_id": "a", "is_backgrounded": true }),
        json!({ "type": "system", "subtype": "background_tasks_changed", "tasks": [{ "task_id": "a" }] }),
    ] {
        let rig = Rig::new(script(vec![
            read_user(),
            init(),
            emit(signal.clone()),
            result_ok(),
        ]));
        let s = rig.open(rig.spec(), allow_all()).await.unwrap();
        let updates = turn(&s, "hi").await.unwrap();
        assert!(
            matches!(ends(&updates)[..], [AgentUpdate::Failed(AgentError::Protocol { message })] if message.contains("background_task")),
            "{signal}：{updates:?}"
        );
    }
    // 对照：前台子代理（is_backgrounded: false）与空的任务清单不算
    let rig = Rig::new(script(vec![
        read_user(),
        init(),
        emit(
            json!({ "type": "system", "subtype": "task_started", "task_id": "a", "is_backgrounded": false }),
        ),
        emit(json!({ "type": "system", "subtype": "background_tasks_changed", "tasks": [] })),
        result_ok(),
    ]));
    let s = rig.open(rig.spec(), allow_all()).await.unwrap();
    let updates = turn(&s, "hi").await.unwrap();
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

/// 4.4、4.6 第 14 行在 ACP 这一层：hook 在等答复时死了（被 CLI 杀掉、自己到期），桥接看到 hook 断开，
/// 这一轮以 `approval_backstop` 失败、会话收掉，不等后台以后的答复。后台这边一直不答。
///
/// 后台收到权限请求时建一个文件，假 CLI 等它出现再杀 hook 的整个进程组：这时 hook 一定已经连上、在等答复。
/// 原来按「起来 300 毫秒后只杀 sh」：Linux 的 dash 不把 `sh -c` exec 成 hook，hook 活着、连接不断；
/// 机器慢时 hook 还没连上就被杀，桥接根本不知道有这次调用。两种都让这一轮永远不结束（CI 43280644009）。
#[tokio::test(flavor = "multi_thread")]
async fn hook_lost_while_waiting_ends_the_session() {
    struct Never(std::path::PathBuf);
    impl iwork_ports::agent::PermissionHandler for Never {
        async fn on_permission(
            &self,
            _req: iwork_ports::agent::PermissionRequest,
        ) -> PermissionDecision {
            std::fs::write(&self.0, b"").unwrap();
            std::future::pending().await
        }
    }
    let dir = tempfile::tempdir().unwrap();
    let marker = dir.path().join("ran");
    let asked = dir.path().join("asked");
    let input = json!({ "command": "touch", "marker": marker.display().to_string() });
    let rig = Rig::new(script(vec![
        read_user(),
        init(),
        json!({ "step": "tool", "id": "toolu_1", "name": "Bash", "input": input,
            "kill_hook_when": asked.display().to_string() }),
    ]));
    let s = rig.open(rig.spec(), Never(asked.clone())).await.unwrap();
    let updates = tokio::time::timeout(std::time::Duration::from_secs(10), turn(&s, "跑一下"))
        .await
        .expect("hook 死了之后 10 秒内这一轮结束")
        .unwrap();
    assert!(
        matches!(ends(&updates)[..], [AgentUpdate::Failed(AgentError::Protocol { message })] if message.contains("兜底")),
        "{updates:?}"
    );
    assert!(!marker.exists());
}

/// 5.1：取消之后（interrupt 已发、这一轮还没结束）CLI 又起了一次工具调用：hook 拿到「回合已结束」的拒绝，
/// 不交给后台，命令不执行（`TurnGate::hook` 在取消中给 `DenyTurnGone`）。
#[tokio::test(flavor = "multi_thread")]
async fn hook_after_cancel_is_denied_without_asking() {
    let dir = tempfile::tempdir().unwrap();
    let marker = dir.path().join("ran");
    let input = json!({ "command": "touch", "marker": marker.display().to_string() });
    let rig = Rig::new(script(vec![
        read_user(),
        init(),
        text("m1", "开始"),
        json!({ "step": "read_line" }),
        json!({ "step": "tool", "id": "toolu_late", "name": "Bash", "input": input }),
        result(json!({
            "subtype": "error_during_execution", "is_error": true, "terminal_reason": "aborted_tools"
        })),
    ]));
    let answers = Arc::new(Answers::default());
    let s = rig
        .open(rig.spec(), Handler(answers.clone()))
        .await
        .unwrap();
    let (updates, _) = started(&s).await;
    s.cancel().await.unwrap();
    let after = rest(updates).await;
    assert_eq!(
        ends(&after),
        [&AgentUpdate::Finished(TurnOutcome::Cancelled)],
        "{after:?}"
    );
    let tool = rig
        .log()
        .into_iter()
        .find(|v| v["kind"] == "tool")
        .expect("取消后 hook 照样问了");
    assert_eq!(tool["decision"], "deny");
    assert_eq!(tool["reason"], "回合已结束，权限请求作废");
    assert!(!marker.exists());
    assert!(answers.seen.lock().unwrap().is_empty(), "没交给后台");
}

/// 审查 S6-4 1/F-2 的对照：不开故障注入时，设置文件里 hook 的 timeout 是产品值 7200，报给后台的等待上限是
/// 由它算出来的 7080 秒。
#[tokio::test(flavor = "multi_thread")]
async fn product_hook_timeout_reaches_settings_and_wait_limit() {
    let rig = Rig::new(script(vec![]));
    let s = rig.open(rig.spec(), allow_all()).await.unwrap();
    let settings: Value = serde_json::from_str(
        &std::fs::read_to_string(rig.record.path().join("settings.json")).unwrap(),
    )
    .unwrap();
    assert_eq!(
        settings["hooks"]["PreToolUse"][0]["hooks"][0]["timeout"],
        7200
    );
    assert_eq!(
        s.permission_wait_limit(),
        Some(std::time::Duration::from_secs(7080))
    );
}
