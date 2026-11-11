//! 审批裁决的屏障测试（T06 方案 15.3）：W1–W4 四个窗口，外加等待上限、续接入口、会话绑定项守卫。
//!
//! 屏障 = 测试自己控制的闸门：判定方返回路上的闸门（`GateDecider`）、只由测试推进的定时器与时钟、
//! 假运行时清进程树那一步的闸门。用次序而不是用时间把执行逼进指定的窗口。每条断言三样：提交的事件序列、
//! 给运行时的答复、判定方最后有没有留授权。
//!
//! 每条用例对两种判定方各跑一遍：参照判定方（只按已提交事件推进 R1–R3）与真实审批引擎（T07）。
//! 后者是 S2 的合入门槛（T07 方案 6.10.8）。
//!
//! 假运行时的 `cancel()` 照 `iwork-agent-acp` 的约定写：等名下的权限任务都把答复写出去，再记
//! `session/cancel`。它不设上限（真适配器的上限是 GRACE_PERIOD）：编排不守「先停止受理」的约定时，
//! 用例会卡住、由 `wait_until` 的 5 秒报出来，而不是悄悄等满一个期限。

use std::future::Future;
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use iwork_core::turn::{
    ResumeReason, SessionLine, TREE_NOT_CLEARED, TURN_GONE_MESSAGE, TurnPlan, TurnPorts,
    TurnRunError, resume_note,
};
use iwork_domain::action::ToolRef;
use iwork_domain::approval::ApprovalState;
use iwork_domain::common::{ModelId, ModelSelection};
use iwork_domain::event::DomainEvent;
use iwork_domain::id::{RawUlid, TurnId};
use iwork_domain::message::MessageStatus;
use iwork_domain::turn::{InterruptReason, Turn, TurnState};
use iwork_ports::agent::{
    AgentError, AgentPort, AgentSession, AgentUpdate, ContentBlock, PermissionDecision,
    PermissionHandler, PermissionRequest, RawToolInput, RuntimeCapabilities, SandboxStatus,
    SessionSpec, TurnOutcome, UpdateStream,
};
use iwork_ports::clock::IdGenerator;
use iwork_ports::live::{LivePublisher, MessageDelta};
use tokio::sync::{Notify, mpsc, watch};

mod support;

use support::engine_fakes::MemStore;
use support::judges::*;

/// 假运行时声明的权限等待上限。
const LIMIT: Duration = Duration::from_secs(10);

// ---------- 假运行时 ----------

type Handler = Arc<
    dyn Fn(PermissionRequest) -> Pin<Box<dyn Future<Output = PermissionDecision> + Send>>
        + Send
        + Sync,
>;

/// 清进程树那一步怎么答。
#[derive(Clone)]
enum Terminate {
    Ok,
    Fail,
    /// 等这个闸门打开再答 `Ok`
    Gated(Arc<Notify>),
}

struct Ctl {
    handler: Mutex<Option<Handler>>,
    updates: Mutex<Option<mpsc::UnboundedSender<AgentUpdate>>>,
    /// 本回合已发出的更新条数（权限请求的前序边界）
    sent: AtomicU64,
    /// 线上的先后：权限答复（`A allow` / `A deny`）与 `session/cancel`
    wire: Mutex<Vec<String>>,
    /// 还没把答复写出去的权限任务
    in_flight: watch::Sender<usize>,
    /// `cancel()` 之后是否以「已取消」结束这一轮；为假时扮演「这一轮已经自己结束」
    cancel_ends_turn: AtomicBool,
    limit: Mutex<Option<Duration>>,
    terminate: Mutex<Terminate>,
    terminates: AtomicUsize,
    /// 进过 `terminate()`、还没返回
    terminating: AtomicBool,
    opens: AtomicUsize,
    specs: Mutex<Vec<SessionSpec>>,
    prompts: AtomicUsize,
}

impl Default for Ctl {
    fn default() -> Self {
        Self {
            handler: Mutex::default(),
            updates: Mutex::default(),
            sent: AtomicU64::new(0),
            wire: Mutex::default(),
            in_flight: watch::channel(0).0,
            cancel_ends_turn: AtomicBool::new(true),
            limit: Mutex::new(Some(LIMIT)),
            terminate: Mutex::new(Terminate::Ok),
            terminates: AtomicUsize::new(0),
            terminating: AtomicBool::new(false),
            opens: AtomicUsize::new(0),
            specs: Mutex::default(),
            prompts: AtomicUsize::new(0),
        }
    }
}

impl Ctl {
    fn send(&self, update: AgentUpdate) {
        if !matches!(update, AgentUpdate::Finished(_) | AgentUpdate::Failed(_)) {
            self.sent.fetch_add(1, Ordering::SeqCst);
        }
        if let Some(tx) = self.updates.lock().unwrap().as_ref() {
            let _ = tx.send(update);
        }
    }

    /// 以运行时的身份为 `Bash {command}` 发一个权限请求；答复按到达先后记进线上记录（`{name} allow|deny`）。
    fn ask(
        self: &Arc<Self>,
        name: &str,
        command: &str,
    ) -> tokio::task::JoinHandle<PermissionDecision> {
        let handler = self.handler.lock().unwrap().clone().expect("已开会话");
        let req = PermissionRequest {
            updates_before: self.sent.load(Ordering::SeqCst),
            tool: ToolRef("Bash".into()),
            raw_input: RawToolInput(serde_json::json!({ "command": command }).to_string()),
            bypass_sandbox: false,
            cwd: None,
        };
        self.in_flight.send_modify(|n| *n += 1);
        let (ctl, name) = (self.clone(), name.to_owned());
        tokio::spawn(async move {
            let decision = handler(req).await;
            let word = match &decision {
                PermissionDecision::Allow | PermissionDecision::AllowRewritten { .. } => "allow",
                PermissionDecision::Deny { .. } => "deny",
            };
            ctl.wire.lock().unwrap().push(format!("{name} {word}"));
            ctl.in_flight.send_modify(|n| *n -= 1);
            decision
        })
    }

