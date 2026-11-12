//! 审批裁决的屏障测试里要连着 `iwork-agent-acp` 测的几条（T06 方案 15.3）：真实回合编排 + 真实 ACP 客户端
//! + 假运行时进程。线上的先后看假运行时的到达记录（读到那一行就记，早于派发）。
//!
//! 每条对两种判定方各跑一遍：参照判定方与真实审批引擎（S2 的合入门槛，T07 方案 6.10.8）。判定方、闸门、
//! 时钟与布景和 `iwork-core` 的屏障测试共用一份（`iwork-core/tests/support/judges.rs`）。
//!
//! 「`cancel()` 没有等满 GRACE_PERIOD」：编排一侧的定时器只由测试推进，从不推进别的时长；ACP 客户端
//! 的 GRACE_PERIOD 是真实时间，用「从放开最后一道闸门到回合结束的真实耗时远小于它」来证。

mod support;

#[path = "../../iwork-core/tests/support/mod.rs"]
mod core_support;

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use core_support::engine_fakes::MemStore;
use core_support::history::FakeHistory;
use core_support::judges::*;
use iwork_agent_acp::AcpRuntime;
use iwork_core::turn::{SessionLine, TurnCanceller, TurnEntry, TurnPlan, TurnPorts, TurnRunError};
use iwork_domain::id::TurnId;
use iwork_domain::turn::{InterruptReason, Turn, TurnState};
use iwork_ports::agent::{
    AgentError, AgentPort, PermissionDecision, PermissionHandler, PermissionRequest,
    RuntimeCapabilities, SessionSpec,
};
use serde_json::{Value, json};
use support::{CountingIds, Deltas, Fake};
use tokio::sync::Notify;

/// 假运行时声明的权限等待上限（秒）。
const LIMIT_SECS: u64 = 10;
/// ACP 客户端等权限任务、等 prompt 结束各自的上限（`iwork-agent-acp::process::GRACE_PERIOD`）。
const GRACE: Duration = Duration::from_secs(3);

// ---------- 包在处理方外面的闸门 ----------

/// 处理方（编排的 `PermissionGate`）返回之后、交还给适配器的权限任务之前，工具输入含标记的请求停在这里。
#[derive(Default)]
struct Hold {
    gates: Mutex<Vec<(String, Arc<Gate>)>>,
    /// 进过处理方的请求数（登记在第一次轮询里同步完成）
    entered: AtomicUsize,
}

impl Hold {
    fn gate(&self, marker: &str) -> Arc<Gate> {
        let gate = Arc::new(Gate::default());
        self.gates
            .lock()
            .unwrap()
            .push((marker.to_owned(), gate.clone()));
        gate
    }
}

struct Held<H> {
    inner: H,
    hold: Arc<Hold>,
}

impl<H: PermissionHandler> PermissionHandler for Held<H> {
    async fn on_permission(&self, req: PermissionRequest) -> PermissionDecision {
        self.hold.entered.fetch_add(1, Ordering::SeqCst);
        let gate = self
            .hold
            .gates
            .lock()
            .unwrap()
            .iter()
            .find(|(m, _)| req.raw_input.0.contains(m.as_str()))
            .map(|(_, g)| g.clone());
        let decision = self.inner.on_permission(req).await;
        if let Some(gate) = gate {
            gate.reached
                .store(true, std::sync::atomic::Ordering::SeqCst);
            gate.wait().await;
        }
        decision
    }
}

/// 真实的 ACP 运行时，开会话时把处理方包一层 [`Held`]。
struct HeldRuntime {
    inner: AcpRuntime,
    hold: Arc<Hold>,
}

impl AgentPort for HeldRuntime {
    type Session = <AcpRuntime as AgentPort>::Session;

    async fn capabilities(&self) -> Result<RuntimeCapabilities, AgentError> {
        self.inner.capabilities().await
    }

    async fn open_session<H: PermissionHandler + 'static>(
        &self,
        spec: SessionSpec,
        on_permission: H,
    ) -> Result<Self::Session, AgentError> {
        let held = Held {
            inner: on_permission,
            hold: self.hold.clone(),
        };
        self.inner.open_session(spec, held).await
    }
}

// ---------- 测试台 ----------

struct W<J: Judge> {
    store: MemStore,
    judge: J,
    decider: Arc<GateDecider<J::D>>,
    clock: ManualClock,
    timer: ManualTimer,
    ids: CountingIds,
    live: Deltas,
    fake: Fake,
    hold: Arc<Hold>,
}

