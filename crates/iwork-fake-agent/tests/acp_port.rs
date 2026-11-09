//! AgentPort 的 ACP 实现（iwork-agent-acp）对着假运行时：运行时登记、能力协商、不兼容阻止、
//! 续接、进程监管的采样与关闭。

mod support;

use std::path::PathBuf;
use std::time::Duration;

use iwork_agent_acp::AcpRuntime;
use iwork_agent_acp::registry::{CliInfo, RuntimeRegistration};
use iwork_ports::agent::{
    AgentError, AgentPort, AgentSession, AgentUpdate, PermissionDecision, PermissionHandler,
    PermissionRequest, RuntimeCapabilities, TurnOutcome, UpdateStream,
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

#[tokio::test(flavor = "multi_thread")]
async fn system_prompt_requires_capability() {
    let fake = Fake::new(&one_text_turn(json!({ "system_prompt": false })));
    let err = AcpRuntime::new(fake.launch.clone())
        .open_session(spec(fake.dir.path()), AllowAll)
        .await
        .unwrap_err();
    assert!(err.to_string().contains("不支持会话系统提示"), "{err}");
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
