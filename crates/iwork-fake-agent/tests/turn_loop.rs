//! 回合闭环：真 ACP 客户端（iwork-agent-acp）+ 回合编排（iwork-core）+ 假运行时进程。
//! 覆盖 T05 验收 1（完整回合，放行与拒绝）、2（取消与进程树清理）、3（中途崩溃）、5（不兼容阻止运行）。

mod support;

use std::sync::Arc;
use std::time::{Duration, Instant};

use iwork_agent_acp::AcpRuntime;
use iwork_core::turn::{STUB_DENY_MESSAGE, SessionLine, StubDecider, TurnPlan, TurnPorts};
use iwork_domain::turn::TurnState;
use serde_json::json;
use support::*;

/// 验收 1 用的脚本：分片输出正文、开一个工具调用、请求权限，按放行 / 拒绝走两条分支。
fn full_turn_script() -> serde_json::Value {
    json!({
        "turns": [{
            "steps": [
                { "type": "text", "text": "我来看看目录。", "chunk_chars": 3 },
                { "type": "tool_call", "id": "t1", "name": "Bash", "input": { "command": "ls" } },
                { "type": "permission", "tool_call_id": "t1",
                  "allowed": [
                      { "type": "tool_result", "id": "t1", "ok": true, "output": "README.md" },
                      { "type": "text", "text": "看完了。" }
                  ],
                  "denied": [
                      { "type": "tool_result", "id": "t1", "ok": false, "output": "被拒绝" },
                      { "type": "text", "text": "好的，不执行。" }
                  ] }
            ]
        }]
    })
}

struct Harness {
    store: MemStore,
    clock: RealClock,
    ids: CountingIds,
    live: Deltas,
}

impl Harness {
    fn new() -> Self {
        Self {
            store: MemStore::default(),
            clock: RealClock::default(),
            ids: CountingIds::default(),
            live: Deltas::default(),
        }
    }