    fn wire(&self) -> Vec<String> {
        self.wire.lock().unwrap().clone()
    }
}

struct FakeAgent(Arc<Ctl>);
struct FakeSession(Arc<Ctl>);
struct FakeUpdates(mpsc::UnboundedReceiver<AgentUpdate>);

impl AgentPort for FakeAgent {
    type Session = FakeSession;

    async fn capabilities(&self) -> Result<RuntimeCapabilities, AgentError> {
        unimplemented!()
    }

    async fn open_session<H: PermissionHandler + 'static>(
        &self,
        spec: SessionSpec,
        on_permission: H,
    ) -> Result<FakeSession, AgentError> {
        self.0.opens.fetch_add(1, Ordering::SeqCst);
        self.0.specs.lock().unwrap().push(spec);
        let h = Arc::new(on_permission);
        let handler: Handler = Arc::new(move |req| {
            let h = h.clone();
            Box::pin(async move { h.on_permission(req).await })
        });
        *self.0.handler.lock().unwrap() = Some(handler);
        Ok(FakeSession(self.0.clone()))
    }
}

impl AgentSession for FakeSession {
    type Updates = FakeUpdates;

    fn runtime_session_id(&self) -> Option<&str> {
        Some("fake")
    }

    fn permission_wait_limit(&self) -> Option<Duration> {
        *self.0.limit.lock().unwrap()
    }

    fn sandbox(&self) -> SandboxStatus {
        SandboxStatus::Enforced
    }

    async fn prompt(&self, _blocks: Vec<ContentBlock>) -> Result<FakeUpdates, AgentError> {
        let (tx, rx) = mpsc::unbounded_channel();
        *self.0.updates.lock().unwrap() = Some(tx);
        self.0.sent.store(0, Ordering::SeqCst);
        self.0.prompts.fetch_add(1, Ordering::SeqCst);
        Ok(FakeUpdates(rx))
    }

    /// 先等名下的权限任务都把答复写出去，再记 `session/cancel`（`iwork-agent-acp` 的约定）。
    async fn cancel(&self) -> Result<(), AgentError> {
        let mut rx = self.0.in_flight.subscribe();
        let _ = rx.wait_for(|n| *n == 0).await;
        self.0.wire.lock().unwrap().push("session/cancel".into());
        if self.0.cancel_ends_turn.load(Ordering::SeqCst) {
            self.0.send(AgentUpdate::Finished(TurnOutcome::Cancelled));
        }
        Ok(())
    }

    async fn terminate(&self) -> Result<(), AgentError> {
        self.0.terminates.fetch_add(1, Ordering::SeqCst);
        let mode = self.0.terminate.lock().unwrap().clone();
        self.0.terminating.store(true, Ordering::SeqCst);
        let result = match mode {
            Terminate::Ok => Ok(()),
            Terminate::Fail => Err(AgentError::ProcessExited {
                message: "运行时进程树没能在时限内结束".into(),
            }),
            Terminate::Gated(gate) => {
                gate.notified().await;
                Ok(())
            }
        };
        self.0.terminating.store(false, Ordering::SeqCst);
        result
    }

    async fn close(self) -> Result<(), AgentError> {
        Ok(())
    }
}

impl UpdateStream for FakeUpdates {
    async fn next(&mut self) -> Option<AgentUpdate> {
        self.0.recv().await
    }
}

#[derive(Default)]
struct Ids(AtomicU64);

impl IdGenerator for Ids {
    fn next(&self) -> RawUlid {
        RawUlid::from_u128(u128::from(self.0.fetch_add(1, Ordering::SeqCst)) + 0x5_0000)
    }
}

struct NoLive;

impl LivePublisher for NoLive {
    fn message_delta(&self, _delta: MessageDelta) {}
}

// ---------- 测试台 ----------

type Line<J> = SessionLine<FakeAgent, GateDecider<<J as Judge>::D>>;
type Task<J> = tokio::task::JoinHandle<(Result<TurnState, TurnRunError>, Line<J>)>;

struct W<J: Judge> {
    store: MemStore,
    judge: J,
    decider: Arc<GateDecider<J::D>>,
    ctl: Arc<Ctl>,
    clock: ManualClock,
    timer: ManualTimer,
    ids: Ids,
}

impl<J: Judge> W<J> {
    /// 布景 + 判定方，回合 `turn` 已排队。
    async fn new(turn: &Turn) -> Arc<Self> {
        let store = MemStore::default();
        seed_world(&store, &[turn]);
        let judge = J::make(&store).await;
        let decider = Arc::new(GateDecider::new(judge.decider()));
        Arc::new(Self {
            store,
            judge,
            decider,
            ctl: Arc::new(Ctl::default()),
            clock: ManualClock::default(),
            timer: ManualTimer::default(),
            ids: Ids::default(),
        })
    }

    fn line(&self) -> Line<J> {
        SessionLine::new(self.decider.clone(), None)
    }

    fn plan(&self, turn: Turn) -> TurnPlan {
        TurnPlan {
            turn,
            assistant_name: "小助手".into(),
            context_project_id: None,
            spec: session_spec("/work".into()),
            blocks: vec![ContentBlock::Text("hi".into())],
        }
    }

    fn spawn(self: &Arc<Self>, mut line: Line<J>, plan: TurnPlan) -> Task<J> {
        let w = self.clone();
        tokio::spawn(async move {
            let ports = TurnPorts {
                store: &w.store,
                clock: &w.clock,
                ids: &w.ids,
                live: &NoLive,
                timer: &w.timer,
            };
            let result = line.run_turn(&FakeAgent(w.ctl.clone()), ports, plan).await;
            (result, line)
        })
    }

