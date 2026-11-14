//! AgentPort 的 ACP 实现（iwork-agent-acp）对着假运行时：运行时登记、能力协商、不兼容阻止、
//! 续接、进程监管的采样与关闭；T06 加的 `_meta` 约定项（等待上限、沙箱、上报、cwd）、点名错误码、
//! `terminate()`。

mod support;

use std::path::PathBuf;
use std::time::Duration;

use iwork_agent_acp::AcpRuntime;
use iwork_agent_acp::registry::{CliInfo, RuntimeRegistration};
use iwork_domain::common::{EffortLevel, ModelId, ModelSelection, Timestamp, Usage};
use iwork_domain::session::SessionLostReason;
use iwork_ports::agent::{
    AgentError, AgentPort, AgentSession, AgentUpdate, ModelOption, ModelUnavailableReason,
    PermissionDecision, PermissionHandler, PermissionRequest, QuotaReading, RuntimeCapabilities,
    SandboxPlatform, SandboxStatus, TurnOutcome, UpdateStream,
};
use serde_json::json;
use support::*;

struct AllowAll;

impl PermissionHandler for AllowAll {
    async fn on_permission(&self, _req: PermissionRequest) -> PermissionDecision {
        PermissionDecision::Allow
    }
}

fn one_text_turn(agent: serde_json::Value) -> serde_json::Value {
    json!({ "agent": agent, "turns": [{ "steps": [{ "type": "text", "text": "好" }] }] })
}

#[tokio::test(flavor = "multi_thread")]
async fn registration_records_adapter_cli_and_protocol_versions() {
    let fake = Fake::new(&one_text_turn(json!({
        "name": "fake-bridge", "version": "9.8.7",
        "cli": { "path": "/opt/vendor/cli", "version": "2.1.319" }
    })));
    let runtime = AcpRuntime::new(fake.launch.clone());
    let session = runtime
        .open_session(spec(fake.dir.path()), AllowAll)
        .await
        .unwrap();
    assert_eq!(
        session.registration(),
        &RuntimeRegistration {
            adapter_name: "fake-bridge".into(),
            adapter_version: "9.8.7".into(),
            protocol_version: 1,
            cli: Some(CliInfo {
                path: PathBuf::from("/opt/vendor/cli"),
                version: "2.1.319".into(),
            }),
        }
    );
    // 我们这一侧也报了自己的身份与协议版本
    let init = &fake.params_of("initialize")[0];
    assert_eq!(init["protocolVersion"], 1);
    assert_eq!(init["clientInfo"]["name"], "iwork");
    session.close().await.unwrap();
}

#[tokio::test(flavor = "multi_thread")]
async fn capabilities_come_from_initialize_and_probe_process_is_reaped() {
    let fake = Fake::new(&one_text_turn(
        json!({ "load_session": false, "image": true }),
    ));
    let caps = AcpRuntime::new(fake.launch.clone())
        .capabilities()
        .await
        .unwrap();
    assert_eq!(
        caps,
        RuntimeCapabilities {
            supports_resume: false,
            supports_image_input: true,
            effort_levels: Vec::new(),
            sandbox_platform: SandboxPlatform::Unsupported,
            model_options: Vec::new(),
        }
    );
    assert!(fake.params_of("session/new").is_empty(), "探测不开会话");
}

