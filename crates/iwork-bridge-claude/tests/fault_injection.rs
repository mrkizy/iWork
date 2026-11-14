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
    // 设置文件里 hook 的 timeout 与算期限用的是同一个值（审查 1/F-2）
    let settings: Value = serde_json::from_str(
        &std::fs::read_to_string(rig.record.path().join("settings.json")).unwrap(),
    )
    .unwrap();
    assert_eq!(
        settings["hooks"]["PreToolUse"][0]["hooks"][0]["timeout"],
        10
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

/// 会话任务 panic（审查 S6-5b 停放期第 12 条）：桥接整个退出（不留一个没有会话任务的桥接），后台看到输出到头就
/// 清树。CLI 这时还活着、父链没断，它另立会话的命令一起被清掉。
#[tokio::test(flavor = "multi_thread")]
async fn session_task_panic_ends_the_bridge_and_daemon_clears() {
    let mut steps = opening();
    steps.extend([
        read_user(),
        init(),
        json!({ "step": "spawn", "command": "exec sleep 30" }),
        text("m1", "PANIC-HERE"),
        json!({ "step": "log_signals_until_term" }),
    ]);
    let mut rig = Rig::new(Value::Array(steps));
    rig.launch
        .env
        .push(("IWORK_BRIDGE_FAULT_PANIC_ON".into(), "PANIC-HERE".into()));
    let session = rig
        .open(rig.spec(), Handler(Default::default()))
        .await
        .unwrap();
    let updates = turn(&session, "hi").await;
    let started = Instant::now();
    let cmd = pid_of(&rig, "spawned");
    let cli = pid_of(&rig, "started");
    let bridge = rig
        .log()
        .into_iter()
        .find(|v| v["kind"] == "started")
        .unwrap()["parent"]
        .as_i64()
        .unwrap() as i32;
    // 桥接 panic 后整个进程没了
    while alive(bridge) {
        assert!(
            started.elapsed() < Duration::from_secs(5),
            "5 秒内桥接退出（会话任务 panic 时不留着进程）：{updates:?}"
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    // 后台按输出到头清树：CLI 和它另立会话的命令都没了
    while alive(cli) || alive(cmd) {
        assert!(
            started.elapsed() < Duration::from_secs(10),
            "10 秒内 CLI 与命令被清掉：cli={} cmd={}",
            alive(cli),
            alive(cmd)
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    drop(session);
}

fn pid_of(rig: &Rig, kind: &str) -> i32 {
    let v = rig
        .log()
        .into_iter()
        .find(|v| v["kind"] == kind)
        .unwrap_or_else(|| panic!("假 CLI 记了 {kind}：{:?}", rig.log()));
    v["pid"].as_i64().unwrap() as i32
}

/// 进程在跑（僵尸不算；读取出错让测试红）。
fn alive(pid: i32) -> bool {
    support::procs::alive(pid)
}

/// stderr 写满、没人读的时候会话任务 panic（审查 S6-5b 2/F-3）：panic 的处理不先同步写 stderr，桥接照样立刻结束。
#[tokio::test(flavor = "multi_thread")]
async fn panic_with_stderr_full_still_ends_the_bridge() {
    use support::procs::{Launch, Orphanable};
    let mut steps = opening();
    steps.extend([
        read_user(),
        init(),
        result_ok(),
        read_user(),
        text("m1", "PANIC-HERE"),
        json!({ "step": "log_signals_until_term" }),
    ]);
    let mut rig = Rig::new(Value::Array(steps));
    rig.launch
        .env
        .push(("IWORK_BRIDGE_FAULT_PANIC_ON".into(), "PANIC-HERE".into()));
    // 只留 error 级日志：stderr 填满之后，卡住桥接的只会是 panic 时的那次写
    rig.launch
        .env
        .push(("IWORK_BRIDGE_LOG".into(), "error".into()));
    let mut b = Orphanable::start_with(&rig, Launch::StderrFifo).await;
    let session = b.wire.open(&rig).await;
    assert!(b.wire.prompt(3, &session).await["result"].is_object());
    let cli = pid_of(&rig, "started");
    let _cleanup = support::procs::Leftover(vec![cli, b.bridge]);
    b.fill_stderr();
    // 第二轮：CLI 吐出标记，会话任务 panic
    b.wire
        .send(
            json!({ "jsonrpc": "2.0", "id": 4, "method": "session/prompt",
            "params": { "sessionId": session, "prompt": [{ "type": "text", "text": "again" }] } }),
        )
        .await;
    let started = Instant::now();
    while alive(b.bridge) {
        assert!(
            started.elapsed() < Duration::from_secs(3),
            "3 秒内桥接结束（panic 处理不等 stderr）"
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}