    /// 新回合跑到运行时已 prompt。
    async fn running(self: &Arc<Self>, turn: Turn) -> Task<J> {
        let before = self.ctl.prompts.load(Ordering::SeqCst);
        let task = self.spawn(self.line(), self.plan(turn));
        wait_until("运行时收到 prompt", || {
            self.ctl.prompts.load(Ordering::SeqCst) > before
        })
        .await;
        task
    }

    fn state(&self, turn: TurnId) -> Option<TurnState> {
        state_of(&self.store, turn)
    }

    /// 等含 `marker` 的请求成了一条审批。
    async fn approval(&self, marker: &str) -> iwork_domain::id::ApprovalId {
        let mut id = None;
        wait_until("审批已请求", || {
            id = approval_for(&self.store, marker);
            id.is_some()
        })
        .await;
        id.unwrap()
    }

    /// 反复让「查期限」到时，直到条件成立（查期限可以多查，多查无害）。
    async fn tick_until(&self, what: &str, mut ok: impl FnMut() -> bool) {
        wait_until(what, || {
            self.timer.tick();
            ok()
        })
        .await;
    }

    async fn interrupted(&self, turn: TurnId) {
        self.tick_until("回合因等待上限中断", || {
            self.state(turn) == Some(wait_limit())
        })
        .await;
    }
}

fn wait_limit() -> TurnState {
    TurnState::Interrupted {
        reason: InterruptReason::ApprovalWaitLimit,
    }
}

fn gone() -> PermissionDecision {
    PermissionDecision::Deny {
        message: TURN_GONE_MESSAGE.into(),
    }
}

fn approved() -> String {
    format!(
        "decided A {:?}",
        ApprovalState::Approved {
            decider: iwork_domain::approval::Decider::User
        }
    )
}

const START: [&str; 2] = ["Queued -> Running", "Running -> WaitingApproval"];

fn strings(v: &[&str]) -> Vec<String> {
    v.iter().map(|s| (*s).to_owned()).collect()
}

/// 每条用例对两种判定方各跑一遍：`<用例>::reference`、`<用例>::engine`。
macro_rules! both {
    ($($name:ident),* $(,)?) => {$(
        mod $name {
            #[tokio::test(flavor = "current_thread")]
            async fn reference() {
                super::$name::<super::Reference>().await
            }
            #[tokio::test(flavor = "current_thread")]
            async fn engine() {
                super::$name::<super::RealEngine>().await
            }
        }
    )*};
}

both!(
    w1_approved_then_limit_keeps_one_grant,
    w2_decision_in_table_is_delivered_before_limit,
    w2_limit_first_rejects_late_decision,
    w3_delivered_then_limit_no_grant,
    limit_adopts_decided_before_interrupting,
    delivery_commit_precedes_allow,
    interrupt_commit_precedes_deny,
    w4_prompt_end_first_wins,
    w4_limit_first_then_end_only_finalizes,
    w4_complete_first_still_confirms_tree,
    w4_cleanup_failure_fails_turn_and_blocks_reopen,
    wait_limit_keeps_approvals,
    wait_limit_counts_sleep_time,
);

// ---------- W1–W4 ----------

/// W1：批准已落盘、结论还没交进交接表（停在判定方的返回路上）时到时。中断先提交，停止受理撤了这张票：
/// 处理方答拒绝，之后才放出来的结论没有去处（结论在撤票之后才交的那条路——`submit` 当场被拒——由
/// `permission.rs` 的 `stop_accepting_releases_every_waiter` 第五个请求盖住）。
/// 事件 `ApprovalDecided` → `Interrupted`，没有交付；运行时拿到拒绝；留下恰好一份授权。
async fn w1_approved_then_limit_keeps_one_grant<J: Judge>() {
    let turn = queued_turn(1);
    let t = turn.id;
    let w = W::<J>::new(&turn).await;
    let task = w.running(turn).await;
    let gate = w.decider.gate("npm test");
    let a = w.ctl.ask("A", "npm test");
    let id = w.approval("npm test").await;
    w.judge.user(id, true).await;
    wait_until("结论停在闸门前", || {
        gate.reached.load(Ordering::SeqCst)
    })
    .await;

    w.clock.advance_mono(LIMIT);
    w.interrupted(t).await;
    assert_eq!(a.await.unwrap(), gone(), "{}", J::NAME);
    gate.open();
    let (result, _) = task.await.unwrap();
    assert_eq!(result.unwrap(), wait_limit());

    let mut expected = strings(&START);
    expected.push(approved());
    expected.push("WaitingApproval -> Interrupted { reason: ApprovalWaitLimit }".into());
    assert_eq!(trail(&w.store, t), expected, "{}", J::NAME);
    assert_eq!(w.ctl.wire(), ["A deny", "session/cancel"]);
    assert!(w.judge.has_grant(id).await, "{}：留下一次性授权", J::NAME);
}

/// W2：结论已在交接表里，到时也就绪。用户的决定算数：照常交付、回到 Running，不中断，不留授权。
async fn w2_decision_in_table_is_delivered_before_limit<J: Judge>() {
    let turn = queued_turn(1);
    let t = turn.id;
    let w = W::<J>::new(&turn).await;
    let task = w.running(turn).await;
    let gate = w.decider.gate("npm test");
    let a = w.ctl.ask("A", "npm test");
    let id = w.approval("npm test").await;
    w.judge.user(id, true).await;
    wait_until("结论停在闸门前", || {
        gate.reached.load(Ordering::SeqCst)
    })
    .await;
    w.clock.advance_mono(LIMIT);
    // 同一段同步代码里：先放结论（处理方先被唤醒，交进表里），再让到时就绪；编排醒来时两样都在
    gate.open();
    w.timer.tick();
    assert_eq!(a.await.unwrap(), PermissionDecision::Allow, "{}", J::NAME);
    w.tick_until("回到 Running", || w.state(t) == Some(TurnState::Running))
        .await;

    let mut expected = strings(&START);
    expected.extend([
        approved(),
        "delivered A".into(),
        "WaitingApproval -> Running".into(),
    ]);
    assert_eq!(trail(&w.store, t), expected, "{}", J::NAME);
    assert!(
        !w.judge.has_grant(id).await,
        "{}：交付过的不留授权",
        J::NAME
    );
    w.ctl.send(AgentUpdate::Finished(TurnOutcome::Completed));
    assert_eq!(task.await.unwrap().0.unwrap(), TurnState::Completed);
}