type Task = tokio::task::JoinHandle<Result<TurnState, TurnRunError>>;

impl<J: Judge> W<J> {
    async fn new(turn: &Turn, steps: Value) -> Arc<Self> {
        let store = MemStore::default();
        seed_world(&store, &[turn]);
        let judge = J::make(&store).await;
        let decider = Arc::new(GateDecider::new(judge.decider()));
        let fake = Fake::new(&json!({
            "agent": { "permission_wait_limit_secs": LIMIT_SECS },
            "turns": [{ "steps": steps }]
        }));
        Arc::new(Self {
            store,
            judge,
            decider,
            clock: ManualClock::default(),
            timer: ManualTimer::default(),
            ids: CountingIds::default(),
            live: Deltas::default(),
            fake,
            hold: Arc::new(Hold::default()),
        })
    }

    fn start(self: &Arc<Self>, turn: Turn) -> (Task, TurnCanceller) {
        let mut line = SessionLine::new(self.decider.clone(), None);
        let canceller = line.canceller();
        let plan = TurnPlan {
            changes: None,
            entry: TurnEntry::New,
            turn,
            assistant_name: "小助手".into(),
            context_project_id: None,
            spec: session_spec(self.fake.dir.path().to_owned()),
            blocks: support::text("开始"),
        };
        let w = self.clone();
        let task = tokio::spawn(async move {
            let runtime = HeldRuntime {
                inner: AcpRuntime::new(w.fake.launch.clone()),
                hold: w.hold.clone(),
            };
            let history = FakeHistory::default();
            let ports = TurnPorts {
                store: &w.store,
                clock: &w.clock,
                ids: &w.ids,
                live: &w.live,
                timer: &w.timer,
                history: &history,
            };
            line.run_turn(&runtime, ports, plan).await
        });
        (task, canceller)
    }

    fn state(&self, turn: TurnId) -> Option<TurnState> {
        state_of(&self.store, turn)
    }

    async fn approval(&self, marker: &str) -> iwork_domain::id::ApprovalId {
        let mut id = None;
        wait_until("审批已请求", || {
            id = approval_for(&self.store, marker);
            id.is_some()
        })
        .await;
        id.unwrap()
    }

    fn arrivals(&self) -> Vec<String> {
        self.fake.arrivals()
    }

    /// 线上 `a` 在 `b` 之前到达。
    fn before(&self, a: &str, b: &str) {
        let arrivals = self.arrivals();
        let pos = |x: &str| {
            arrivals
                .iter()
                .position(|y| y == x)
                .unwrap_or_else(|| panic!("没到达：{x}；{arrivals:?}"))
        };
        assert!(pos(a) < pos(b), "{a} 应当先于 {b}：{arrivals:?}");
    }
}

fn wait_limit() -> TurnState {
    TurnState::Interrupted {
        reason: InterruptReason::ApprovalWaitLimit,
    }
}

/// 两次工具调用 A、B 同时请求权限，然后一直等到被取消。
fn two_asks() -> Value {
    json!([
        { "type": "tool_call", "id": "A", "name": "Bash", "input": { "command": "npm test" } },
        { "type": "tool_call", "id": "B", "name": "Bash", "input": { "command": "npm run lint" } },
        { "type": "parallel_permissions", "tool_call_ids": ["A", "B"] },
        { "type": "sleep", "ms": 600000 }
    ])
}

macro_rules! both {
    ($($name:ident),* $(,)?) => {$(
        mod $name {
            #[tokio::test(flavor = "multi_thread")]
            async fn reference() {
                super::$name::<super::Reference>().await
            }
            #[tokio::test(flavor = "multi_thread")]
            async fn engine() {
                super::$name::<super::RealEngine>().await
            }
        }
    )*};
}

both!(
    w3_ack_sent_before_adapter_polls,
    decision_during_interrupt_commit_does_not_stall_cancel,
    ask_before_boundary_is_rejected_on_stop,
    stop_right_after_sender_registered,
);

