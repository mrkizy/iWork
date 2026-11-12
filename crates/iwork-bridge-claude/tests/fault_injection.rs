//! 故障注入（T06 方案 4.3、4.6 第 13 行；`fault-injection` + `fake-cli` feature）：hook 的 timeout 调成 10 秒，
//! 三个期限是 4 / 6 / 8 秒。跑法：`cargo test -p iwork-bridge-claude --features fake-cli,fault-injection --test fault_injection`。
//!
//! 这里测的是桥接自己的那一段：等待上限报给后台、后台一直不答时桥接第 6 秒兜底（答 hook 拒绝、这一轮以
//! `approval_backstop` 失败、命令没执行）。验收 2 的完整流程（编排到第 4 秒中断、审批保留、批准后续接恰好执行
//! 一次）要接回合编排与参照判定方，在 S6-4b 做。
#![cfg(unix)]

mod support;

use std::time::{Duration, Instant};

use iwork_ports::agent::{
    AgentError, AgentSession, AgentUpdate, PermissionDecision, PermissionHandler, PermissionRequest,
};
use serde_json::{Value, json};
use support::acp::*;

/// 一直不答的处理方（后台卡住）。
struct Never;

impl PermissionHandler for Never {
    async fn on_permission(&self, _req: PermissionRequest) -> PermissionDecision {
        std::future::pending().await
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn backstop_fires_when_backend_never_answers() {
    let dir = tempfile::tempdir().unwrap();
    let marker = dir.path().join("ran");
    let input = json!({ "command": "touch", "marker": marker.display().to_string() });
    let mut steps = opening();
    steps.extend([
        read_user(),
        init(),
        json!({ "step": "tool", "id": "toolu_1", "name": "Bash", "input": input }),
        wait_eof(),
    ]);
    let mut rig = Rig::new(Value::Array(steps));
    rig.launch
        .env
        .push(("IWORK_BRIDGE_HOOK_TIMEOUT_SECS".into(), "10".into()));
    let s = rig.open(rig.spec(), Never).await.expect("开会话");
    assert_eq!(
        s.permission_wait_limit(),
        Some(Duration::from_secs(4)),
        "等待上限 = deadlines(10s).daemon"
    );
    let started = Instant::now();
    let updates = turn(&s, "跑一下").await.unwrap();
    let took = started.elapsed();
    assert!(
        matches!(ends(&updates)[..], [AgentUpdate::Failed(AgentError::Protocol { message })] if message.contains("兜底")),
        "{updates:?}"
    );
    assert!(
        took >= Duration::from_secs(5) && took < Duration::from_secs(9),
        "第 6 秒左右兜底：{took:?}"
    );
    assert!(!marker.exists(), "命令没执行");
    // 兜底之后桥接马上收掉会话、结束 CLI：假 CLI 未必来得及把 hook 的决定写进记录，不拿那条记录断言
}

/// 不开这个 feature 的构建不认这个变量：这条只在开了 feature 时跑，反向的那一半由
/// `scripts/check-no-dev-trust.sh` 的 feature 图检查保证（发布构建里这个 feature 没开）。
#[tokio::test(flavor = "multi_thread")]
async fn too_small_timeout_is_refused() {
    let mut rig = Rig::new(Value::Array(opening()));
    rig.launch
        .env
        .push(("IWORK_BRIDGE_HOOK_TIMEOUT_SECS".into(), "3".into()));
    let err = rig.open(rig.spec(), Never).await.err().expect("起不来");
    assert!(
        matches!(
            err,
            AgentError::ProcessExited { .. } | AgentError::Protocol { .. }
        ),
        "{err}"
    );
}