/// W2 的另一种次序：到时先处理完（中断、撤票），用户之后才批准。运行时拿到拒绝；批准留作一次性授权。
async fn w2_limit_first_rejects_late_decision<J: Judge>() {
    let turn = queued_turn(1);
    let t = turn.id;
    let w = W::<J>::new(&turn).await;
    let task = w.running(turn).await;
    let a = w.ctl.ask("A", "npm test");
    let id = w.approval("npm test").await;
    w.clock.advance_mono(LIMIT);
    w.interrupted(t).await;
    assert_eq!(a.await.unwrap(), gone(), "{}", J::NAME);
    assert_eq!(task.await.unwrap().0.unwrap(), wait_limit());
    assert!(still_pending(&w.store, id), "{}：审批仍待定", J::NAME);

    w.judge.user(id, true).await;
    let mut expected = strings(&START);
    expected.extend([
        "WaitingApproval -> Interrupted { reason: ApprovalWaitLimit }".into(),
        approved(),
    ]);
    assert_eq!(trail(&w.store, t), expected, "{}", J::NAME);
    assert_eq!(w.ctl.wire(), ["A deny", "session/cancel"]);
    assert!(w.judge.has_grant(id).await, "{}", J::NAME);
}

/// W3：A 的交付已提交、回执已发，B 超期。A 原样放行、不留授权；B 被撤票、拒绝、仍待定。
async fn w3_delivered_then_limit_no_grant<J: Judge>() {
    let turn = queued_turn(1);
    let t = turn.id;
    let w = W::<J>::new(&turn).await;
    let task = w.running(turn).await;
    let a = w.ctl.ask("A", "npm test");
    let id_a = w.approval("npm test").await;
    let b = w.ctl.ask("B", "npm run lint");
    let id_b = w.approval("npm run lint").await;
    w.judge.user(id_a, true).await;
    assert_eq!(a.await.unwrap(), PermissionDecision::Allow, "{}", J::NAME);
    w.clock.advance_mono(LIMIT);
    w.interrupted(t).await;
    assert_eq!(b.await.unwrap(), gone(), "{}", J::NAME);
    assert_eq!(task.await.unwrap().0.unwrap(), wait_limit());

    let mut expected = strings(&START);
    expected.extend([
        approved(),
        "delivered A".into(),
        "WaitingApproval -> Interrupted { reason: ApprovalWaitLimit }".into(),
    ]);
    assert_eq!(trail(&w.store, t), expected, "{}", J::NAME);
    assert_eq!(w.ctl.wire(), ["A allow", "B deny", "session/cancel"]);
    assert!(!w.judge.has_grant(id_a).await, "{}：A 用掉了", J::NAME);
    assert!(still_pending(&w.store, id_b), "{}：B 仍待定", J::NAME);
    assert!(!w.judge.has_grant(id_b).await);
}

/// 到时那一刻表里同时有一张有结论的票 A 和一张超期的票 B：先把 A 采纳（用户已经做了决定的，算数），
/// 再因 B 中断。A 原样放行、有交付、不留授权；B 拒绝、仍待定。
async fn limit_adopts_decided_before_interrupting<J: Judge>() {
    let turn = queued_turn(1);
    let t = turn.id;
    let w = W::<J>::new(&turn).await;
    let task = w.running(turn).await;
    let gate = w.decider.gate("npm test");
    let a = w.ctl.ask("A", "npm test");
    let id_a = w.approval("npm test").await;
    let b = w.ctl.ask("B", "npm run lint");
    let id_b = w.approval("npm run lint").await;
    w.judge.user(id_a, true).await;
    wait_until("A 的结论停在闸门前", || {
        gate.reached.load(Ordering::SeqCst)
    })
    .await;
    w.clock.advance_mono(LIMIT);
    // 同一段同步代码里：A 先被唤醒、交进表里；编排醒来时「查期限」排在「看交接表」之前
    gate.open();
    w.timer.tick();
    w.interrupted(t).await;
    assert_eq!(a.await.unwrap(), PermissionDecision::Allow, "{}", J::NAME);
    assert_eq!(b.await.unwrap(), gone());
    assert_eq!(task.await.unwrap().0.unwrap(), wait_limit());
    let mut expected = strings(&START);
    expected.extend([
        approved(),
        "delivered A".into(),
        "WaitingApproval -> Interrupted { reason: ApprovalWaitLimit }".into(),
    ]);
    assert_eq!(trail(&w.store, t), expected, "{}", J::NAME);
    assert_eq!(w.ctl.wire(), ["A allow", "B deny", "session/cancel"]);
    assert!(!w.judge.has_grant(id_a).await);
    assert!(still_pending(&w.store, id_b));
}