    fn ports(&self) -> TurnPorts<'_, MemStore, RealClock, CountingIds, Deltas> {
        TurnPorts {
            store: &self.store,
            clock: &self.clock,
            ids: &self.ids,
            live: &self.live,
        }
    }

    fn plan(&self, cwd: &std::path::Path, prompt: &str) -> TurnPlan {
        TurnPlan {
            turn: queued_turn(&self.ids),
            assistant_name: "小助手".into(),
            context_project_id: None,
            spec: spec(cwd),
            blocks: text(prompt),
        }
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn full_turn_with_permission_allowed_matches_snapshot() {
    let fake = Fake::new(&full_turn_script());
    let runtime = AcpRuntime::new(fake.launch.clone());
    let h = Harness::new();
    let mut line = SessionLine::new(Arc::new(StubDecider::allow_all()), None);

    let state = line
        .run_turn(&runtime, h.ports(), h.plan(fake.dir.path(), "看看目录"))
        .await
        .unwrap();

    assert_eq!(state, TurnState::Completed);
    assert_eq!(
        h.store.snapshot(),
        [
            "turn Queued -> Running",
            "message appended Streaming",
            "tool started t1 Bash",
            "turn Running -> WaitingApproval",
            "turn WaitingApproval -> Running",
            "tool finished t1 ok=true",
            "turn Running -> Completed",
            "message Completed \"我来看看目录。看完了。\"",
            "turn finished Completed",
        ]
    );
    // 正文增量按片推出，偏移是 UTF-8 字节
    let deltas: Vec<_> = h
        .live
        .all()
        .iter()
        .map(|d| (d.offset, d.text.clone()))
        .collect();
    assert_eq!(
        deltas,
        [
            (0, "我来看".to_string()),
            (9, "看目录".to_string()),
            (18, "。".to_string()),
            (21, "看完了。".to_string()),
        ]
    );
    // 第一片之后就写了检查点
    assert_eq!(h.store.checkpoints()[0].2, "我来看");
    // 运行时收到的会话配置：系统提示走标准字段，其余在 _meta
    let new_session = &fake.params_of("session/new")[0];
    assert_eq!(
        new_session["systemPrompt"]["content"][0]["text"],
        "你是测试助手"
    );
    let setup =
        iwork_agent_acp::convert::decode_session_setup(new_session["_meta"].as_object()).unwrap();
    assert_eq!(setup.model.unwrap().model_id, "fake-model");
    // 会话线记下运行时会话 id，下次续接用
    assert_eq!(line.resume_id(), Some("fake-session-1"));
}

#[tokio::test(flavor = "multi_thread")]
async fn full_turn_with_permission_denied_matches_snapshot() {
    let fake = Fake::new(&full_turn_script());
    let runtime = AcpRuntime::new(fake.launch.clone());
    let h = Harness::new();
    let mut line = SessionLine::new(Arc::new(StubDecider::deny_all()), None);

    let state = line
        .run_turn(&runtime, h.ports(), h.plan(fake.dir.path(), "看看目录"))
        .await
        .unwrap();

    assert_eq!(state, TurnState::Completed);
    assert_eq!(
        h.store.snapshot(),
        [
            "turn Queued -> Running",
            "message appended Streaming",
            "tool started t1 Bash",
            "turn Running -> WaitingApproval",
            "turn WaitingApproval -> Running",
            "tool finished t1 ok=false",
            "turn Running -> Completed",
            "message Completed \"我来看看目录。好的，不执行。\"",
            "turn finished Completed",
        ]
    );
    // 运行时收到的是「拒绝一次」，理由放在 _meta.iwork.message
    let answers: Vec<_> = fake
        .records()
        .into_iter()
        .filter_map(|r| r.get("permission_response").cloned())
        .collect();
    assert_eq!(answers.len(), 1);
    assert_eq!(answers[0]["outcome"]["optionId"], "reject-once");
    assert_eq!(answers[0]["_meta"]["iwork"]["message"], STUB_DENY_MESSAGE);
}

#[tokio::test(flavor = "multi_thread")]
async fn stub_can_be_switched_between_turns() {
    let script = json!({ "turns": [
        full_turn_script()["turns"][0].clone(),
        full_turn_script()["turns"][0].clone(),
    ]});
    let fake = Fake::new(&script);
    let runtime = AcpRuntime::new(fake.launch.clone());
    let h = Harness::new();
    let stub = Arc::new(StubDecider::allow_all());
    let mut line = SessionLine::new(stub.clone(), None);

    line.run_turn(&runtime, h.ports(), h.plan(fake.dir.path(), "一"))
        .await
        .unwrap();
    stub.set_allow_all(false);
    line.run_turn(&runtime, h.ports(), h.plan(fake.dir.path(), "二"))
        .await
        .unwrap();

    let finished: Vec<_> = h
        .store
        .snapshot()
        .into_iter()
        .filter(|l| l.starts_with("tool finished"))
        .collect();
    assert_eq!(
        finished,
        ["tool finished t1 ok=true", "tool finished t1 ok=false"]
    );
    // 两个回合用的是同一条会话：只开了一次
    assert_eq!(fake.params_of("session/new").len(), 1);
    assert_eq!(fake.params_of("session/prompt").len(), 2);
}

/// 验收 2：运行中取消，运行时不理会取消（挂起）时 3 秒后结束整棵进程树，含它启动的子进程。
#[cfg(unix)]
#[tokio::test(flavor = "multi_thread")]
async fn cancel_kills_whole_process_tree_when_runtime_ignores_it() {
    let fake = Fake::new(&json!({ "turns": [{ "steps": [
        { "type": "spawn_child", "program": "sleep", "args": ["600"] },
        { "type": "text", "text": "开始" },
        { "type": "hang" }
    ]}]}));
    let runtime = AcpRuntime::new(fake.launch.clone());
    let h = Arc::new(Harness::new());
    let mut line = SessionLine::new(Arc::new(StubDecider::allow_all()), None);
    let canceller = line.canceller();
    let plan = h.plan(fake.dir.path(), "跑");
    let turn_id = plan.turn.id;

    let task = {
        let h = h.clone();
        tokio::spawn(async move {
            let state = line.run_turn(&runtime, h.ports(), plan).await.unwrap();
            (state, line)
        })
    };
    wait_until(Duration::from_secs(10), || !h.live.all().is_empty()).await;
    let child = fake
        .records()
        .iter()
        .find_map(|r| r["spawned_child"].as_u64())
        .expect("假运行时启动了子进程") as i32;
    assert!(process_exists(child), "对照：取消前子进程活着");

    let cancelled_at = Instant::now();
    canceller.cancel(turn_id);
    let (state, line) = task.await.unwrap();
    let turn_ended = cancelled_at.elapsed();

    assert_eq!(state, TurnState::Cancelled);
    let snapshot = h.store.snapshot();
    assert_eq!(
        &snapshot[snapshot.len() - 4..],
        [
            "turn Running -> Cancelling",
            "turn Cancelling -> Cancelled",
            "message Interrupted \"开始\"",
            "turn finished Cancelled",
        ]
    );
    let session = line.session().expect("取消后会话对象还在");
    wait_until(Duration::from_secs(2), || {
        session.process_tree_is_empty().unwrap()
    })
    .await;
    let tree_gone = cancelled_at.elapsed();
    assert!(!process_exists(child), "子进程随进程组一起结束");
    eprintln!("取消到回合结束 {turn_ended:?}，到进程树清空 {tree_gone:?}");
    assert!(
        turn_ended >= Duration::from_millis(2900),
        "挂起的运行时要等满 3 秒才强制结束"
    );
    assert!(tree_gone < Duration::from_secs(4), "{tree_gone:?}");
    assert_eq!(fake.params_of("session/cancel").len(), 1);
}

/// 验收 2 的对照：配合取消的运行时立即以 cancelled 结束，不用等 3 秒。
#[tokio::test(flavor = "multi_thread")]
async fn cooperative_cancel_ends_turn_quickly() {
    let fake = Fake::new(&json!({ "turns": [{ "steps": [
        { "type": "text", "text": "开始" },
        { "type": "sleep", "ms": 60000 }
    ]}]}));
    let runtime = AcpRuntime::new(fake.launch.clone());
    let h = Arc::new(Harness::new());
    let mut line = SessionLine::new(Arc::new(StubDecider::allow_all()), None);
    let canceller = line.canceller();
    let plan = h.plan(fake.dir.path(), "跑");
    let turn_id = plan.turn.id;
    let task = {
        let h = h.clone();
        tokio::spawn(async move { line.run_turn(&runtime, h.ports(), plan).await.unwrap() })
    };
    wait_until(Duration::from_secs(10), || !h.live.all().is_empty()).await;
    let cancelled_at = Instant::now();
    canceller.cancel(turn_id);
    assert_eq!(task.await.unwrap(), TurnState::Cancelled);
    assert!(cancelled_at.elapsed() < Duration::from_secs(2));
}

/// 验收 3：运行时中途崩溃，回合 Failed、消息 Failed；后台服务这一侧照常工作：
/// 同一条会话线下一回合按记下的会话 id 续接（`session/load`）并正常完成。
#[tokio::test(flavor = "multi_thread")]
async fn crash_mid_turn_fails_the_turn_and_next_turn_resumes() {
    let crashing = Fake::new(&json!({ "turns": [{ "steps": [
        { "type": "text", "text": "写到一半" },
        { "type": "crash", "exit_code": 3 }
    ]}]}));
    let healthy = Fake::new(&json!({ "turns": [{ "steps": [
        { "type": "text", "text": "续上了" }
    ]}]}));
    let h = Harness::new();
    let mut line = SessionLine::new(Arc::new(StubDecider::allow_all()), None);

    let state = line
        .run_turn(
            &AcpRuntime::new(crashing.launch.clone()),
            h.ports(),
            h.plan(crashing.dir.path(), "干活"),
        )
        .await
        .unwrap();
    assert_eq!(state, TurnState::Failed);
    let snapshot = h.store.snapshot();
    let failed = &snapshot[snapshot.len() - 3];
    assert!(
        failed.starts_with("turn Running -> Failed (运行时进程意外退出"),
        "{failed}"
    );
    assert!(failed.contains("exit status: 3"), "{failed}");
    assert_eq!(
        &snapshot[snapshot.len() - 2..],
        ["message Failed \"写到一半\"", "turn finished Failed"]
    );
    assert!(line.session().is_none(), "进程没了，会话随之丢弃");

    let state = line
        .run_turn(
            &AcpRuntime::new(healthy.launch.clone()),
            h.ports(),
            h.plan(healthy.dir.path(), "继续"),
        )
        .await
        .unwrap();
    assert_eq!(state, TurnState::Completed);
    let loads = healthy.params_of("session/load");
    assert_eq!(loads.len(), 1);
    assert_eq!(loads[0]["sessionId"], "fake-session-1");
    assert!(healthy.params_of("session/new").is_empty());
}

/// 验收 5：已知不兼容的组合在开会话时被阻止，回合以可读原因失败，没有发出任何 prompt。
#[tokio::test(flavor = "multi_thread")]
async fn incompatible_runtime_fails_turn_with_readable_reason() {
    let fake = Fake::new(&json!({
        "agent": { "name": "codex-acp", "version": "0.16.2" },
        "turns": [{ "steps": [{ "type": "text", "text": "不该出现" }] }]
    }));
    let h = Harness::new();
    let mut line = SessionLine::new(Arc::new(StubDecider::allow_all()), None);
    let state = line
        .run_turn(
            &AcpRuntime::new(fake.launch.clone()),
            h.ports(),
            h.plan(fake.dir.path(), "干活"),
        )
        .await
        .unwrap();
    assert_eq!(state, TurnState::Failed);
    assert_eq!(
        h.store.snapshot(),
        [
            "turn Queued -> Failed (运行时版本不兼容：codex-acp 0.16.2 是已知不兼容的版本：适配器内置旧内核，所有模型都不可用（t0-findings 第 3 节实测）)",
            "turn finished Failed",
        ]
    );
    assert!(fake.params_of("session/new").is_empty());
    assert!(fake.params_of("session/prompt").is_empty());
}

async fn wait_until(limit: Duration, mut ok: impl FnMut() -> bool) {
    let start = Instant::now();
    while !ok() {
        assert!(start.elapsed() < limit, "等了 {limit:?} 条件仍不成立");
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

#[cfg(unix)]
fn process_exists(pid: i32) -> bool {
    // 僵尸进程也算不存在：它已经结束，只等父进程回收
    let out = std::process::Command::new("ps")
        .args(["-o", "stat=", "-p", &pid.to_string()])
        .output()
        .expect("运行 ps");
    let stat = String::from_utf8_lossy(&out.stdout);
    let stat = stat.trim();
    !stat.is_empty() && !stat.starts_with('Z')
}