/// W3（2/F-1）：A 的回执 `true` 已经发出，包在外面的闸门先不把结果交还给适配器的权限任务；这时 B 超期，
/// 编排提交中断、停止受理、调 `cancel()`。闸门放开之前没有 `session/cancel`；运行时收到 A 的允许，
/// 排在 `session/cancel` 之前；B 收到拒绝。
async fn w3_ack_sent_before_adapter_polls<J: Judge>() {
    let turn = queued_turn(1);
    let t = turn.id;
    let w = W::<J>::new(&turn, two_asks()).await;
    let a_gate = w.hold.gate("npm test");
    let (task, _) = w.start(turn);
    let id_a = w.approval("npm test").await;
    let id_b = w.approval("npm run lint").await;
    w.judge.user(id_a, true).await;
    wait_until("A 的回执已发、结果停在闸门前", || {
        a_gate.reached.load(Ordering::SeqCst)
    })
    .await;
    w.clock.advance_mono(Duration::from_secs(LIMIT_SECS));
    wait_until("回合因等待上限中断", || {
        w.timer.tick();
        w.state(t) == Some(wait_limit())
    })
    .await;
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert!(
        !w.arrivals().iter().any(|a| a == "session/cancel"),
        "{}：A 的答复写出去之前不发取消：{:?}",
        J::NAME,
        w.arrivals()
    );
    let released = Instant::now();
    a_gate.open();
    assert_eq!(task.await.unwrap().unwrap(), wait_limit());
    assert!(released.elapsed() < GRACE, "{:?}", released.elapsed());
    w.before("permission_response A allow-once", "session/cancel");
    w.before("permission_response B reject-once", "session/cancel");
    let trail = trail(&w.store, t);
    assert!(trail.contains(&"delivered A".to_owned()), "{trail:?}");
    assert!(!w.judge.has_grant(id_a).await, "{}：A 用掉了", J::NAME);
    assert!(still_pending(&w.store, id_b));
}

/// 3/F-1：A、B 都在等。闸门卡在「提交中断」那一笔上；这时 B 的判定方返回、结论交进表里等回执；放开闸门。
/// B 收到拒绝、排在 `session/cancel` 之前；`cancel()` 没有等满上限；没有 B 的交付；B 是允许，留一份授权。
async fn decision_during_interrupt_commit_does_not_stall_cancel<J: Judge>() {
    let turn = queued_turn(1);
    let t = turn.id;
    let w = W::<J>::new(&turn, two_asks()).await;
    let b_gate = w.decider.gate("npm run lint");
    let (task, _) = w.start(turn);
    w.approval("npm test").await;
    let id_b = w.approval("npm run lint").await;
    w.judge.user(id_b, true).await;
    wait_until("B 的结论停在判定方返回路上", || {
        b_gate.reached.load(Ordering::SeqCst)
    })
    .await;

    let commit_gate = Arc::new(Notify::new());
    *w.store.before_commit.lock().unwrap() = Some(commit_gate.clone());
    w.clock.advance_mono(Duration::from_secs(LIMIT_SECS));
    wait_until("编排停在提交中断那一笔上", || {
        w.timer.tick();
        w.store.commit_entered.load(Ordering::SeqCst)
    })
    .await;
    b_gate.open();
    // B 离开判定方、交结论、等回执：都在这一次轮询里
    tokio::time::sleep(Duration::from_millis(100)).await;
    let released = Instant::now();
    commit_gate.notify_one();
    assert_eq!(task.await.unwrap().unwrap(), wait_limit());
    assert!(released.elapsed() < GRACE, "{:?}", released.elapsed());
    w.before("permission_response B reject-once", "session/cancel");
    w.before("permission_response A reject-once", "session/cancel");
    let trail = trail(&w.store, t);
    assert!(
        !trail.iter().any(|e| e.starts_with("delivered")),
        "{trail:?}"
    );
    assert!(w.judge.has_grant(id_b).await, "{}：B 留一份授权", J::NAME);
}