/// 卡住编排「采纳 A」那一笔提交：交付事件落盘之前，线上没有 A 的允许（回执在提交之后才发）。放开后照常放行。
async fn delivery_commit_precedes_allow<J: Judge>() {
    let turn = queued_turn(1);
    let t = turn.id;
    let w = W::<J>::new(&turn).await;
    let task = w.running(turn).await;
    let gate = w.decider.gate("npm test");
    let a = w.ctl.ask("A", "npm test");
    let id = w.approval("npm test").await;
    w.judge.user(id, true).await;
    wait_until("结论停在闸门前", || {
        gate.reached.load(Ordering::SeqCst)
    })
    .await;
    let commit = Arc::new(Notify::new());
    *w.store.before_commit.lock().unwrap() = Some(commit.clone());
    gate.open();
    wait_until("编排停在交付那一笔上", || {
        w.store.commit_entered.load(Ordering::SeqCst)
    })
    .await;
    tokio::time::sleep(Duration::from_millis(50)).await;
    assert!(
        w.ctl.wire().is_empty(),
        "{}：交付落盘之前不放行：{:?}",
        J::NAME,
        w.ctl.wire()
    );
    assert!(!a.is_finished());
    assert!(!trail(&w.store, t).contains(&"delivered A".to_owned()));
    commit.notify_one();
    assert_eq!(a.await.unwrap(), PermissionDecision::Allow, "{}", J::NAME);
    assert!(trail(&w.store, t).contains(&"delivered A".to_owned()));
    w.ctl.send(AgentUpdate::Finished(TurnOutcome::Completed));
    assert_eq!(task.await.unwrap().0.unwrap(), TurnState::Completed);
}

/// 卡住「中断」那一笔提交：中断落盘之前，线上没有拒绝（先取得回合的业务收尾权，再回绝）。放开后照常拒绝、取消。
async fn interrupt_commit_precedes_deny<J: Judge>() {
    let turn = queued_turn(1);
    let t = turn.id;
    let w = W::<J>::new(&turn).await;
    let task = w.running(turn).await;
    let a = w.ctl.ask("A", "npm test");
    w.approval("npm test").await;
    let commit = Arc::new(Notify::new());
    *w.store.before_commit.lock().unwrap() = Some(commit.clone());
    w.clock.advance_mono(LIMIT);
    w.tick_until("编排停在中断那一笔上", || {
        w.store.commit_entered.load(Ordering::SeqCst)
    })
    .await;
    tokio::time::sleep(Duration::from_millis(50)).await;
    assert!(
        w.ctl.wire().is_empty(),
        "{}：中断落盘之前不回绝：{:?}",
        J::NAME,
        w.ctl.wire()
    );
    assert!(!a.is_finished());
    assert_eq!(w.state(t), Some(TurnState::WaitingApproval));
    commit.notify_one();
    assert_eq!(a.await.unwrap(), gone(), "{}", J::NAME);
    assert_eq!(task.await.unwrap().0.unwrap(), wait_limit());
    assert_eq!(w.ctl.wire(), ["A deny", "session/cancel"]);
}

/// W4：更新流先给出失败，再让到时就绪：回合 Failed，审批作废，没有中断；旧树确认清过。
async fn w4_prompt_end_first_wins<J: Judge>() {
    let turn = queued_turn(1);
    let t = turn.id;
    let w = W::<J>::new(&turn).await;
    let task = w.running(turn).await;
    let a = w.ctl.ask("A", "npm test");
    let id = w.approval("npm test").await;
    w.ctl.send(AgentUpdate::Failed(AgentError::ProcessExited {
        message: "退出了".into(),
    }));
    wait_until("先处理了结束", || {
        w.state(t) == Some(TurnState::Failed)
    })
    .await;
    // 到时随后才就绪：回合已经结束，没有可中断的
    w.clock.advance_mono(LIMIT);
    w.timer.tick();
    assert_eq!(task.await.unwrap().0.unwrap(), TurnState::Failed);
    assert_eq!(a.await.unwrap(), gone());

    let mut expected = strings(&START);
    expected.extend([
        "WaitingApproval -> Failed (运行时进程意外退出：退出了)".into(),
        "voided A".into(),
        "finished Failed".into(),
    ]);
    assert_eq!(trail(&w.store, t), expected, "{}", J::NAME);
    assert!(w.ctl.terminates.load(Ordering::SeqCst) >= 1);
    assert!(!w.judge.has_grant(id).await);
}

/// W4：先提交了中断，更新流随后给出「正常结束」：业务状态停在中断，助手消息 Interrupted；
/// 旧树确认清空之后 `run_turn` 才返回。
async fn w4_limit_first_then_end_only_finalizes<J: Judge>() {
    let turn = queued_turn(1);
    let t = turn.id;
    let w = W::<J>::new(&turn).await;
    w.ctl.cancel_ends_turn.store(false, Ordering::SeqCst);
    let task = w.running(turn).await;
    let a = w.ctl.ask("A", "npm test");
    w.approval("npm test").await;
    w.clock.advance_mono(LIMIT);
    w.interrupted(t).await;
    assert_eq!(a.await.unwrap(), gone());
    w.ctl.send(AgentUpdate::Finished(TurnOutcome::Completed));
    assert_eq!(task.await.unwrap().0.unwrap(), wait_limit());

    let mut expected = strings(&START);
    expected.push("WaitingApproval -> Interrupted { reason: ApprovalWaitLimit }".into());
    assert_eq!(trail(&w.store, t), expected, "{}", J::NAME);
    assert_eq!(message_endings(&w.store), [MessageStatus::Interrupted]);
    assert_eq!(w.ctl.terminates.load(Ordering::SeqCst), 1);
}

