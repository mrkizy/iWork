//! 回合闭环：真 ACP 客户端（iwork-agent-acp）+ 回合编排（iwork-core）+ 假运行时进程。
//! 覆盖 T05 验收 1（完整回合，放行与拒绝）、2（取消与进程树清理）、3（中途崩溃）、5（不兼容阻止运行）。

mod support;

#[path = "../../iwork-core/tests/support/history.rs"]
#[allow(dead_code, reason = "这里只用默认值")]
mod history;

use std::sync::Arc;
use std::time::{Duration, Instant};

use history::FakeHistory;
use iwork_agent_acp::AcpRuntime;
use iwork_core::turn::{
    STUB_DENY_MESSAGE, SessionLine, StubDecider, TurnEntry, TurnPlan, TurnPorts,
};
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
    timer: TokioTimer,
    history: FakeHistory,
}

impl Harness {
    fn new() -> Self {
        Self {
            store: MemStore::default(),
            clock: RealClock::default(),
            ids: CountingIds::default(),
            live: Deltas::default(),
            timer: TokioTimer,
            history: FakeHistory::default(),
        }
    }

    fn ports(
        &self,
    ) -> TurnPorts<'_, MemStore, RealClock, CountingIds, Deltas, TokioTimer, FakeHistory> {
        TurnPorts {
            store: &self.store,
            clock: &self.clock,
            ids: &self.ids,
            live: &self.live,
            timer: &self.timer,
            history: &self.history,
        }
    }

    fn plan(&self, cwd: &std::path::Path, prompt: &str) -> TurnPlan {
        TurnPlan {
            entry: TurnEntry::New,
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
            // 会话绑定与进 Running 同一次提交，在发 prompt 之前
            "session bound fake-session-1",
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
            // 会话绑定与进 Running 同一次提交，在发 prompt 之前
            "session bound fake-session-1",
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
/// Unix 查进程组，Windows 查 Job（Windows 只在 CI 上跑）。
#[tokio::test(flavor = "multi_thread")]
async fn cancel_kills_whole_process_tree_when_runtime_ignores_it() {
    let fake = Fake::new(&json!({ "turns": [{ "steps": [
        child_step(),
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
        .expect("假运行时启动了子进程") as u32;
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
    // 子进程随进程组一起结束。Windows 上 Job 活动进程数归零后，进程还要过一会儿才从
    // tasklist 里消失，所以等它消失而不是只查一次。
    wait_until(Duration::from_secs(5), || !process_exists(child)).await;
    eprintln!("取消到回合结束 {turn_ended:?}，到进程树清空 {tree_gone:?}");
    assert!(
        turn_ended >= Duration::from_millis(2900),
        "挂起的运行时要等满 3 秒才强制结束"
    );
    assert!(tree_gone < Duration::from_secs(4), "{tree_gone:?}");
    assert_eq!(fake.params_of("session/cancel").len(), 1);
}

/// 验收 2（F-2）：运行时配合取消、很快答 cancelled，但它起的子进程还在跑：取消照样结束整棵树，
/// 回合 Cancelled 时子进程已不在；会话随之失效，同一条会话线下一回合按会话 id 续接。
#[tokio::test(flavor = "multi_thread")]
async fn cooperative_cancel_still_clears_children_and_next_turn_resumes() {
    bounded(async {
        let fake = Fake::new(&json!({ "turns": [{ "steps": [
            child_step(),
            { "type": "text", "text": "开始" },
            { "type": "sleep", "ms": 60000 }
        ]}]}));
        let healthy =
            Fake::new(&json!({ "turns": [{ "steps": [{ "type": "text", "text": "续上了" }] }] }));
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
            .expect("假运行时启动了子进程") as u32;
        let cancelled_at = Instant::now();
        canceller.cancel(turn_id);
        let (state, mut line) = task.await.unwrap();

        assert_eq!(state, TurnState::Cancelled);
        assert!(
            cancelled_at.elapsed() < Duration::from_secs(2),
            "配合取消不用等满 3 秒"
        );
        // 回合标为 Cancelled 时子进程已被结束；Windows 上它从 tasklist 消失有延迟，等它消失。
        wait_until(Duration::from_secs(5), || !process_exists(child)).await;
        let session = line.session().expect("会话对象还在");
        assert!(session.process_tree_is_empty().unwrap());

        let state = line
            .run_turn(
                &AcpRuntime::new(healthy.launch.clone()),
                h.ports(),
                // 同一条会话线的工作目录不变（会话绑定项守卫）；换的只是运行时
                h.plan(fake.dir.path(), "继续"),
            )
            .await
            .unwrap();
        assert_eq!(state, TurnState::Completed);
        assert_eq!(
            healthy.params_of("session/load")[0]["sessionId"],
            "fake-session-1"
        );
    })
    .await;
}

/// F-3：运行时崩溃，但它起的子进程继承了 stdout、还拿着管道：读端收不到 EOF，
/// 也要凭进程退出及时发现，回合 Failed，整棵树清空。
#[tokio::test(flavor = "multi_thread")]
async fn crash_with_child_holding_stdout_is_detected() {
    bounded(async {
        let fake = Fake::new(&json!({ "turns": [{ "steps": [
            child_step(),
            { "type": "text", "text": "写到一半" },
            { "type": "crash", "exit_code": 7 }
        ]}]}));
        let h = Harness::new();
        let mut line = SessionLine::new(Arc::new(StubDecider::allow_all()), None);
        let started = Instant::now();
        let state = tokio::time::timeout(
            Duration::from_secs(10),
            line.run_turn(
                &AcpRuntime::new(fake.launch.clone()),
                h.ports(),
                h.plan(fake.dir.path(), "干活"),
            ),
        )
        .await
        .expect("10 秒内必须发现崩溃")
        .unwrap();
        assert_eq!(state, TurnState::Failed);
        assert!(
            started.elapsed() < Duration::from_secs(5),
            "{:?}",
            started.elapsed()
        );
        let snapshot = h.store.snapshot();
        let failed = &snapshot[snapshot.len() - 3];
        assert!(failed.contains("运行时进程意外退出"), "{failed}");
        let child = fake
            .records()
            .iter()
            .find_map(|r| r["spawned_child"].as_u64())
            .expect("假运行时启动了子进程") as u32;
        wait_until(Duration::from_secs(2), || !process_exists(child)).await;
    })
    .await;
}

/// 第 2 轮 F-1：真适配器里，权限请求之前积压了几千段文字，「工具调用开始」照样排在「等待审批」之前。
#[tokio::test(flavor = "multi_thread")]
async fn tool_start_precedes_waiting_behind_a_real_backlog() {
    let fake = Fake::new(&json!({ "turns": [{ "steps": [
        { "type": "repeat_text", "chunk": "x", "count": 3000 },
        { "type": "tool_call", "id": "t1", "name": "Bash", "input": { "command": "ls" } },
        { "type": "permission", "tool_call_id": "t1",
          "allowed": [ { "type": "tool_result", "id": "t1", "ok": true } ] }
    ]}]}));
    let h = Harness::new();
    let mut line = SessionLine::new(Arc::new(StubDecider::allow_all()), None);
    let state = line
        .run_turn(
            &AcpRuntime::new(fake.launch.clone()),
            h.ports(),
            h.plan(fake.dir.path(), "跑"),
        )
        .await
        .unwrap();
    assert_eq!(state, TurnState::Completed);
    let order: Vec<String> = h
        .store
        .snapshot()
        .into_iter()
        .filter(|l| l.starts_with("tool started") || l.contains("-> WaitingApproval"))
        .collect();
    assert_eq!(
        order,
        ["tool started t1 Bash", "turn Running -> WaitingApproval"]
    );
    assert_eq!(h.live.all().len(), 3000);
}

/// 第 2 轮 F-3：运行时发来超长行或非 UTF-8 行：报固定原因、回合 Failed、进程树清空。
#[tokio::test(flavor = "multi_thread")]
async fn bad_frames_fail_the_turn_with_fixed_reason_and_clear_the_tree() {
    bounded(async {
        let cases = [
            (
                json!({ "type": "bad_line", "kind": "oversize", "bytes": iwork_agent_acp::MAX_FRAME_BYTES + 1 }),
                format!(
                    "运行时发来的单条消息超过 {} 字节",
                    iwork_agent_acp::MAX_FRAME_BYTES
                ),
            ),
            (
                json!({ "type": "bad_line", "kind": "invalid_utf8" }),
                "运行时发来的消息不是 UTF-8".to_string(),
            ),
        ];
        for (step, reason) in cases {
            let fake = Fake::new(&json!({ "turns": [{ "steps": [
                { "type": "text", "text": "前面正常" },
                step
            ]}]}));
            let h = Harness::new();
            let mut line = SessionLine::new(Arc::new(StubDecider::allow_all()), None);
            let state = tokio::time::timeout(
                Duration::from_secs(10),
                line.run_turn(
                    &AcpRuntime::new(fake.launch.clone()),
                    h.ports(),
                    h.plan(fake.dir.path(), "跑"),
                ),
            )
            .await
            .expect("10 秒内收尾")
            .unwrap();
            assert_eq!(state, TurnState::Failed);
            let snapshot = h.store.snapshot();
            assert_eq!(
                snapshot[snapshot.len() - 3],
                format!("turn Running -> Failed (与运行时通信出错：{reason})")
            );
            let session = line.session().expect("协议错误不丢会话对象");
            wait_until(Duration::from_secs(3), || {
                session.process_tree_is_empty().unwrap()
            })
            .await;
            assert!(
                session
                    .raw_log()
                    .iter()
                    .any(|l| l.contains("\"rejected\":true")),
                "诊断里记了一条被拒收的消息"
            );
        }
    })
    .await;
}

/// F-1：真实会话的诊断记录里没有正文、工具输入输出、stderr 内容，只有协议元数据。
#[tokio::test(flavor = "multi_thread")]
async fn diagnostics_of_a_real_session_carry_no_content() {
    let fake = Fake::new(&json!({ "turns": [{ "steps": [
        { "type": "stderr", "text": "TOKEN=stderr-secret-1" },
        { "type": "text", "text": "正文里的秘密 body-secret-2" },
        { "type": "tool_call", "id": "t1", "name": "Bash",
          "input": { "command": "curl -H 'Authorization: input-secret-3'", "env": { "CUSTOM_AUTH": "env-secret-4" } } },
        { "type": "permission", "tool_call_id": "t1",
          "allowed": [ { "type": "tool_result", "id": "t1", "ok": true, "output": "output-secret-5" } ] }
    ]}]}));
    let session = iwork_ports::agent::AgentPort::open_session(
        &AcpRuntime::new(fake.launch.clone()),
        spec(fake.dir.path()),
        AllowAll,
    )
    .await
    .unwrap();
    let mut updates = iwork_ports::agent::AgentSession::prompt(&session, text("hi"))
        .await
        .unwrap();
    while iwork_ports::agent::UpdateStream::next(&mut updates)
        .await
        .is_some()
    {}
    // stderr 由另一个任务读，给它一点时间
    tokio::time::sleep(Duration::from_millis(200)).await;
    let log = session.raw_log().join("\n");
    for secret in [
        "stderr-secret-1",
        "body-secret-2",
        "input-secret-3",
        "env-secret-4",
        "output-secret-5",
        "CUSTOM_AUTH",
    ] {
        assert!(!log.contains(secret), "{secret} 进了诊断：{log}");
    }
    // 对照：元数据确实记下了
    assert!(log.contains("agent_message_chunk"), "{log}");
    assert!(log.contains("session/request_permission"), "{log}");
    assert!(log.contains("\"stream\":\"stderr\""), "{log}");
}

struct AllowAll;

impl iwork_ports::agent::PermissionHandler for AllowAll {
    async fn on_permission(
        &self,
        _req: iwork_ports::agent::PermissionRequest,
    ) -> iwork_ports::agent::PermissionDecision {
        iwork_ports::agent::PermissionDecision::Allow
    }
}

/// 验收 3：运行时中途崩溃，回合 Failed、消息 Failed；后台服务这一侧照常工作：
/// 同一条会话线下一回合按记下的会话 id 续接（`session/load`）并正常完成。
#[tokio::test(flavor = "multi_thread")]
async fn crash_mid_turn_fails_the_turn_and_next_turn_resumes() {
    bounded(async {
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
        assert!(failed.contains(&exit_code_text(3)), "{failed}");
        assert_eq!(
            &snapshot[snapshot.len() - 2..],
            ["message Failed \"写到一半\"", "turn finished Failed"]
        );
        assert!(line.session().is_none(), "进程没了，会话随之丢弃");

        let state = line
            .run_turn(
                &AcpRuntime::new(healthy.launch.clone()),
                h.ports(),
                // 同一条会话线的工作目录不变（会话绑定项守卫）；换的只是运行时
                h.plan(crashing.dir.path(), "继续"),
            )
            .await
            .unwrap();
        assert_eq!(state, TurnState::Completed);
        let loads = healthy.params_of("session/load");
        assert_eq!(loads.len(), 1);
        assert_eq!(loads[0]["sessionId"], "fake-session-1");
        assert!(healthy.params_of("session/new").is_empty());
    })
    .await;
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

/// 会走 retire / terminate / close 的用例的整体期限。正常几秒跑完；取消、清树各自的上限加起来也远小于它。
const CASE_LIMIT: Duration = Duration::from_secs(90);

/// 给用例套一层期限，打开 debug 日志（测试捕获，失败时才打出来）。超时就 panic：
/// retire、terminate、kill_tree_and_wait、close、shutdown 的入口和出口都记了日志，
/// 最后一条只有 enter 没有 exit 的就是卡住的那一步（2026-11-11 Windows CI 卡过一次）。
async fn bounded(case: impl std::future::Future<Output = ()>) {
    let _ = tracing_subscriber::fmt()
        .with_max_level(tracing::Level::DEBUG)
        .with_test_writer()
        .try_init();
    if tokio::time::timeout(CASE_LIMIT, case).await.is_err() {
        panic!("用例 {CASE_LIMIT:?} 没跑完：看上面日志里最后一条只有 enter、没有 exit 的步骤");
    }
}

async fn wait_until(limit: Duration, mut ok: impl FnMut() -> bool) {
    let start = Instant::now();
    while !ok() {
        assert!(start.elapsed() < limit, "等了 {limit:?} 条件仍不成立");
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

/// 退出码在退出说明里的写法（标准库 `ExitStatus` 的 Display，两个平台不同）。
fn exit_code_text(code: i32) -> String {
    if cfg!(windows) {
        format!("exit code: {code}")
    } else {
        format!("exit status: {code}")
    }
}

/// 一个长时间运行、**不往 stdout 写任何东西**的子进程步骤：子进程继承运行时的 stdout，
/// 写一个字都会混进 ACP 管道（Windows 上的 ping 就会）。
fn child_step() -> serde_json::Value {
    if cfg!(windows) {
        json!({ "type": "spawn_child", "program": "powershell",
                "args": ["-NoProfile", "-NonInteractive", "-Command", "Start-Sleep -Seconds 600"] })
    } else {
        json!({ "type": "spawn_child", "program": "sleep", "args": ["600"] })
    }
}

#[cfg(unix)]
fn process_exists(pid: u32) -> bool {
    // 僵尸进程也算不存在：它已经结束，只等父进程回收
    let out = std::process::Command::new("ps")
        .args(["-o", "stat=", "-p", &pid.to_string()])
        .output()
        .expect("运行 ps");
    let stat = String::from_utf8_lossy(&out.stdout);
    let stat = stat.trim();
    !stat.is_empty() && !stat.starts_with('Z')
}

#[cfg(windows)]
fn process_exists(pid: u32) -> bool {
    let out = std::process::Command::new("tasklist")
        .args(["/FI", &format!("PID eq {pid}"), "/NH", "/FO", "CSV"])
        .output()
        .expect("运行 tasklist");
    String::from_utf8_lossy(&out.stdout).contains(&format!("\"{pid}\""))
}

/// 运行时上报的模型、额度进事件，用量记在回合结束的统计里；额度的观测时间由编排按自己的时钟
/// 记（上报里不带时间）。走的是真 ACP 客户端 + 回合编排 + 假运行时进程。
#[tokio::test(flavor = "multi_thread")]
async fn runtime_reports_become_events_with_orchestrator_time() {
    use iwork_domain::event::DomainEvent;
    let fake = Fake::new(&json!({ "turns": [{ "steps": [
        { "type": "report_model", "model_id": "claude-opus", "effort": "high" },
        { "type": "text", "text": "好" },
        { "type": "report_quota", "windows": [
            { "window": "five_hour", "used_pct": 42, "resets_at_secs": 1731300000 }
        ] },
        { "type": "report_usage", "input_tokens": 10, "output_tokens": 20 }
    ] }] }));
    let runtime = AcpRuntime::new(fake.launch.clone());
    let h = Harness::new();
    let mut line = SessionLine::new(Arc::new(StubDecider::allow_all()), None);
    let state = line
        .run_turn(&runtime, h.ports(), h.plan(fake.dir.path(), "hi"))
        .await
        .unwrap();
    assert_eq!(state, TurnState::Completed);

    let events = h.store.events();
    let model = events.iter().find_map(|e| match e {
        DomainEvent::TurnModelReported(m) => Some(m.model.model_id.0.clone()),
        _ => None,
    });
    assert_eq!(model.as_deref(), Some("claude-opus"));
    let quota = events
        .iter()
        .find_map(|e| match e {
            DomainEvent::QuotaObserved(q) => Some(q.clone()),
            _ => None,
        })
        .expect("额度进了事件");
    assert_eq!(quota.windows.len(), 1);
    let w = &quota.windows[0];
    assert_eq!((w.window.as_str(), w.used_pct), ("five_hour", 42));
    assert_eq!(
        w.resets_at,
        Some(iwork_domain::common::Timestamp(1_731_300_000_000))
    );
    assert_eq!(
        w.observed_at, quota.observed_at,
        "窗口的观测时间就是编排记的那一刻"
    );
    assert!(quota.observed_at.0 >= 1_700_000_000_000, "取自编排的时钟");
    let usage = events.iter().find_map(|e| match e {
        DomainEvent::UsageReported(u) => Some(u.usage),
        _ => None,
    });
    assert_eq!(
        usage.map(|u| (u.input_tokens, u.output_tokens)),
        Some((10, 20))
    );
}