/// 验收 5（端口层）：已知不兼容组合返回 Incompatible，说明可读。协议版本不一致的判定在
/// iwork-agent-acp 的 registry 单元测试里（SDK 的 v1 连接回不出别的版本）。
#[tokio::test(flavor = "multi_thread")]
async fn incompatible_versions_are_blocked_with_readable_error() {
    let fake = Fake::new(&one_text_turn(
        json!({ "name": "codex-acp", "version": "0.16.0" }),
    ));
    let err = AcpRuntime::new(fake.launch.clone())
        .capabilities()
        .await
        .unwrap_err();
    assert_eq!(
        err.to_string(),
        "运行时版本不兼容：codex-acp 0.16.0 是已知不兼容的版本：适配器内置旧内核，所有模型都不可用（t0-findings 第 3 节实测）"
    );
    assert!(!err.retryable());

    // 开会话同样被挡，且挡在 session/new 之前
    let err = AcpRuntime::new(fake.launch.clone())
        .open_session(spec(fake.dir.path()), AllowAll)
        .await
        .unwrap_err();
    assert!(matches!(err, AgentError::Incompatible { .. }), "{err}");
    assert!(fake.params_of("session/new").is_empty());

    // 对照：同名的新版本可以运行
    let fake = Fake::new(&one_text_turn(
        json!({ "name": "codex-acp", "version": "2.5.0" }),
    ));
    assert!(
        AcpRuntime::new(fake.launch.clone())
            .capabilities()
            .await
            .is_ok()
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn resume_uses_session_load_and_requires_capability() {
    let fake = Fake::new(&one_text_turn(json!({})));
    let mut s = spec(fake.dir.path());
    s.resume = Some("old-session".into());
    let session = AcpRuntime::new(fake.launch.clone())
        .open_session(s.clone(), AllowAll)
        .await
        .unwrap();
    assert_eq!(session.runtime_session_id(), Some("old-session"));
    let loads = fake.params_of("session/load");
    assert_eq!(loads[0]["sessionId"], "old-session");
    assert!(loads[0]["_meta"]["iwork"]["sessionSetup"].is_string());
    drop(session);

    let fake = Fake::new(&one_text_turn(json!({ "load_session": false })));
    let err = AcpRuntime::new(fake.launch.clone())
        .open_session(s, AllowAll)
        .await
        .unwrap_err();
    assert!(err.to_string().contains("不支持续接会话"), "{err}");
}

/// 续接时运行时点名「会话记录用不了」：三个错误码各映射到 `SessionLost` 的一种，不是协议错误（T06 方案 5.2a）。
#[tokio::test(flavor = "multi_thread")]
async fn lost_session_codes_surface_as_session_lost() {
    for (code, reason) in [
        ("session_not_found", SessionLostReason::NotFound),
        ("session_corrupt", SessionLostReason::Corrupt),
        ("session_incompatible", SessionLostReason::Incompatible),
    ] {
        let fake = Fake::new(&one_text_turn(json!({ "load_error": code })));
        let mut s = spec(fake.dir.path());
        s.resume = Some("old-session".into());
        let err = AcpRuntime::new(fake.launch.clone())
            .open_session(s, AllowAll)
            .await
            .unwrap_err();
        assert_eq!(err, AgentError::SessionLost { reason }, "{code}");
        assert_eq!(fake.params_of("session/load").len(), 1);
    }
}

/// 本机导入功能接入前，会话挂 MCP 报可读错误，且不发出 session/new（透传移交 T10）。
#[tokio::test(flavor = "multi_thread")]
async fn mcp_is_rejected_until_local_import_lands() {
    let fake = Fake::new(&one_text_turn(json!({})));
    let mut s = spec(fake.dir.path());
    s.mcp = vec![iwork_ports::agent::McpServerRef {
        id: iwork_domain::event::McpServerId("m1".into()),
        name: "filesystem".into(),
    }];
    let err = AcpRuntime::new(fake.launch.clone())
        .open_session(s, AllowAll)
        .await
        .unwrap_err();
    assert_eq!(
        err.to_string(),
        "与运行时通信出错：M1 本机导入功能未接入前不支持挂载 MCP"
    );
    assert!(fake.params_of("session/new").is_empty());
}

/// Windows CI 上见过一次在这里挂了 40 多分钟（run 43537915924）：有界地跑，卡住时失败并打印现场。
#[test]
fn system_prompt_requires_capability() {
    let fake = Fake::new(&one_text_turn(json!({ "system_prompt": false })));
    let record = fake.record.clone();
    bounded(
        Duration::from_secs(60),
        move || fake_diag(&record),
        move || async move {
            let err = AcpRuntime::new(fake.launch.clone())
                .open_session(spec(fake.dir.path()), AllowAll)
                .await
                .unwrap_err();
            assert!(err.to_string().contains("不支持会话系统提示"), "{err}");
        },
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn prompt_streams_normalized_updates_then_ends() {
    let fake = Fake::new(&one_text_turn(json!({})));
    let session = AcpRuntime::new(fake.launch.clone())
        .open_session(spec(fake.dir.path()), AllowAll)
        .await
        .unwrap();
    let mut updates = session.prompt(text("hi")).await.unwrap();
    let mut got = Vec::new();
    while let Some(u) = updates.next().await {
        got.push(u);
    }
    assert_eq!(
        got,
        [
            AgentUpdate::Text {
                offset: 0,
                text: "好".into()
            },
            AgentUpdate::Finished(TurnOutcome::Completed),
        ]
    );
    // 原始消息进了诊断记录（脱敏、限量的单元测试在 iwork-agent-acp 里）
    assert!(
        session
            .raw_log()
            .iter()
            .any(|l| l.contains("agent_message_chunk"))
    );
    session.close().await.unwrap();
}

/// 技术设计 6.5：每 2 秒采样进程树的内存与 CPU；关闭会话后进程树清空。
#[tokio::test(flavor = "multi_thread")]
async fn process_tree_is_sampled_and_cleaned_up_on_close() {
    let fake = Fake::new(&one_text_turn(json!({})));
    let session = AcpRuntime::new(fake.launch.clone())
        .open_session(spec(fake.dir.path()), AllowAll)
        .await
        .unwrap();
    // 第一次采样在启动时立即做，之后每 2 秒一次
    let mut sample = None;
    for _ in 0..50 {
        sample = session.latest_sample();
        if sample.is_some() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    let sample = sample.expect("5 秒内有采样");
    assert!(sample.rss_bytes > 0, "{sample:?}");
    assert!(sample.processes >= 1, "{sample:?}");
    assert!(
        !session.process_tree_is_empty().unwrap(),
        "对照：运行中进程树非空"
    );

    let pid = session.process_id();
    session.close().await.unwrap();
    #[cfg(unix)]
    {
        // close 之后整组都已结束
        // SAFETY: 信号 0 只做存在性检查。
        let rc = unsafe { libc_killpg(pid as i32, 0) };
        assert_eq!(rc, -1, "进程组 {pid} 已不存在");
    }
    #[cfg(not(unix))]
    let _ = pid;
}

#[cfg(unix)]
unsafe extern "C" {
    #[link_name = "killpg"]
    fn libc_killpg(pgrp: i32, sig: i32) -> i32;
}

async fn drain(mut updates: impl UpdateStream) -> Vec<AgentUpdate> {
    let mut got = Vec::new();
    while let Some(u) = updates.next().await {
        got.push(u);
    }
    got
}

/// 运行时在 `initialize` 与开会话响应里报的等待上限、平台沙箱、可选值，以及这条会话的沙箱，
/// 原样到达端口；没报的按「不限时、平台不支持、会话没沙箱」。
#[tokio::test(flavor = "multi_thread")]
async fn session_reports_wait_limit_and_sandbox() {
    let fake = Fake::new(&one_text_turn(json!({
        "permission_wait_limit_secs": 7080,
        "sandbox_platform": "supported",
        "effort_levels": ["low", "high"],
        "models": [{ "id": "opus", "label": "Opus" }],
        "session_sandbox": "enforced",
    })));
    let runtime = AcpRuntime::new(fake.launch.clone());
    assert_eq!(
        runtime.capabilities().await.unwrap(),
        RuntimeCapabilities {
            supports_resume: true,
            supports_image_input: false,
            effort_levels: vec![EffortLevel("low".into()), EffortLevel("high".into())],
            sandbox_platform: SandboxPlatform::Supported,
            model_options: vec![ModelOption {
                id: ModelId("opus".into()),
                label: "Opus".into(),
            }],
        }
    );
    let session = runtime
        .open_session(spec(fake.dir.path()), AllowAll)
        .await
        .unwrap();
    assert_eq!(
        session.permission_wait_limit(),
        Some(Duration::from_secs(7080))
    );
    assert_eq!(session.sandbox(), SandboxStatus::Enforced);
    session.close().await.unwrap();

    // 没报：不限时；会话沙箱按没起来
    let fake = Fake::new(&one_text_turn(json!({})));
    let session = AcpRuntime::new(fake.launch.clone())
        .open_session(spec(fake.dir.path()), AllowAll)
        .await
        .unwrap();
    assert_eq!(session.permission_wait_limit(), None);
    assert_eq!(session.sandbox(), SandboxStatus::Unavailable);
    session.close().await.unwrap();
}

/// 续接后运行时换了会话 id：端口用新的。
#[tokio::test(flavor = "multi_thread")]
async fn resumed_session_takes_the_id_the_runtime_reports() {
    let fake = Fake::new(&one_text_turn(json!({
        "resumed_session_id": "cli-new-id",
        "session_sandbox": "unavailable",
    })));
    let mut s = spec(fake.dir.path());
    s.resume = Some("old-session".into());
    let session = AcpRuntime::new(fake.launch.clone())
        .open_session(s, AllowAll)
        .await
        .unwrap();
    assert_eq!(session.runtime_session_id(), Some("cli-new-id"));
    assert_eq!(session.sandbox(), SandboxStatus::Unavailable);
    assert_eq!(
        fake.params_of("session/load")[0]["sessionId"],
        "old-session"
    );
    session.close().await.unwrap();
}

/// 等待上限报错了值：不当成「不限」，连接被拒、探测进程结束。
#[tokio::test(flavor = "multi_thread")]
async fn malformed_wait_limit_is_rejected_not_treated_as_unlimited() {
    let fake = Fake::new(&one_text_turn(
        json!({ "permission_wait_limit_secs": "7080" }),
    ));
    let err = AcpRuntime::new(fake.launch.clone())
        .open_session(spec(fake.dir.path()), AllowAll)
        .await
        .unwrap_err();
    assert!(
        matches!(&err, AgentError::Protocol { message } if message.contains("permissionWaitLimitSecs")),
        "{err}"
    );
    assert!(fake.params_of("session/new").is_empty());
}

/// 运行时挂在 `session/update` 上的上报（模型、用量、额度）进当前回合的更新流，夹在正文之间、
/// 次序不变；解不出来的上报丢弃并记一笔诊断，不打断回合。
#[tokio::test(flavor = "multi_thread")]
async fn report_meta_maps_to_updates() {
    let fake = Fake::new(&json!({ "turns": [{ "steps": [
        { "type": "report_model", "model_id": "claude-opus", "effort": "high" },
        { "type": "text", "text": "好" },
        { "type": "report_raw", "value": "!!not-base64" },
        { "type": "report_quota", "windows": [
            { "window": "five_hour", "used_pct": 42, "resets_at_secs": 1731300000 },
            { "window": "seven_day", "used_pct": 7 }
        ] },
        { "type": "report_usage", "input_tokens": 10, "output_tokens": 20,
          "cache_read_tokens": 30, "cache_write_tokens": 40, "cost_usd": 0.5 }
    ] }] }));
    let session = AcpRuntime::new(fake.launch.clone())
        .open_session(spec(fake.dir.path()), AllowAll)
        .await
        .unwrap();
    let got = drain(session.prompt(text("hi")).await.unwrap()).await;
    assert_eq!(
        got,
        [
            AgentUpdate::ModelReported(ModelSelection {
                model_id: ModelId("claude-opus".into()),
                effort: Some(EffortLevel("high".into())),
            }),
            AgentUpdate::Text {
                offset: 0,
                text: "好".into()
            },
            AgentUpdate::Quota(vec![
                QuotaReading {
                    window: "five_hour".into(),
                    used_pct: 42,
                    resets_at: Some(Timestamp(1_731_300_000_000)),
                },
                QuotaReading {
                    window: "seven_day".into(),
                    used_pct: 7,
                    resets_at: None,
                },
            ]),
            AgentUpdate::Usage(Usage {
                input_tokens: 10,
                output_tokens: 20,
                cache_read_tokens: 30,
                cache_write_tokens: 40,
                cost_usd: Some(0.5),
            }),
            AgentUpdate::Finished(TurnOutcome::Completed),
        ]
    );
    let log = session.raw_log();
    assert!(log.iter().any(|l| l.contains("bad_report")), "{log:?}");
    assert!(
        !log.iter().any(|l| l.contains("not-base64")),
        "上报的内容不进诊断"
    );
    session.close().await.unwrap();
}

/// 记下收到的每个权限请求，然后拒绝。
#[derive(Default, Clone)]
struct Recording(std::sync::Arc<std::sync::Mutex<Vec<PermissionRequest>>>);

impl PermissionHandler for Recording {
    async fn on_permission(&self, req: PermissionRequest) -> PermissionDecision {
        self.0.lock().unwrap().push(req);
        PermissionDecision::Deny {
            message: "测试拒绝".into(),
        }
    }
}

/// 运行时给的这次调用的工作目录随权限请求到达处理方（X1）；没给的是 `None`。
#[tokio::test(flavor = "multi_thread")]
async fn permission_request_carries_runtime_cwd() {
    // 各平台的绝对路径写法不同：用临时目录
    let abs = std::env::temp_dir().join("sub");
    let cwd = abs.to_str().unwrap();
    let fake = Fake::new(&json!({ "turns": [{ "steps": [
        { "type": "tool_call", "id": "t1", "name": "Bash", "input": { "command": "rm -rf build" } },
        { "type": "permission", "tool_call_id": "t1", "cwd": cwd },
        { "type": "tool_call", "id": "t2", "name": "Bash", "input": { "command": "ls" } },
        { "type": "permission", "tool_call_id": "t2" }
    ] }] }));
    let handler = Recording::default();
    let session = AcpRuntime::new(fake.launch.clone())
        .open_session(spec(fake.dir.path()), handler.clone())
        .await
        .unwrap();
    drain(session.prompt(text("hi")).await.unwrap()).await;
    let cwds: Vec<_> = handler
        .0
        .lock()
        .unwrap()
        .iter()
        .map(|r| r.cwd.clone())
        .collect();
    assert_eq!(cwds, [Some(abs.clone()), None]);
    session.close().await.unwrap();
}

/// 运行时点名的错误码经映射到达端口：模型名取自会话请求，不取运行时给的文字；
/// 不像枚举的 `reason` 一律当 `unknown`。
#[tokio::test(flavor = "multi_thread")]
async fn named_error_codes_reach_the_port() {
    let fake = Fake::new(&json!({ "turns": [
        { "steps": [
            { "type": "text", "text": "好" },
            { "type": "error", "code": "model_unavailable", "reason": "overloaded" }
        ] },
    ] }));
    let session = AcpRuntime::new(fake.launch.clone())
        .open_session(spec(fake.dir.path()), AllowAll)
        .await
        .unwrap();
    let got = drain(session.prompt(text("hi")).await.unwrap()).await;
    let err = AgentError::ModelUnavailable {
        model: "fake-model".into(),
        reason: ModelUnavailableReason::Overloaded,
    };
    assert_eq!(got.last(), Some(&AgentUpdate::Failed(err.clone())));
    assert!(err.retryable());
    session.close().await.unwrap();

    let fake = Fake::new(&json!({ "turns": [
        { "steps": [{ "type": "error", "code": "cli_error", "reason": "leaked sk-ant-123" }] },
    ] }));
    let session = AcpRuntime::new(fake.launch.clone())
        .open_session(spec(fake.dir.path()), AllowAll)
        .await
        .unwrap();
    let got = drain(session.prompt(text("hi")).await.unwrap()).await;
    assert_eq!(
        got.last(),
        Some(&AgentUpdate::Failed(AgentError::Protocol {
            message: "运行时报告了错误（unknown）".into()
        }))
    );
    session.close().await.unwrap();
}

/// `terminate()` 结束整棵树（含运行时起的、还在跑的子进程）并确认清空才返回；不看有没有回合，
/// 可以重复调。
#[cfg(unix)]
#[tokio::test(flavor = "multi_thread")]
async fn terminate_confirms_tree_empty() {
    let fake = Fake::new(&json!({ "turns": [{ "steps": [
        { "type": "spawn_child", "program": "sleep", "args": ["30"] },
        { "type": "hang" }
    ] }] }));
    let session = AcpRuntime::new(fake.launch.clone())
        .open_session(spec(fake.dir.path()), AllowAll)
        .await
        .unwrap();
    let updates = session.prompt(text("hi")).await.unwrap();
    let mut child = None;
    for _ in 0..100 {
        child = fake
            .records()
            .iter()
            .find_map(|r| r["spawned_child"].as_i64());
        if child.is_some() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    let child = child.expect("5 秒内子进程起来了") as u32;
    assert!(process_exists(child), "对照：子进程在跑");
    assert!(!session.process_tree_is_empty().unwrap(), "对照：树非空");

    session.terminate().await.unwrap();
    assert!(session.process_tree_is_empty().unwrap());
    // 子进程已被结束。它可能还是僵尸：它的父进程（假运行时）也被杀了，它由 init 收养、回收，什么时候回收不归
    // 清理管，清理判空也不把僵尸算进去。所以只看它是不是还在跑，不用 kill(pid, 0)（对僵尸照样成功）
    assert!(!process_exists(child), "子进程 {child} 已结束");
    // 已经清空：再调立刻成功
    session.terminate().await.unwrap();
    // 进行中的回合以失败收尾，不会挂住
    let got = tokio::time::timeout(Duration::from_secs(5), drain(updates))
        .await
        .expect("回合收尾了");
    assert!(
        matches!(got.last(), Some(AgentUpdate::Failed(_))),
        "{got:?}"
    );
}

/// 1/F-5：运行时给的 `cwd` 不是绝对路径：不交给处理方，按拒绝答复运行时，拒绝理由点名键。
#[tokio::test(flavor = "multi_thread")]
async fn relative_cwd_is_denied_without_asking_handler() {
    let fake = Fake::new(&json!({ "turns": [{ "steps": [
        { "type": "tool_call", "id": "t1", "name": "Bash", "input": { "command": "rm -rf build" } },
        { "type": "permission", "tool_call_id": "t1", "cwd": "sub/dir",
          "allowed": [{ "type": "text", "text": "执行了" }],
          "denied": [{ "type": "text", "text": "没执行" }] }
    ] }] }));
    let handler = Recording::default();
    let session = AcpRuntime::new(fake.launch.clone())
        .open_session(spec(fake.dir.path()), handler.clone())
        .await
        .unwrap();
    let got = drain(session.prompt(text("hi")).await.unwrap()).await;
    assert!(handler.0.lock().unwrap().is_empty(), "处理方没被问到");
    assert!(
        got.iter()
            .any(|u| matches!(u, AgentUpdate::Text { text, .. } if text == "没执行")),
        "{got:?}"
    );
    let resp = fake
        .records()
        .into_iter()
        .find_map(|r| r.get("permission_response").cloned())
        .expect("运行时收到了答复");
    assert_eq!(resp["outcome"]["optionId"], "reject-once", "{resp}");
    assert!(
        resp["_meta"]["iwork"]["message"]
            .as_str()
            .unwrap()
            .contains("_meta.iwork.cwd"),
        "{resp}"
    );
    assert!(
        session
            .raw_log()
            .iter()
            .any(|l| l.contains("bad_permission_meta"))
    );
    session.close().await.unwrap();
}

/// 1/F-7：上报挂在一条带正文的更新上（不合约定）：正文和上报都到，不丢一样。
#[tokio::test(flavor = "multi_thread")]
async fn report_on_a_text_update_keeps_both() {
    let fake = Fake::new(&json!({ "turns": [{ "steps": [
        { "type": "report_model", "model_id": "claude-opus", "on_text": "好" }
    ] }] }));
    let session = AcpRuntime::new(fake.launch.clone())
        .open_session(spec(fake.dir.path()), AllowAll)
        .await
        .unwrap();
    let got = drain(session.prompt(text("hi")).await.unwrap()).await;
    assert_eq!(
        got,
        [
            AgentUpdate::Text {
                offset: 0,
                text: "好".into()
            },
            AgentUpdate::ModelReported(ModelSelection {
                model_id: ModelId("claude-opus".into()),
                effort: None,
            }),
            AgentUpdate::Finished(TurnOutcome::Completed),
        ]
    );
    session.close().await.unwrap();
}

// ---------- T06 S2：权限任务只转发处理方的返回值；cancel() 先等答复写出去 ----------

/// 处理方：进来一次记一次，等测试放行后拒绝；`release` 一直不放就一直不返回。
#[derive(Default, Clone)]
struct Releasable {
    entered: std::sync::Arc<std::sync::atomic::AtomicUsize>,
    release: std::sync::Arc<tokio::sync::Notify>,
}

impl PermissionHandler for Releasable {
    async fn on_permission(&self, _req: PermissionRequest) -> PermissionDecision {
        self.entered
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        self.release.notified().await;
        PermissionDecision::Deny {
            message: "测试拒绝".into(),
        }
    }
}

fn waiting_permission_turn() -> serde_json::Value {
    json!({ "turns": [{ "steps": [
        { "type": "tool_call", "id": "t1", "name": "Bash", "input": { "command": "rm -rf build" } },
        { "type": "permission", "tool_call_id": "t1" },
        { "type": "sleep", "ms": 600000 }
    ] }] })
}

async fn entered(handler: &Releasable) {
    let start = std::time::Instant::now();
    while handler.entered.load(std::sync::atomic::Ordering::SeqCst) == 0 {
        assert!(
            start.elapsed() < Duration::from_secs(5),
            "权限请求没到处理方"
        );
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
}

/// 处理方一直不返回时，取消不会让权限任务自己答「已取消」：运行时收不到任何权限答复，
/// `session/cancel` 照样在上限之后发出，回合以结束进程树收尾。
#[tokio::test(flavor = "multi_thread")]
async fn permission_task_forwards_handler_result_only() {
    let fake = Fake::new(&waiting_permission_turn());
    let handler = Releasable::default();
    let session = AcpRuntime::new(fake.launch.clone())
        .open_session(spec(fake.dir.path()), handler.clone())
        .await
        .unwrap();
    let updates = session.prompt(text("hi")).await.unwrap();
    entered(&handler).await;
    session.cancel().await.unwrap();
    let got = drain(updates).await;
    assert_eq!(
        got.last(),
        Some(&AgentUpdate::Finished(TurnOutcome::Cancelled))
    );
    let arrivals = fake.arrivals();
    assert!(
        arrivals.iter().any(|a| a == "session/cancel"),
        "对照：取消发出去了：{arrivals:?}"
    );
    assert!(
        !arrivals
            .iter()
            .any(|a| a.starts_with("permission_response")),
        "适配器没有替处理方答复：{arrivals:?}"
    );
    session.close().await.unwrap();
}

/// `cancel()` 先等这条会话上的权限任务把答复写出去，再发 `session/cancel`：处理方返回之前没有取消，
/// 返回之后答复排在取消之前。
#[tokio::test(flavor = "multi_thread")]
async fn cancel_waits_for_permission_responses() {
    let fake = Fake::new(&waiting_permission_turn());
    let handler = Releasable::default();
    let session = std::sync::Arc::new(
        AcpRuntime::new(fake.launch.clone())
            .open_session(spec(fake.dir.path()), handler.clone())
            .await
            .unwrap(),
    );
    let updates = session.prompt(text("hi")).await.unwrap();
    entered(&handler).await;
    let cancelling = {
        let session = session.clone();
        tokio::spawn(async move { session.cancel().await })
    };
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert!(!cancelling.is_finished(), "处理方还没返回，cancel() 在等");
    assert!(
        !fake.arrivals().iter().any(|a| a == "session/cancel"),
        "{:?}",
        fake.arrivals()
    );
    handler.release.notify_one();
    cancelling.await.unwrap().unwrap();
    drain(updates).await;
    let arrivals = fake.arrivals();
    let pos = |x: &str| arrivals.iter().position(|a| a == x).unwrap_or(usize::MAX);
    assert!(
        pos("permission_response t1 reject-once") < pos("session/cancel")
            && pos("session/cancel") != usize::MAX,
        "{arrivals:?}"
    );
}

/// 有界外壳自己的对照：用例本体永远不结束，时限到了就失败，并说清卡在用例本体。
#[test]
#[should_panic(expected = "用例本体还没跑完")]
fn bounded_fails_when_the_body_hangs() {
    bounded(
        Duration::from_millis(300),
        || "（对照）".into(),
        std::future::pending::<()>,
    );
}

/// 有界外壳自己的对照：用例本体跑完了，但有阻塞任务卡住、运行时收尾不完，同样在时限内失败。
#[test]
#[should_panic(expected = "运行时收尾")]
fn bounded_fails_when_runtime_teardown_hangs() {
    bounded(
        Duration::from_millis(600),
        || "（对照）".into(),
        || async {
            tokio::task::spawn_blocking(|| std::thread::sleep(Duration::from_secs(5)));
        },
    );
}