/// W4（2/F-2）：这一轮已经自己结束（`cancel()` 什么都不做），进程树还活着，清树卡在闸门上。
/// 闸门放开之前 `run_turn` 不返回（会话线还在它手里，续接开不了）；放开后回合 Interrupted，续接能开新会话。
async fn w4_complete_first_still_confirms_tree<J: Judge>() {
    let turn = queued_turn(1);
    let t = turn.id;
    let w = W::<J>::new(&turn).await;
    w.ctl.cancel_ends_turn.store(false, Ordering::SeqCst);
    let tree = Arc::new(Notify::new());
    *w.ctl.terminate.lock().unwrap() = Terminate::Gated(tree.clone());
    let task = w.running(turn.clone()).await;
    let a = w.ctl.ask("A", "npm test");
    let id = w.approval("npm test").await;
    w.clock.advance_mono(LIMIT);
    w.interrupted(t).await;
    assert_eq!(a.await.unwrap(), gone());
    w.ctl.send(AgentUpdate::Finished(TurnOutcome::Completed));
    wait_until("在清树", || w.ctl.terminating.load(Ordering::SeqCst)).await;
    tokio::time::sleep(Duration::from_millis(50)).await;
    assert!(
        !task.is_finished(),
        "{}：树没确认清空，run_turn 不返回",
        J::NAME
    );

    tree.notify_one();
    let (result, line) = task.await.unwrap();
    assert_eq!(result.unwrap(), wait_limit());
    assert!(line.session().is_none(), "旧会话已放掉");

    // 用户批准后续接：同一个回合，从 Interrupted 开始，开一条新会话（带续接 id）
    w.judge.user(id, true).await;
    *w.ctl.terminate.lock().unwrap() = Terminate::Ok;
    let mut resumed = turn;
    resumed.state = wait_limit();
    let mut plan = w.plan(resumed);
    plan.blocks = vec![ContentBlock::Text(resume_note(&ResumeReason::Approved {
        tool: "Bash",
        summary: "npm test",
    }))];
    let task = w.spawn(line, plan);
    wait_until("续接后 prompt", || {
        w.ctl.prompts.load(Ordering::SeqCst) == 2
    })
    .await;
    assert_eq!(w.ctl.opens.load(Ordering::SeqCst), 2);
    assert_eq!(
        w.ctl.specs.lock().unwrap()[1].resume.as_deref(),
        Some("fake"),
        "续接带上运行时会话 id"
    );
    w.ctl.send(AgentUpdate::Finished(TurnOutcome::Completed));
    assert_eq!(task.await.unwrap().0.unwrap(), TurnState::Completed);
    let trail = trail(&w.store, t);
    assert!(
        trail.contains(&"Interrupted { reason: ApprovalWaitLimit } -> Running".to_owned()),
        "{trail:?}"
    );
    assert_eq!(
        message_endings(&w.store),
        [MessageStatus::Interrupted, MessageStatus::Completed],
        "续接另起一条助手消息"
    );
}

/// W4（2/F-2）：清树失败。事件 `Interrupted` → `Failed`（原因「运行时进程树没能结束」）+ `TurnFinished`；
/// 已批准未交付的那条本会成为授权，回合进终态后不留。下一次开会话先再清一次，仍失败就不开、回合 Failed
/// 且原因点名；清得掉之后照常开。
async fn w4_cleanup_failure_fails_turn_and_blocks_reopen<J: Judge>() {
    let turn = queued_turn(1);
    let t = turn.id;
    let next = queued_turn(2);
    let third = queued_turn(3);
    let w = W::<J>::new(&turn).await;
    seed_turns(&w.store, &[&next, &third]);
    *w.ctl.terminate.lock().unwrap() = Terminate::Fail;
    let task = w.running(turn).await;
    let gate = w.decider.gate("npm test");
    let a = w.ctl.ask("A", "npm test");
    let id = w.approval("npm test").await;
    w.judge.user(id, true).await;
    wait_until("结论停在闸门前", || {
        gate.reached.load(Ordering::SeqCst)
    })
    .await;
    w.clock.advance_mono(LIMIT);
    // 中断之后清树失败，回合紧接着进 Failed
    w.tick_until("回合失败", || w.state(t) == Some(TurnState::Failed))
        .await;
    gate.open();
    assert_eq!(a.await.unwrap(), gone());
    let (result, line) = task.await.unwrap();
    assert_eq!(result.unwrap(), TurnState::Failed);

    let mut expected = strings(&START);
    expected.extend([
        approved(),
        "WaitingApproval -> Interrupted { reason: ApprovalWaitLimit }".into(),
        format!("Interrupted {{ reason: ApprovalWaitLimit }} -> Failed ({TREE_NOT_CLEARED})"),
        "finished Failed".into(),
    ]);
    assert_eq!(trail(&w.store, t), expected, "{}", J::NAME);
    assert!(!w.judge.has_grant(id).await, "{}：终态不留授权", J::NAME);
    assert!(line.session().is_some(), "清不掉的会话留着");

    // 下一回合：先再清一次，仍失败 → 不开会话，回合失败且原因点名
    let terminates = w.ctl.terminates.load(Ordering::SeqCst);
    let (result, line) = w.spawn(line, w.plan(next.clone())).await.unwrap();
    assert_eq!(result.unwrap(), TurnState::Failed);
    assert_eq!(w.ctl.terminates.load(Ordering::SeqCst), terminates + 1);
    assert_eq!(w.ctl.opens.load(Ordering::SeqCst), 1, "没有开新会话");
    let why = trail(&w.store, next.id);
    assert!(
        why[0].starts_with("Queued -> Failed (上一棵运行时进程树没能确认清空"),
        "{why:?}"
    );

    // 清得掉之后照常开
    *w.ctl.terminate.lock().unwrap() = Terminate::Ok;
    let before = w.ctl.prompts.load(Ordering::SeqCst);
    let task = w.spawn(line, w.plan(third));
    wait_until("新会话 prompt", || {
        w.ctl.prompts.load(Ordering::SeqCst) > before
    })
    .await;
    assert_eq!(w.ctl.opens.load(Ordering::SeqCst), 2);
    w.ctl.send(AgentUpdate::Finished(TurnOutcome::Completed));
    assert_eq!(task.await.unwrap().0.unwrap(), TurnState::Completed);
}

// ---------- 等待上限 ----------