/// 4/F-1：请求卡在前序边界之前（编排还没把它之前的更新处理完），这时取消就绪。运行时收到这个请求的拒绝，
/// 排在 `session/cancel` 之前；没有为它提交「等待审批」。
async fn ask_before_boundary_is_rejected_on_stop<J: Judge>() {
    let turn = queued_turn(1);
    let t = turn.id;
    let w = W::<J>::new(
        &turn,
        json!([
            { "type": "sleep", "ms": 300 },
            { "type": "tool_call", "id": "t1", "name": "Bash", "input": { "command": "npm test" } },
            { "type": "text", "text": "x" },
            { "type": "permission", "tool_call_id": "t1" },
            { "type": "sleep", "ms": 600000 }
        ]),
    )
    .await;
    let (task, canceller) = w.start(turn);
    wait_until("回合在跑", || w.state(t) == Some(TurnState::Running)).await;
    // 编排处理「工具调用开始」那一笔时停住：它后面的正文与权限请求的边界都没到
    let commit_gate = Arc::new(Notify::new());
    *w.store.before_commit.lock().unwrap() = Some(commit_gate.clone());
    wait_until("权限请求进了处理方", || {
        w.hold.entered.load(Ordering::SeqCst) == 1
    })
    .await;
    wait_until("编排停在提交上", || {
        w.store.commit_entered.load(Ordering::SeqCst)
    })
    .await;
    canceller.cancel(t);
    let released = Instant::now();
    commit_gate.notify_one();
    assert_eq!(task.await.unwrap().unwrap(), TurnState::Cancelled);
    assert!(released.elapsed() < GRACE, "{:?}", released.elapsed());
    w.before("permission_response t1 reject-once", "session/cancel");
    let trail = trail(&w.store, t);
    assert!(
        !trail.iter().any(|e| e.contains("WaitingApproval")),
        "{}：{trail:?}",
        J::NAME
    );
}

/// 5/F-1 屏障 b 的连着测：请求刚登记完（等受理）、刚交完结论（等回执）时停止受理，`cancel()` 是真的
/// `AcpSession::cancel()`。两种情形下拒绝都排在 `session/cancel` 之前，`cancel()` 没有等满上限。
async fn stop_right_after_sender_registered<J: Judge>() {
    // 刚登记完：编排正卡在一笔提交上，请求进来登记、没人受理；这时取消
    {
        let turn = queued_turn(1);
        let t = turn.id;
        let w = W::<J>::new(
            &turn,
            json!([
                { "type": "sleep", "ms": 300 },
                { "type": "tool_call", "id": "t1", "name": "Bash", "input": { "command": "npm test" } },
                { "type": "permission", "tool_call_id": "t1" },
                { "type": "sleep", "ms": 600000 }
            ]),
        )
        .await;
        let (task, canceller) = w.start(turn);
        wait_until("回合在跑", || w.state(t) == Some(TurnState::Running)).await;
        let commit_gate = Arc::new(Notify::new());
        *w.store.before_commit.lock().unwrap() = Some(commit_gate.clone());
        wait_until("请求已登记", || {
            w.hold.entered.load(Ordering::SeqCst) == 1
        })
        .await;
        wait_until("编排停在提交上", || {
            w.store.commit_entered.load(Ordering::SeqCst)
        })
        .await;
        canceller.cancel(t);
        let released = Instant::now();
        commit_gate.notify_one();
        assert_eq!(task.await.unwrap().unwrap(), TurnState::Cancelled);
        assert!(released.elapsed() < GRACE, "{:?}", released.elapsed());
        w.before("permission_response t1 reject-once", "session/cancel");
    }
    // 刚交完结论：编排正卡在「取消」那一笔提交上，判定方这时放出结论、交进表里等回执
    {
        let turn = queued_turn(1);
        let t = turn.id;
        let w = W::<J>::new(
            &turn,
            json!([
                { "type": "tool_call", "id": "t1", "name": "Bash", "input": { "command": "npm test" } },
                { "type": "permission", "tool_call_id": "t1" },
                { "type": "sleep", "ms": 600000 }
            ]),
        )
        .await;
        let gate = w.decider.gate("npm test");
        let (task, canceller) = w.start(turn);
        let id = w.approval("npm test").await;
        w.judge.user(id, true).await;
        wait_until("结论停在判定方返回路上", || {
            gate.reached.load(Ordering::SeqCst)
        })
        .await;
        let commit_gate = Arc::new(Notify::new());
        *w.store.before_commit.lock().unwrap() = Some(commit_gate.clone());
        canceller.cancel(t);
        wait_until("编排停在提交取消上", || {
            w.store.commit_entered.load(Ordering::SeqCst)
        })
        .await;
        gate.open();
        tokio::time::sleep(Duration::from_millis(100)).await;
        let released = Instant::now();
        commit_gate.notify_one();
        assert_eq!(task.await.unwrap().unwrap(), TurnState::Cancelled);
        assert!(released.elapsed() < GRACE, "{:?}", released.elapsed());
        w.before("permission_response t1 reject-once", "session/cancel");
        assert!(
            !trail(&w.store, t)
                .iter()
                .any(|e| e.starts_with("delivered")),
            "{}",
            J::NAME
        );
    }
}