/// 等待上限中断不作废审批：判定方没被要过作废计划，审批仍待定。
async fn wait_limit_keeps_approvals<J: Judge>() {
    let turn = queued_turn(1);
    let t = turn.id;
    let w = W::<J>::new(&turn).await;
    let task = w.running(turn).await;
    let a = w.ctl.ask("A", "npm test");
    let id = w.approval("npm test").await;
    w.clock.advance_mono(LIMIT - Duration::from_millis(1));
    for _ in 0..20 {
        w.timer.tick();
        tokio::task::yield_now().await;
    }
    assert_eq!(
        w.state(t),
        Some(TurnState::WaitingApproval),
        "差 1 毫秒不中断"
    );
    w.clock.advance_mono(Duration::from_millis(1));
    w.interrupted(t).await;
    assert_eq!(a.await.unwrap(), gone());
    assert_eq!(task.await.unwrap().0.unwrap(), wait_limit());
    assert_eq!(w.decider.void_calls.load(Ordering::SeqCst), 0);
    assert!(still_pending(&w.store, id));
}

/// 机器睡眠时单调时钟可能不走：只拨墙钟也算到时（宁早勿晚）。墙钟往回拨不算。
async fn wait_limit_counts_sleep_time<J: Judge>() {
    let turn = queued_turn(1);
    let t = turn.id;
    let w = W::<J>::new(&turn).await;
    let task = w.running(turn).await;
    let a = w.ctl.ask("A", "npm test");
    w.approval("npm test").await;
    w.clock.advance_wall(LIMIT);
    w.interrupted(t).await;
    assert_eq!(a.await.unwrap(), gone());
    assert_eq!(task.await.unwrap().0.unwrap(), wait_limit());
}

/// 没声明上限的运行时：时间怎么走都一直等，审批答复后照常继续。
#[tokio::test(flavor = "current_thread")]
async fn no_wait_limit_runtime_waits_forever() {
    let turn = queued_turn(1);
    let t = turn.id;
    let w = W::<Reference>::new(&turn).await;
    *w.ctl.limit.lock().unwrap() = None;
    let task = w.running(turn).await;
    let a = w.ctl.ask("A", "npm test");
    let id = w.approval("npm test").await;
    w.clock.advance_mono(Duration::from_secs(1_000_000));
    w.clock.advance_wall(Duration::from_secs(1_000_000));
    for _ in 0..50 {
        w.timer.tick();
        tokio::task::yield_now().await;
    }
    assert_eq!(w.state(t), Some(TurnState::WaitingApproval));
    w.judge.user(id, true).await;
    assert_eq!(a.await.unwrap(), PermissionDecision::Allow);
    w.ctl.send(AgentUpdate::Finished(TurnOutcome::Completed));
    assert_eq!(task.await.unwrap().0.unwrap(), TurnState::Completed);
}

// ---------- 续接入口与续接说明 ----------

/// 续接：回合在 Interrupted，第一步是 Resume，另起一条助手消息（同一个回合）。说明文字原样交给运行时。
#[tokio::test(flavor = "current_thread")]
async fn resume_entry_starts_from_interrupted() {
    let mut turn = queued_turn(1);
    let t = turn.id;
    let w = W::<Reference>::new(&turn).await;
    w.store.seed(vec![
        iwork_domain::event::TurnStateChanged {
            turn_id: t,
            from: TurnState::Queued,
            to: TurnState::Running,
            reason: None,
        }
        .into(),
        iwork_domain::event::TurnStateChanged {
            turn_id: t,
            from: TurnState::Running,
            to: TurnState::Interrupted {
                reason: InterruptReason::Crash,
            },
            reason: None,
        }
        .into(),
    ]);
    turn.state = TurnState::Interrupted {
        reason: InterruptReason::Crash,
    };
    let task = w.running(turn).await;
    w.ctl.send(AgentUpdate::Finished(TurnOutcome::Completed));
    assert_eq!(task.await.unwrap().0.unwrap(), TurnState::Completed);
    assert_eq!(
        trail(&w.store, t),
        [
            "Queued -> Running",
            "Running -> Interrupted { reason: Crash }",
            "Interrupted { reason: Crash } -> Running",
            "Running -> Completed",
            "finished Completed",
        ]
    );
    let messages: Vec<_> = w
        .store
        .all()
        .into_iter()
        .filter_map(|e| match e {
            DomainEvent::MessageAppended(m) => m.message.turn_id,
            _ => None,
        })
        .collect();
    assert_eq!(messages, [t], "续接另起一条助手消息，属于同一个回合");
}

/// 续接开不起来（这里是运行时起不来）：中断的回合有合法的去处，记成 Failed。
#[tokio::test(flavor = "current_thread")]
async fn resume_failure_fails_interrupted_turn() {
    let mut turn = queued_turn(1);
    let t = turn.id;
    let w = W::<Reference>::new(&turn).await;
    w.store.seed(vec![
        iwork_domain::event::TurnStateChanged {
            turn_id: t,
            from: TurnState::Queued,
            to: TurnState::Running,
            reason: None,
        }
        .into(),
        iwork_domain::event::TurnStateChanged {
            turn_id: t,
            from: TurnState::Running,
            to: wait_limit(),
            reason: None,
        }
        .into(),
    ]);
    turn.state = wait_limit();
    // 会话线上留着一棵清不掉的树：续接之前先清，清不掉就不开
    let line = {
        let task = w.running(queued_turn_seeded(&w, 9)).await;
        *w.ctl.terminate.lock().unwrap() = Terminate::Fail;
        w.ctl.send(AgentUpdate::Failed(AgentError::ProcessExited {
            message: "退出了".into(),
        }));
        let (result, line) = task.await.unwrap();
        assert_eq!(result.unwrap(), TurnState::Failed);
        line
    };
    let (result, _) = w.spawn(line, w.plan(turn)).await.unwrap();
    assert_eq!(result.unwrap(), TurnState::Failed);
    let why = trail(&w.store, t);
    assert!(
        why.last().unwrap() == "finished Failed"
            && why[2].starts_with("Interrupted { reason: ApprovalWaitLimit } -> Failed (上一棵"),
        "{why:?}"
    );
}

/// 布景里再排一个回合。
fn queued_turn_seeded<J: Judge>(w: &W<J>, n: u128) -> Turn {
    let turn = queued_turn(n);
    seed_turns(&w.store, &[&turn]);
    turn
}

/// 三种续接说明的文字（手写期望，T06 方案 5.3）。
#[test]
fn resume_note_texts() {
    assert_eq!(
        resume_note(&ResumeReason::Approved {
            tool: "Bash",
            summary: "npm test"
        }),
        "[iWork] 上一轮因为等用户审批暂停了。用户现在已批准你当时发起的那次调用（Bash：npm test）。请原样重新发起它，所有参数与上次逐字相同，不要改写、不要合并别的操作。这次批准只对这一次调用有效。"
    );
    assert_eq!(
        resume_note(&ResumeReason::Denied {
            tool: "Bash",
            summary: "rm -rf build"
        }),
        "[iWork] 上一轮因为等用户审批暂停了。用户拒绝了你当时发起的那次调用（Bash：rm -rf build）。不要换别的方式做同一件事。请继续剩下的工作；如果没有它就完不成，说明原因后停下。"
    );
    assert_eq!(
        resume_note(&ResumeReason::Interrupted),
        "[iWork] 上一轮被中断了。请先检查做到了哪一步，再从中断的地方继续，不要重复已经完成的操作。"
    );
}

// ---------- 会话绑定项守卫 ----------

/// 模型改了：下一轮关掉会话、带 resume 重开；进行中的那一轮不受影响（它用的是开会话时的模型）。
#[tokio::test(flavor = "current_thread")]
async fn model_change_reopens_session() {
    let first = queued_turn(1);
    let w = W::<Reference>::new(&first).await;
    let task = w.running(first).await;
    w.ctl.send(AgentUpdate::Finished(TurnOutcome::Completed));
    let (result, line) = task.await.unwrap();
    assert_eq!(result.unwrap(), TurnState::Completed);
    assert!(line.session().is_some());

    // 同样的 spec：沿用会话
    let same = queued_turn_seeded(&w, 2);
    let task = w.spawn(line, w.plan(same));
    wait_until("prompt", || w.ctl.prompts.load(Ordering::SeqCst) == 2).await;
    w.ctl.send(AgentUpdate::Finished(TurnOutcome::Completed));
    let (_, line) = task.await.unwrap();
    assert_eq!(w.ctl.opens.load(Ordering::SeqCst), 1, "对照：没改就不重开");

    let changed = queued_turn_seeded(&w, 3);
    let mut plan = w.plan(changed);
    plan.spec.model = ModelSelection {
        model_id: ModelId("m2".into()),
        effort: None,
    };
    let terminates = w.ctl.terminates.load(Ordering::SeqCst);
    let task = w.spawn(line, plan);
    wait_until("prompt", || w.ctl.prompts.load(Ordering::SeqCst) == 3).await;
    w.ctl.send(AgentUpdate::Finished(TurnOutcome::Completed));
    assert_eq!(task.await.unwrap().0.unwrap(), TurnState::Completed);
    assert_eq!(w.ctl.opens.load(Ordering::SeqCst), 2);
    assert_eq!(
        w.ctl.terminates.load(Ordering::SeqCst),
        terminates + 1,
        "旧树确认清过"
    );
    let specs = w.ctl.specs.lock().unwrap();
    assert_eq!(specs[1].model.model_id, ModelId("m2".into()));
    assert_eq!(specs[1].resume.as_deref(), Some("fake"), "带 resume 重开");
}

/// 约束（以及模型、系统提示以外的任何绑定项）和打开会话时不同：调用方的错，回合失败、原因点名字段，
/// 不开会话。
#[tokio::test(flavor = "current_thread")]
async fn mismatched_constraints_fail_turn() {
    let first = queued_turn(1);
    let w = W::<Reference>::new(&first).await;
    let task = w.running(first).await;
    w.ctl.send(AgentUpdate::Finished(TurnOutcome::Completed));
    let (_, mut line) = task.await.unwrap();

    type Edit = fn(&mut SessionSpec);
    let edits: [(&str, Edit); 7] = [
        ("constraints", |s| {
            s.constraints.allow_unsandboxed_retry = true
        }),
        ("tool_set", |s| s.tool_set.include_always_on = false),
        ("disabled_tools", |s| {
            s.disabled_tools.push(ToolRef("Cron".into()))
        }),
        ("plugin_dirs", |s| {
            s.plugin_dirs.push("/imports/plugin".into())
        }),
        ("mcp", |s| {
            s.mcp.push(iwork_ports::agent::McpServerRef {
                id: iwork_domain::event::McpServerId("m1".into()),
                name: "m1".into(),
            })
        }),
        ("cwd", |s| s.cwd = "/elsewhere".into()),
        ("kind", |s| {
            s.kind = iwork_domain::turn::TurnKind::SystemAiReview
        }),
    ];
    for (n, (field, edit)) in (2u128..).zip(edits) {
        let turn = queued_turn_seeded(&w, n);
        let mut plan = w.plan(turn.clone());
        edit(&mut plan.spec);
        let (result, back) = w.spawn(line, plan).await.unwrap();
        line = back;
        assert_eq!(result.unwrap(), TurnState::Failed, "{field}");
        assert_eq!(
            trail(&w.store, turn.id)[0],
            format!("Queued -> Failed (会话约束与快照不一致：{field})")
        );
    }
    assert_eq!(w.ctl.opens.load(Ordering::SeqCst), 1, "一次都没有重开");
    assert_eq!(w.ctl.prompts.load(Ordering::SeqCst), 1);
}

fn message_endings(store: &MemStore) -> Vec<MessageStatus> {
    store
        .all()
        .into_iter()
        .filter_map(|e| match e {
            DomainEvent::MessageCompleted(m) => Some(m.status),
            _ => None,
        })
        .collect()
}
