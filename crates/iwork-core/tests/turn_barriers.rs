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

use std::collections::VecDeque;
use std::future::Future;
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use iwork_core::turn::{
    REQUEST_HEADER, ResumeReason, ResumeRequest, SUMMARY_HEADER, SUMMARY_RECENT_LIMIT, SessionLine,
    TREE_NOT_CLEARED, TURN_GONE_MESSAGE, TurnEntry, TurnPlan, TurnPorts, TurnRunError, resume_note,
};
use iwork_domain::action::ToolRef;
use iwork_domain::approval::ApprovalState;
use iwork_domain::common::{ModelId, ModelSelection};
use iwork_domain::event::{DomainEvent, SessionReplaced, TurnSessionBound};
use iwork_domain::id::{RawUlid, TurnId};
use iwork_domain::message::MessageStatus;
use iwork_domain::session::SessionLostReason;
use iwork_domain::turn::{InterruptReason, Turn, TurnState};
use iwork_ports::agent::{
    AgentError, AgentPort, AgentSession, AgentUpdate, ContentBlock, PermissionDecision,
    PermissionHandler, PermissionRequest, RawToolInput, RuntimeCapabilities, SandboxStatus,
    SessionSpec, TurnOutcome, UpdateStream,
};
use iwork_ports::clock::IdGenerator;
use iwork_ports::live::{LivePublisher, MessageDelta};
use iwork_ports::read_model::{SummaryLine, SummarySource, SummaryText, SummaryWho};
use tokio::sync::{Notify, mpsc, watch};

mod support;

use support::engine_fakes::MemStore;
use support::history::FakeHistory;
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
    /// 每次 prompt 收到的输入
    inputs: Mutex<Vec<Vec<ContentBlock>>>,
    /// 接下来几次开会话的结局，排空后一律成功
    open_errors: Mutex<VecDeque<AgentError>>,
    /// 接下来几次 prompt 的错误，排空后一律成功
    prompt_errors: Mutex<VecDeque<AgentError>>,
    /// 不带 resume 新开的会话依次用这些 id，排空后用 `fake`；带 resume 的沿用那个 id
    fresh_ids: Mutex<VecDeque<String>>,
    /// 下一次 prompt 一直不返回（测 future 被丢掉）
    hang_prompt: AtomicBool,
    /// 下一次开会话停在这个闸门上（`opening` 为真时停着）
    open_gate: Mutex<Option<Arc<Notify>>>,
    opening: AtomicBool,
    /// 下一次 prompt 收下请求之前停在这个闸门上（`prompting` 为真时停着）
    prompt_gate: Mutex<Option<Arc<Notify>>>,
    prompting: AtomicBool,
    /// 每次调 prompt 时会话实例的编号（第几次开出来的会话，从 1 数），含没收下的
    prompt_calls: Mutex<Vec<usize>>,
    /// 真正收下 prompt 的会话实例编号
    received: Mutex<Vec<usize>>,
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
            inputs: Mutex::default(),
            open_errors: Mutex::default(),
            prompt_errors: Mutex::default(),
            fresh_ids: Mutex::default(),
            hang_prompt: AtomicBool::new(false),
            open_gate: Mutex::default(),
            opening: AtomicBool::new(false),
            prompt_gate: Mutex::default(),
            prompting: AtomicBool::new(false),
            prompt_calls: Mutex::default(),
            received: Mutex::default(),
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
/// 第二个字段是运行时会话 id，第三个是会话实例编号（第几次开出来的，从 1 数）。
struct FakeSession(Arc<Ctl>, String, usize);
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
        let instance = self.0.opens.fetch_add(1, Ordering::SeqCst) + 1;
        self.0.specs.lock().unwrap().push(spec.clone());
        let gate = self.0.open_gate.lock().unwrap().take();
        if let Some(gate) = gate {
            self.0.opening.store(true, Ordering::SeqCst);
            gate.notified().await;
            self.0.opening.store(false, Ordering::SeqCst);
        }
        if let Some(error) = self.0.open_errors.lock().unwrap().pop_front() {
            return Err(error);
        }
        let id = spec.resume.unwrap_or_else(|| {
            self.0
                .fresh_ids
                .lock()
                .unwrap()
                .pop_front()
                .unwrap_or_else(|| "fake".into())
        });
        let h = Arc::new(on_permission);
        let handler: Handler = Arc::new(move |req| {
            let h = h.clone();
            Box::pin(async move { h.on_permission(req).await })
        });
        *self.0.handler.lock().unwrap() = Some(handler);
        Ok(FakeSession(self.0.clone(), id, instance))
    }
}

impl AgentSession for FakeSession {
    type Updates = FakeUpdates;

    fn runtime_session_id(&self) -> Option<&str> {
        Some(&self.1)
    }

    fn permission_wait_limit(&self) -> Option<Duration> {
        *self.0.limit.lock().unwrap()
    }

    fn sandbox(&self) -> SandboxStatus {
        SandboxStatus::Enforced
    }

    async fn prompt(&self, blocks: Vec<ContentBlock>) -> Result<FakeUpdates, AgentError> {
        self.0.prompt_calls.lock().unwrap().push(self.2);
        if let Some(error) = self.0.prompt_errors.lock().unwrap().pop_front() {
            return Err(error);
        }
        let gate = self.0.prompt_gate.lock().unwrap().take();
        if let Some(gate) = gate {
            self.0.prompting.store(true, Ordering::SeqCst);
            gate.notified().await;
            self.0.prompting.store(false, Ordering::SeqCst);
        }
        if self.0.hang_prompt.swap(false, Ordering::SeqCst) {
            return std::future::pending().await;
        }
        self.0.inputs.lock().unwrap().push(blocks);
        self.0.received.lock().unwrap().push(self.2);
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
    history: FakeHistory,
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
            history: FakeHistory::default(),
        })
    }

    fn line(&self) -> Line<J> {
        SessionLine::new(self.decider.clone(), None)
    }

    fn plan(&self, turn: Turn) -> TurnPlan {
        TurnPlan {
            changes: None,
            entry: TurnEntry::New,
            turn,
            assistant_name: "小助手".into(),
            context_project_id: None,
            spec: session_spec("/work".into()),
            blocks: vec![ContentBlock::Text("hi".into())],
        }
    }

    /// 续接回合的 plan：请求是 [`request_blocks`]，`blocks` 是续接说明。
    fn resume_plan(&self, turn: Turn) -> TurnPlan {
        TurnPlan {
            entry: TurnEntry::Resume {
                request: ResumeRequest::Ready(request_blocks()),
            },
            blocks: vec![ContentBlock::Text(resume_note(&ResumeReason::Interrupted))],
            ..self.plan(turn)
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
                history: &w.history,
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
    recovery_keeps_denial_memory_and_grants,
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
    let mut plan = w.resume_plan(resumed);
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
    let task = w.spawn(w.line(), w.resume_plan(turn));
    wait_until("prompt", || w.ctl.prompts.load(Ordering::SeqCst) == 1).await;
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
    let (result, _) = w.spawn(line, w.resume_plan(turn)).await.unwrap();
    assert_eq!(result.unwrap(), TurnState::Failed);
    let why = trail(&w.store, t);
    assert!(
        why.last().unwrap() == "finished Failed"
            && why[2].starts_with("Interrupted { reason: ApprovalWaitLimit } -> Failed (上一棵"),
        "{why:?}"
    );
}

// ---------- 续接失败用摘要新开（T06 方案 5.2a；需求第 12 章 v1.78） ----------

const OLD_ID: &str = "old-1";
const NEW_ID: &str = "new-1";

fn user_line(text: &str) -> SummaryLine {
    SummaryLine {
        who: SummaryWho::User,
        text: SummaryText::Inline(text.into()),
        attachments: Vec::new(),
    }
}

/// 布景里的历史：第一句、一条助手回复，这一轮的请求单独给。
fn history() -> SummarySource {
    SummarySource {
        first_user: Some(user_line("第一句")),
        earlier: vec![SummaryLine {
            who: SummaryWho::Assistant {
                name: "小助手".into(),
            },
            text: SummaryText::Inline("做完了".into()),
            attachments: Vec::new(),
        }],
        unfetched: 0,
    }
}

/// 调用方给的续接回合请求：正文全文加一个附件块，照新回合的样子。
fn request_blocks() -> Vec<ContentBlock> {
    vec![
        ContentBlock::Text("这一轮的请求".into()),
        ContentBlock::Attachment {
            artifact: iwork_domain::common::ArtifactRef {
                sha256: iwork_domain::common::Sha256([7; 32]),
                area: iwork_domain::common::ArtifactArea::Attachments,
                size: 3,
            },
            mime: "text/plain".into(),
            name: "spec.txt".into(),
        },
    ]
}

fn lost(reason: SessionLostReason) -> AgentError {
    AgentError::SessionLost { reason }
}

fn text(block: &ContentBlock) -> &str {
    match block {
        ContentBlock::Text(t) => t,
        other => panic!("不是文字块：{other:?}"),
    }
}

/// 这个回合的 `TurnSessionBound`。
fn bindings(store: &MemStore, turn: TurnId) -> Vec<TurnSessionBound> {
    store
        .all()
        .into_iter()
        .filter_map(|e| match e {
            DomainEvent::TurnSessionBound(b) if b.turn_id == turn => Some(b),
            _ => None,
        })
        .collect()
}

/// 把回合布成「崩溃后中断」，返回续接用的回合。
fn interrupted_turn<J: Judge>(w: &W<J>, mut turn: Turn) -> Turn {
    w.store.seed(vec![
        iwork_domain::event::TurnStateChanged {
            turn_id: turn.id,
            from: TurnState::Queued,
            to: TurnState::Running,
            reason: None,
        }
        .into(),
        iwork_domain::event::TurnStateChanged {
            turn_id: turn.id,
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
    turn
}

/// 续接时记录用不了（三类各一遍）：回合不失败；第二次开会话不带旧编号；`TurnSessionBound` 记新编号、
/// `replaced` 里是旧编号与原因，与回合进 `Running` 同一笔；第一条输入开头是摘要；没有给用户看的消息或卡片。
#[tokio::test(flavor = "current_thread")]
async fn resume_of_missing_session_recovers_with_summary() {
    for reason in [
        SessionLostReason::NotFound,
        SessionLostReason::Corrupt,
        SessionLostReason::Incompatible,
    ] {
        let turn = queued_turn(1);
        let t = turn.id;
        let w = W::<Reference>::new(&turn).await;
        w.history.set(history());
        w.ctl.open_errors.lock().unwrap().push_back(lost(reason));
        w.ctl.fresh_ids.lock().unwrap().push_back(NEW_ID.into());
        let line = SessionLine::new(w.decider.clone(), Some(OLD_ID.into()));
        let task = w.spawn(line, w.plan(turn));
        wait_until("prompt", || w.ctl.prompts.load(Ordering::SeqCst) == 1).await;
        w.ctl.send(AgentUpdate::Finished(TurnOutcome::Completed));
        let (result, line) = task.await.unwrap();
        assert_eq!(result.unwrap(), TurnState::Completed, "{reason:?}");

        let specs = w.ctl.specs.lock().unwrap().clone();
        assert_eq!(specs.len(), 2, "{reason:?}");
        assert_eq!(specs[0].resume.as_deref(), Some(OLD_ID));
        assert_eq!(specs[1].resume, None, "放下旧编号新开");
        assert_eq!(
            bindings(&w.store, t),
            [TurnSessionBound {
                turn_id: t,
                runtime_session_id: NEW_ID.into(),
                replaced: Some(SessionReplaced {
                    old_runtime_session_id: OLD_ID.into(),
                    reason,
                }),
            }]
        );
        let events = w.store.all();
        let at = events
            .iter()
            .position(|e| matches!(e, DomainEvent::TurnSessionBound(_)))
            .unwrap();
        assert!(
            matches!(&events[at + 1], DomainEvent::TurnStateChanged(c)
                if c.from == TurnState::Queued && c.to == TurnState::Running),
            "绑定与进 Running 同一笔"
        );
        assert_eq!(line.resume_id(), Some(NEW_ID));
        assert_eq!(w.history.calls(), [(t, None, SUMMARY_RECENT_LIMIT)]);

        let inputs = w.ctl.inputs.lock().unwrap().clone();
        let first = &inputs[0];
        assert_eq!(first.len(), 2, "新回合：摘要 + 原本的输入，不另给请求");
        assert!(text(&first[0]).starts_with(SUMMARY_HEADER), "{first:?}");
        assert!(text(&first[0]).contains("第一句") && text(&first[0]).contains("做完了"));
        assert!(!text(&first[0]).contains("这一轮的请求"));
        assert_eq!(text(&first[1]), "hi");

        // 界面不提示：唯一新增的消息是这一轮的助手消息，没有审批、卡片
        let shown: Vec<_> = events
            .iter()
            .filter(|e| {
                matches!(
                    e,
                    DomainEvent::MessageAppended(_)
                        | DomainEvent::ApprovalRequested(_)
                        | DomainEvent::UserActionProposed(_)
                )
            })
            .collect();
        assert!(
            matches!(shown.as_slice(), [DomainEvent::MessageAppended(m)] if m.message.turn_id == Some(t)),
            "{shown:?}"
        );
    }
}

/// 续接的回合记录用不了：第一条输入 = 摘要 + 这一轮的请求原文 + 续接说明，顺序固定。
#[tokio::test(flavor = "current_thread")]
async fn lost_session_on_resume_recovers_interrupted_turn() {
    let turn = queued_turn(1);
    let t = turn.id;
    let w = W::<Reference>::new(&turn).await;
    let turn = interrupted_turn(&w, turn);
    w.history.set(history());
    w.ctl
        .open_errors
        .lock()
        .unwrap()
        .push_back(lost(SessionLostReason::NotFound));
    w.ctl.fresh_ids.lock().unwrap().push_back(NEW_ID.into());
    let plan = w.resume_plan(turn);
    let note = resume_note(&ResumeReason::Interrupted);
    let task = w.spawn(
        SessionLine::new(w.decider.clone(), Some(OLD_ID.into())),
        plan,
    );
    wait_until("prompt", || w.ctl.prompts.load(Ordering::SeqCst) == 1).await;
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
    let first = w.ctl.inputs.lock().unwrap()[0].clone();
    assert!(text(&first[0]).starts_with(SUMMARY_HEADER), "{first:?}");
    assert_eq!(text(&first[1]), REQUEST_HEADER);
    // 请求照调用方给的原样转交，附件块也在
    assert_eq!(first[2..4], request_blocks()[..]);
    assert_eq!(first[4..], [ContentBlock::Text(note)]);
    assert_eq!(bindings(&w.store, t)[0].runtime_session_id, NEW_ID);
}

/// 用摘要新开也失败：开会话恰好两次，回合 `Failed`，原因两段都在；不再重试，旧编号留着给下一回合。
#[tokio::test(flavor = "current_thread")]
async fn recovery_fails_once_then_turn_fails() {
    let turn = queued_turn(1);
    let t = turn.id;
    let w = W::<Reference>::new(&turn).await;
    let turn = interrupted_turn(&w, turn);
    w.ctl.open_errors.lock().unwrap().extend([
        lost(SessionLostReason::Corrupt),
        AgentError::ProcessExited {
            message: "起不来".into(),
        },
    ]);
    let (result, line) = w
        .spawn(
            SessionLine::new(w.decider.clone(), Some(OLD_ID.into())),
            w.resume_plan(turn),
        )
        .await
        .unwrap();
    assert_eq!(result.unwrap(), TurnState::Failed);
    assert_eq!(w.ctl.opens.load(Ordering::SeqCst), 2, "只试一次新开");
    assert_eq!(w.ctl.prompts.load(Ordering::SeqCst), 0);
    assert_eq!(
        trail(&w.store, t)[2..],
        [
            "Interrupted { reason: Crash } -> Failed (续接失败（会话记录损坏），用摘要新开会话也没成：运行时进程意外退出：起不来)",
            "finished Failed",
        ]
    );
    assert!(bindings(&w.store, t).is_empty());
    assert!(w.history.calls().is_empty(), "没开成就不读历史");
    assert_eq!(line.resume_id(), Some(OLD_ID));
}

/// 对照：只有三类原因才走这条路。续接回进程退出或别的错误、没带旧编号的新会话报「记录用不了」，都不新开，
/// 回合 `Failed`。
#[tokio::test(flavor = "current_thread")]
async fn unclassified_resume_failure_still_fails() {
    let cases = [
        (
            Some(OLD_ID),
            AgentError::ProcessExited {
                message: "退出了".into(),
            },
        ),
        (
            Some(OLD_ID),
            AgentError::Protocol {
                message: "运行时报告了错误（unknown）".into(),
            },
        ),
        (None, lost(SessionLostReason::NotFound)),
    ];
    for (resume, error) in cases {
        let turn = queued_turn(1);
        let t = turn.id;
        let w = W::<Reference>::new(&turn).await;
        w.ctl.open_errors.lock().unwrap().push_back(error.clone());
        let line = SessionLine::new(w.decider.clone(), resume.map(str::to_owned));
        let (result, _) = w.spawn(line, w.plan(turn)).await.unwrap();
        assert_eq!(result.unwrap(), TurnState::Failed, "{error:?}");
        assert_eq!(w.ctl.opens.load(Ordering::SeqCst), 1, "{error:?}");
        assert_eq!(trail(&w.store, t)[0], format!("Queued -> Failed ({error})"));
    }
}

/// 开会话的另一处：沿用的会话进程已退出、发 prompt 前重开时记录也用不了，同样用摘要新开、记下旧编号。
#[tokio::test(flavor = "current_thread")]
async fn lost_session_after_process_exit_recovers() {
    let first = queued_turn(1);
    let w = W::<Reference>::new(&first).await;
    let task = w.running(first).await;
    w.ctl.send(AgentUpdate::Finished(TurnOutcome::Completed));
    let (_, line) = task.await.unwrap();

    let second = queued_turn_seeded(&w, 2);
    let t = second.id;
    w.history.set(history());
    w.ctl
        .prompt_errors
        .lock()
        .unwrap()
        .push_back(AgentError::ProcessExited {
            message: "退出了".into(),
        });
    w.ctl
        .open_errors
        .lock()
        .unwrap()
        .push_back(lost(SessionLostReason::NotFound));
    w.ctl.fresh_ids.lock().unwrap().push_back(NEW_ID.into());
    let task = w.spawn(line, w.plan(second));
    wait_until("prompt", || w.ctl.prompts.load(Ordering::SeqCst) == 2).await;
    w.ctl.send(AgentUpdate::Finished(TurnOutcome::Completed));
    assert_eq!(task.await.unwrap().0.unwrap(), TurnState::Completed);
    let specs = w.ctl.specs.lock().unwrap().clone();
    assert_eq!(
        specs
            .iter()
            .map(|s| s.resume.as_deref())
            .collect::<Vec<_>>(),
        [None, Some("fake"), None]
    );
    assert_eq!(
        bindings(&w.store, t),
        [
            TurnSessionBound {
                turn_id: t,
                runtime_session_id: "fake".into(),
                replaced: None,
            },
            TurnSessionBound {
                turn_id: t,
                runtime_session_id: NEW_ID.into(),
                replaced: Some(SessionReplaced {
                    old_runtime_session_id: "fake".into(),
                    reason: SessionLostReason::NotFound,
                }),
            },
        ]
    );
    let input = w.ctl.inputs.lock().unwrap()[1].clone();
    assert!(text(&input[0]).starts_with(SUMMARY_HEADER));
    assert_eq!(text(&input[1]), "hi");
}

/// 新会话开成、摘要还没送到：这一步失败后重跑，要重新走「新开 + 摘要 + replaced」（审查 1/F-1）。
/// `break_once` 让第一次在某一步失败，返回第一次 `run_turn` 的结果。
async fn rerun_after_broken_recovery(
    break_once: impl FnOnce(&W<Reference>),
) -> (Result<TurnState, TurnRunError>, Arc<W<Reference>>, TurnId) {
    let turn = queued_turn(1);
    let t = turn.id;
    let w = W::<Reference>::new(&turn).await;
    w.history.set(history());
    w.ctl
        .open_errors
        .lock()
        .unwrap()
        .push_back(lost(SessionLostReason::NotFound));
    w.ctl.fresh_ids.lock().unwrap().push_back(NEW_ID.into());
    break_once(&w);
    let (first, line) = w
        .spawn(
            SessionLine::new(w.decider.clone(), Some(OLD_ID.into())),
            w.plan(turn.clone()),
        )
        .await
        .unwrap();
    assert_eq!(line.resume_id(), Some(OLD_ID), "新编号没算数，退回旧编号");
    assert!(line.session().is_none(), "没带上文的新会话已丢掉");
    assert_eq!(w.ctl.terminates.load(Ordering::SeqCst), 1);

    // 重跑：记录照样用不了，重新新开、重新发摘要、重新记 replaced
    w.ctl
        .open_errors
        .lock()
        .unwrap()
        .push_back(lost(SessionLostReason::NotFound));
    w.ctl.fresh_ids.lock().unwrap().push_back("new-2".into());
    assert_eq!(w.state(t), None, "第一次失败后回合没离开 Queued");
    let task = w.spawn(line, w.plan(turn));
    wait_until("prompt", || w.ctl.prompts.load(Ordering::SeqCst) == 1).await;
    w.ctl.send(AgentUpdate::Finished(TurnOutcome::Completed));
    let (result, line) = task.await.unwrap();
    assert_eq!(result.unwrap(), TurnState::Completed);
    assert_eq!(line.resume_id(), Some("new-2"));
    let resumes: Vec<_> = w
        .ctl
        .specs
        .lock()
        .unwrap()
        .iter()
        .map(|s| s.resume.clone())
        .collect();
    assert_eq!(
        resumes,
        [Some(OLD_ID.into()), None, Some(OLD_ID.into()), None]
    );
    let input = w.ctl.inputs.lock().unwrap()[0].clone();
    assert!(text(&input[0]).starts_with(SUMMARY_HEADER), "{input:?}");
    assert_eq!(
        bindings(&w.store, t).last().unwrap(),
        &TurnSessionBound {
            turn_id: t,
            runtime_session_id: "new-2".into(),
            replaced: Some(SessionReplaced {
                old_runtime_session_id: OLD_ID.into(),
                reason: SessionLostReason::NotFound,
            }),
        }
    );
    (first, w, t)
}

/// 读历史失败一次：编排出错，会话线退回旧编号，重跑重新恢复。
#[tokio::test(flavor = "current_thread")]
async fn summary_read_failure_keeps_old_id() {
    let (first, w, _) = rerun_after_broken_recovery(|w| {
        w.history.fail_once(iwork_ports::store::StorageError::Busy)
    })
    .await;
    assert!(
        matches!(
            first,
            Err(TurnRunError::Storage(
                iwork_ports::store::StorageError::Busy
            ))
        ),
        "{first:?}"
    );
    assert_eq!(w.history.calls().len(), 2);
}

/// 绑定（与进 Running 同一笔）提交失败一次：同上。
#[tokio::test(flavor = "current_thread")]
async fn binding_commit_failure_keeps_old_id() {
    let (first, _, _) =
        rerun_after_broken_recovery(|w| w.store.fail_next.store(true, Ordering::SeqCst)).await;
    assert!(
        matches!(
            first,
            Err(TurnRunError::Storage(
                iwork_ports::store::StorageError::Busy
            ))
        ),
        "{first:?}"
    );
}

/// 新会话开成、绑定也落了盘，首条 prompt 却发不出去：回合 Failed，新会话丢掉、退回旧编号，下一回合重新恢复。
#[tokio::test(flavor = "current_thread")]
async fn prompt_failure_after_recovery_keeps_old_id() {
    let turn = queued_turn(1);
    let w = W::<Reference>::new(&turn).await;
    w.history.set(history());
    w.ctl
        .open_errors
        .lock()
        .unwrap()
        .push_back(lost(SessionLostReason::NotFound));
    w.ctl.fresh_ids.lock().unwrap().push_back(NEW_ID.into());
    w.ctl
        .prompt_errors
        .lock()
        .unwrap()
        .push_back(AgentError::Protocol {
            message: "发不出去".into(),
        });
    let (result, line) = w
        .spawn(
            SessionLine::new(w.decider.clone(), Some(OLD_ID.into())),
            w.plan(turn),
        )
        .await
        .unwrap();
    assert_eq!(result.unwrap(), TurnState::Failed);
    assert_eq!(line.resume_id(), Some(OLD_ID));
    assert!(line.session().is_none());
    assert_eq!(w.ctl.terminates.load(Ordering::SeqCst), 1);
    assert_eq!(
        bindings(&w.store, turn_id(1)).last().unwrap(),
        &rebound(turn_id(1)),
        "回合最后绑定的是旧编号（审查 2/F-2）"
    );

    let next = queued_turn_seeded(&w, 2);
    w.ctl
        .open_errors
        .lock()
        .unwrap()
        .push_back(lost(SessionLostReason::NotFound));
    w.ctl.fresh_ids.lock().unwrap().push_back("new-2".into());
    let task = w.spawn(line, w.plan(next.clone()));
    wait_until("prompt", || w.ctl.prompts.load(Ordering::SeqCst) == 1).await;
    w.ctl.send(AgentUpdate::Finished(TurnOutcome::Completed));
    assert_eq!(task.await.unwrap().0.unwrap(), TurnState::Completed);
    assert_eq!(
        bindings(&w.store, next.id)[0].replaced,
        Some(SessionReplaced {
            old_runtime_session_id: OLD_ID.into(),
            reason: SessionLostReason::NotFound,
        })
    );
    assert!(text(&w.ctl.inputs.lock().unwrap()[0][0]).starts_with(SUMMARY_HEADER));
}

fn turn_id(n: u128) -> TurnId {
    queued_turn(n).id
}

/// 退回旧编号时补的那条绑定。
fn rebound(turn: TurnId) -> TurnSessionBound {
    TurnSessionBound {
        turn_id: turn,
        runtime_session_id: OLD_ID.into(),
        replaced: None,
    }
}

/// 下一回合（回合 2）照常用摘要新开、记下旧编号。
async fn next_turn_recovers(w: &Arc<W<Reference>>, line: Line<Reference>) {
    let next = queued_turn_seeded(w, 2);
    w.ctl
        .open_errors
        .lock()
        .unwrap()
        .push_back(lost(SessionLostReason::NotFound));
    w.ctl.fresh_ids.lock().unwrap().push_back("new-2".into());
    let prompts = w.ctl.prompts.load(Ordering::SeqCst);
    let task = w.spawn(line, w.plan(next.clone()));
    wait_until("prompt", || w.ctl.prompts.load(Ordering::SeqCst) > prompts).await;
    w.ctl.send(AgentUpdate::Finished(TurnOutcome::Completed));
    assert_eq!(task.await.unwrap().0.unwrap(), TurnState::Completed);
    assert_eq!(
        bindings(&w.store, next.id),
        [TurnSessionBound {
            turn_id: next.id,
            runtime_session_id: "new-2".into(),
            replaced: Some(SessionReplaced {
                old_runtime_session_id: OLD_ID.into(),
                reason: SessionLostReason::NotFound,
            }),
        }]
    );
    let inputs = w.ctl.inputs.lock().unwrap().clone();
    assert!(text(&inputs.last().unwrap()[0]).starts_with(SUMMARY_HEADER));
}

/// 补记旧编号那一笔提交失败：编排自己的错误；会话已丢、编号已退回，下一次入口把这笔补上（审查 2/F-2）。
#[tokio::test(flavor = "current_thread")]
async fn rebind_commit_failure_is_retried_at_next_entry() {
    let turn = queued_turn(1);
    let w = W::<Reference>::new(&turn).await;
    w.history.set(history());
    w.ctl
        .open_errors
        .lock()
        .unwrap()
        .push_back(lost(SessionLostReason::NotFound));
    w.ctl.fresh_ids.lock().unwrap().push_back(NEW_ID.into());
    w.ctl
        .prompt_errors
        .lock()
        .unwrap()
        .push_back(AgentError::Protocol {
            message: "发不出去".into(),
        });
    // 这一轮依次提交：绑定 + 进 Running、失败收尾、补记旧编号。第三笔报 Busy
    let at = w.store.commits.load(Ordering::SeqCst) + 3;
    w.store.fail_commit_at.store(at, Ordering::SeqCst);
    let (result, line) = w
        .spawn(
            SessionLine::new(w.decider.clone(), Some(OLD_ID.into())),
            w.plan(turn),
        )
        .await
        .unwrap();
    assert!(
        matches!(
            result,
            Err(TurnRunError::Storage(
                iwork_ports::store::StorageError::Busy
            ))
        ),
        "{result:?}"
    );
    assert_eq!(w.state(turn_id(1)), Some(TurnState::Failed));
    assert_eq!(line.resume_id(), Some(OLD_ID));
    assert!(line.session().is_none());
    assert_eq!(
        bindings(&w.store, turn_id(1))
            .last()
            .unwrap()
            .runtime_session_id,
        NEW_ID,
        "对照：这时库里还是新编号"
    );
    let terminates = w.ctl.terminates.load(Ordering::SeqCst);

    next_turn_recovers(&w, line).await;
    assert_eq!(
        bindings(&w.store, turn_id(1)).last().unwrap(),
        &rebound(turn_id(1))
    );
    assert_eq!(
        w.ctl.terminates.load(Ordering::SeqCst),
        terminates,
        "入口只补提交，不再丢一次会话"
    );
}

/// run_turn 的 future 在首条 prompt 被收下之前被丢掉（绑定已落盘）：下一次入口丢掉新会话、退回旧编号、
/// 补记旧编号，下一回合重新恢复（审查 2/F-1）。
#[tokio::test(flavor = "current_thread")]
async fn dropped_run_is_settled_at_next_entry() {
    let turn = queued_turn(1);
    let w = W::<Reference>::new(&turn).await;
    w.history.set(history());
    w.ctl
        .open_errors
        .lock()
        .unwrap()
        .push_back(lost(SessionLostReason::NotFound));
    w.ctl.fresh_ids.lock().unwrap().push_back(NEW_ID.into());
    w.ctl.hang_prompt.store(true, Ordering::SeqCst);
    let mut line: Line<Reference> = SessionLine::new(w.decider.clone(), Some(OLD_ID.into()));
    {
        let agent = FakeAgent(w.ctl.clone());
        let ports = TurnPorts {
            store: &w.store,
            clock: &w.clock,
            ids: &w.ids,
            live: &NoLive,
            timer: &w.timer,
            history: &w.history,
        };
        let run = line.run_turn(&agent, ports, w.plan(turn));
        tokio::pin!(run);
        tokio::select! {
            r = &mut run => panic!("prompt 挂住时回合不该结束：{r:?}"),
            () = wait_until("绑定落盘、卡在 prompt", || {
                !bindings(&w.store, turn_id(1)).is_empty()
                    && !w.ctl.hang_prompt.load(Ordering::SeqCst)
            }) => {}
        }
    } // future 丢掉
    assert_eq!(line.resume_id(), Some(NEW_ID), "对照：丢掉时收尾没跑");
    let terminates = w.ctl.terminates.load(Ordering::SeqCst);

    next_turn_recovers(&w, line).await;
    assert_eq!(
        w.ctl.terminates.load(Ordering::SeqCst),
        terminates + 1,
        "入口丢掉了没带上文的新会话"
    );
    assert_eq!(
        bindings(&w.store, turn_id(1)).last().unwrap(),
        &rebound(turn_id(1))
    );
    let resumes: Vec<_> = w
        .ctl
        .specs
        .lock()
        .unwrap()
        .iter()
        .map(|s| s.resume.clone())
        .collect();
    assert_eq!(
        resumes,
        [Some(OLD_ID.into()), None, Some(OLD_ID.into()), None],
        "没有接着用新会话"
    );
}

/// prompt 那条路（沿用的进程已退出、重开时记录用不了）恢复后，第二次 prompt 也失败：回合 Failed，
/// 这个回合最后绑定的是旧编号（审查 3/F-1）。
#[tokio::test(flavor = "current_thread")]
async fn prompt_path_failure_after_recovery_rebinds_old_id() {
    let first = queued_turn(1);
    let w = W::<Reference>::new(&first).await;
    let task = w.running(first).await;
    w.ctl.send(AgentUpdate::Finished(TurnOutcome::Completed));
    let (_, line) = task.await.unwrap();

    let second = queued_turn_seeded(&w, 2);
    w.history.set(history());
    w.ctl.prompt_errors.lock().unwrap().extend([
        AgentError::ProcessExited {
            message: "退出了".into(),
        },
        AgentError::Protocol {
            message: "发不出去".into(),
        },
    ]);
    w.ctl
        .open_errors
        .lock()
        .unwrap()
        .push_back(lost(SessionLostReason::NotFound));
    w.ctl.fresh_ids.lock().unwrap().push_back(NEW_ID.into());
    let (result, line) = w.spawn(line, w.plan(second.clone())).await.unwrap();
    assert_eq!(result.unwrap(), TurnState::Failed);
    assert_eq!(line.resume_id(), Some("fake"));
    let bound: Vec<_> = bindings(&w.store, second.id)
        .into_iter()
        .map(|b| b.runtime_session_id)
        .collect();
    assert_eq!(bound, ["fake", NEW_ID, "fake"], "最后一条是旧编号");
}

/// 绑定那笔提交还在途时 future 被丢掉：那笔照样落盘，下一次入口仍补记旧编号（审查 3/F-2）。
#[tokio::test(flavor = "current_thread")]
async fn run_dropped_during_binding_commit_still_rebinds() {
    let turn = queued_turn(1);
    let w = W::<Reference>::new(&turn).await;
    w.history.set(history());
    w.ctl
        .open_errors
        .lock()
        .unwrap()
        .push_back(lost(SessionLostReason::NotFound));
    w.ctl.fresh_ids.lock().unwrap().push_back(NEW_ID.into());
    // 这一轮的第一笔提交就是绑定 + 进 Running：让它入队后停在处理之前
    let gate = Arc::new(Notify::new());
    *w.store.before_commit.lock().unwrap() = Some(gate.clone());
    let mut line: Line<Reference> = SessionLine::new(w.decider.clone(), Some(OLD_ID.into()));
    {
        let agent = FakeAgent(w.ctl.clone());
        let ports = TurnPorts {
            store: &w.store,
            clock: &w.clock,
            ids: &w.ids,
            live: &NoLive,
            timer: &w.timer,
            history: &w.history,
        };
        let run = line.run_turn(&agent, ports, w.plan(turn));
        tokio::pin!(run);
        tokio::select! {
            r = &mut run => panic!("绑定还没落盘时回合不该结束：{r:?}"),
            () = wait_until("绑定那笔停在处理之前", || {
                w.store.commit_entered.load(Ordering::SeqCst)
            }) => {}
        }
    } // future 丢掉，那笔已入队
    gate.notify_one();
    wait_until("那笔落了盘", || {
        !bindings(&w.store, turn_id(1)).is_empty()
    })
    .await;
    assert_eq!(
        bindings(&w.store, turn_id(1))[0].runtime_session_id,
        NEW_ID,
        "对照：新编号落了盘"
    );

    next_turn_recovers(&w, line).await;
    assert_eq!(
        bindings(&w.store, turn_id(1)).last().unwrap(),
        &rebound(turn_id(1))
    );
}

/// 接住 poll 里的 panic，变成 `Err`。
async fn catch_panic<F: Future>(fut: F) -> Result<F::Output, ()> {
    let mut fut = std::pin::pin!(fut);
    std::future::poll_fn(|cx| {
        match std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| fut.as_mut().poll(cx))) {
            Ok(std::task::Poll::Ready(out)) => std::task::Poll::Ready(Ok(out)),
            Ok(std::task::Poll::Pending) => std::task::Poll::Pending,
            Err(_) => std::task::Poll::Ready(Err(())),
        }
    })
    .await
}

/// run_turn 里 panic、调用方接住后接着用这条线：同样在下一次入口收尾（审查 2/F-1）。
#[tokio::test(flavor = "current_thread")]
async fn panicked_run_is_settled_at_next_entry() {
    let turn = queued_turn(1);
    let w = W::<Reference>::new(&turn).await;
    // 不给历史原料：读历史时假读模型 panic
    w.ctl
        .open_errors
        .lock()
        .unwrap()
        .push_back(lost(SessionLostReason::NotFound));
    w.ctl.fresh_ids.lock().unwrap().push_back(NEW_ID.into());
    let mut line: Line<Reference> = SessionLine::new(w.decider.clone(), Some(OLD_ID.into()));
    let agent = FakeAgent(w.ctl.clone());
    let ports = TurnPorts {
        store: &w.store,
        clock: &w.clock,
        ids: &w.ids,
        live: &NoLive,
        timer: &w.timer,
        history: &w.history,
    };
    let caught = catch_panic(line.run_turn(&agent, ports, w.plan(turn.clone()))).await;
    assert!(caught.is_err(), "读历史时 panic 了");
    assert_eq!(line.resume_id(), Some(NEW_ID), "对照：panic 时收尾没跑");

    w.history.set(history());
    w.ctl
        .open_errors
        .lock()
        .unwrap()
        .push_back(lost(SessionLostReason::NotFound));
    w.ctl.fresh_ids.lock().unwrap().push_back("new-2".into());
    let task = w.spawn(line, w.plan(turn));
    wait_until("prompt", || w.ctl.prompts.load(Ordering::SeqCst) == 1).await;
    w.ctl.send(AgentUpdate::Finished(TurnOutcome::Completed));
    let (result, line) = task.await.unwrap();
    assert_eq!(result.unwrap(), TurnState::Completed);
    assert_eq!(line.resume_id(), Some("new-2"));
    let resumes: Vec<_> = w
        .ctl
        .specs
        .lock()
        .unwrap()
        .iter()
        .map(|s| s.resume.clone())
        .collect();
    assert_eq!(
        resumes,
        [Some(OLD_ID.into()), None, Some(OLD_ID.into()), None]
    );
    assert!(text(&w.ctl.inputs.lock().unwrap()[0][0]).starts_with(SUMMARY_HEADER));
}

/// 续接回合的请求取不全：回合 Failed、写明原因，不发 prompt，已开的新会话照 retire 收尾，退回旧编号
/// （审查 1/F-3）。
#[tokio::test(flavor = "current_thread")]
async fn unavailable_request_fails_resumed_turn() {
    let turn = queued_turn(1);
    let t = turn.id;
    let w = W::<Reference>::new(&turn).await;
    let turn = interrupted_turn(&w, turn);
    w.ctl
        .open_errors
        .lock()
        .unwrap()
        .push_back(lost(SessionLostReason::NotFound));
    w.ctl.fresh_ids.lock().unwrap().push_back(NEW_ID.into());
    let plan = TurnPlan {
        entry: TurnEntry::Resume {
            request: ResumeRequest::Unavailable("附件 spec.txt 不在了".into()),
        },
        ..w.resume_plan(turn)
    };
    let (result, line) = w
        .spawn(
            SessionLine::new(w.decider.clone(), Some(OLD_ID.into())),
            plan,
        )
        .await
        .unwrap();
    assert_eq!(result.unwrap(), TurnState::Failed);
    assert_eq!(
        trail(&w.store, t)[2..],
        [
            "Interrupted { reason: Crash } -> Failed (续接失败（会话记录不存在），这一轮的请求取不全：附件 spec.txt 不在了)",
            "finished Failed",
        ]
    );
    assert_eq!(w.ctl.prompts.load(Ordering::SeqCst), 0, "不发残缺的请求");
    assert_eq!(w.ctl.opens.load(Ordering::SeqCst), 2, "新会话开成了");
    assert_eq!(
        w.ctl.terminates.load(Ordering::SeqCst),
        1,
        "新会话照 retire 收尾"
    );
    assert!(line.session().is_none());
    assert_eq!(line.resume_id(), Some(OLD_ID));
    assert!(bindings(&w.store, t).is_empty());

    // 对照：记录还在（续接成功）时用不到请求，Unavailable 也照常跑
    let other = queued_turn_seeded(&w, 2);
    let other = interrupted_turn(&w, other);
    let plan = TurnPlan {
        entry: TurnEntry::Resume {
            request: ResumeRequest::Unavailable("用不到".into()),
        },
        ..w.resume_plan(other)
    };
    let task = w.spawn(line, plan);
    wait_until("prompt", || w.ctl.prompts.load(Ordering::SeqCst) == 1).await;
    w.ctl.send(AgentUpdate::Finished(TurnOutcome::Completed));
    assert_eq!(task.await.unwrap().0.unwrap(), TurnState::Completed);
    assert_eq!(
        w.ctl.inputs.lock().unwrap()[0],
        [ContentBlock::Text(resume_note(&ResumeReason::Interrupted))]
    );
}

/// 入口填错（给 Queued 的回合填 Resume）：第一步报表外转换，回合不往下走（会话在第一步转换之前已经开了，
/// 随编排出错一起丢掉）。
#[tokio::test(flavor = "current_thread")]
async fn entry_must_match_turn_state() {
    let turn = queued_turn(1);
    let w = W::<Reference>::new(&turn).await;
    let (result, _) = w.spawn(w.line(), w.resume_plan(turn)).await.unwrap();
    assert!(
        matches!(result, Err(TurnRunError::Illegal(_))),
        "{result:?}"
    );
}

/// 审批类与执行类事件（换会话本身不该产生任何一条）。
fn approval_trail(store: &MemStore) -> Vec<String> {
    store
        .all()
        .into_iter()
        .filter_map(|e| match e {
            DomainEvent::ApprovalRequested(r) => Some(format!("requested {}", r.approval.id)),
            DomainEvent::ApprovalDecided(d) => Some(format!("decided {}", d.approval_id)),
            DomainEvent::ApprovalDelivered(d) => Some(format!("delivered {}", d.approval_id)),
            DomainEvent::ApprovalVoided(v) => Some(format!("voided {}", v.approval_id)),
            DomainEvent::ToolCallStarted(c) => Some(format!("tool {}", c.tool_call_id.0)),
            DomainEvent::UserActionProposed(_) => Some("user action".into()),
            _ => None,
        })
        .collect()
}

/// 换了会话之后，之前被拒的同一动作仍被自动拒绝；一次性授权没有因为换会话多出来或被用掉（方案 15.3；
/// AC05 点名「换运行时」是绕过的入口）。拒绝记忆只有真实引擎有，参照判定方只核授权。
async fn recovery_keeps_denial_memory_and_grants<J: Judge>() {
    let turn = queued_turn(1);
    let t = turn.id;
    let w = W::<J>::new(&turn).await;
    let task = w.running(turn.clone()).await;
    // 用户拒绝一次
    let a = w.ctl.ask("A", "rm -rf build");
    let denied = w.approval("rm -rf build").await;
    w.judge.user(denied, false).await;
    assert!(
        matches!(a.await.unwrap(), PermissionDecision::Deny { .. }),
        "{}",
        J::NAME
    );
    w.tick_until("回到 Running", || w.state(t) == Some(TurnState::Running))
        .await;
    // 批准了、还没交付就到时：留下一次性授权（同 W1）
    let gate = w.decider.gate("npm test");
    let b = w.ctl.ask("B", "npm test");
    let granted = w.approval("npm test").await;
    w.judge.user(granted, true).await;
    wait_until("结论停在闸门前", || {
        gate.reached.load(Ordering::SeqCst)
    })
    .await;
    w.clock.advance_mono(LIMIT);
    w.interrupted(t).await;
    assert_eq!(b.await.unwrap(), gone(), "{}", J::NAME);
    gate.open();
    let (result, line) = task.await.unwrap();
    assert_eq!(result.unwrap(), wait_limit());
    assert!(w.judge.has_grant(granted).await, "{}", J::NAME);
    let before = approval_trail(&w.store);

    // 续接：记录用不了，用摘要新开
    w.history.set(history());
    w.ctl
        .open_errors
        .lock()
        .unwrap()
        .push_back(lost(SessionLostReason::NotFound));
    w.ctl.fresh_ids.lock().unwrap().push_back(NEW_ID.into());
    let mut resumed = turn;
    resumed.state = wait_limit();
    let task = w.spawn(line, w.resume_plan(resumed));
    wait_until("续接后 prompt", || {
        w.ctl.prompts.load(Ordering::SeqCst) == 2
    })
    .await;
    assert!(bindings(&w.store, t).last().unwrap().replaced.is_some());
    assert_eq!(
        approval_trail(&w.store),
        before,
        "{}：换会话本身不动审批",
        J::NAME
    );
    assert!(
        w.judge.has_grant(granted).await,
        "{}：授权没被换会话用掉",
        J::NAME
    );
    if J::DENIAL_MEMORY {
        let c = w.ctl.ask("C", "rm -rf build");
        let asked = || {
            w.store
                .all()
                .iter()
                .filter(|e| {
                    matches!(e, DomainEvent::ApprovalRequested(r)
                    if r.approval.action.raw_summary.contains("rm -rf build"))
                })
                .count()
        };
        // 没有拒绝记忆时会再问一次、一直等用户：两样谁先到都停下来判，不靠超时
        wait_until("C 有结论或又问了用户", || {
            c.is_finished() || asked() > 1
        })
        .await;
        assert_eq!(asked(), 1, "自动拒绝，没有再问用户");
        assert!(
            matches!(c.await.unwrap(), PermissionDecision::Deny { .. }),
            "新会话里同一动作仍被拒"
        );
    }
    w.ctl.send(AgentUpdate::Finished(TurnOutcome::Completed));
    assert_eq!(task.await.unwrap().0.unwrap(), TurnState::Completed);
    assert!(
        !approval_trail(&w.store).contains(&format!("delivered {granted}")),
        "{}：授权没被交付出去",
        J::NAME
    );
}

/// 换会话不重放：新会话只收到文字；iWork 不发起任何工具调用、不答任何权限请求；没有新的审批或执行类事件。
#[tokio::test(flavor = "current_thread")]
async fn recovery_does_not_replay() {
    let first = queued_turn(1);
    let w = W::<Reference>::new(&first).await;
    let task = w.running(first).await;
    w.ctl.send(AgentUpdate::ToolCallStarted {
        tool_call_id: iwork_domain::event::ToolCallId("c1".into()),
        tool: ToolRef("Bash".into()),
        raw_input: RawToolInput(r#"{"command":"npm test"}"#.into()),
    });
    let a = w.ctl.ask("A", "npm test");
    let id = w.approval("npm test").await;
    w.judge.user(id, true).await;
    assert_eq!(a.await.unwrap(), PermissionDecision::Allow);
    w.ctl.send(AgentUpdate::Finished(TurnOutcome::Completed));
    let (_, line) = task.await.unwrap();
    let wire = w.ctl.wire();
    let before = approval_trail(&w.store);
    assert!(!before.is_empty(), "对照：第一轮确实有审批与工具调用");

    let second = queued_turn_seeded(&w, 2);
    w.history.set(history());
    w.ctl
        .prompt_errors
        .lock()
        .unwrap()
        .push_back(AgentError::ProcessExited {
            message: "退出了".into(),
        });
    w.ctl
        .open_errors
        .lock()
        .unwrap()
        .push_back(lost(SessionLostReason::NotFound));
    w.ctl.fresh_ids.lock().unwrap().push_back(NEW_ID.into());
    let task = w.spawn(line, w.plan(second));
    wait_until("prompt", || w.ctl.prompts.load(Ordering::SeqCst) == 2).await;
    w.ctl.send(AgentUpdate::Finished(TurnOutcome::Completed));
    assert_eq!(task.await.unwrap().0.unwrap(), TurnState::Completed);
    let input = w.ctl.inputs.lock().unwrap()[1].clone();
    assert!(
        input.iter().all(|b| matches!(b, ContentBlock::Text(_))),
        "{input:?}"
    );
    assert_eq!(w.ctl.wire(), wire, "没有答过任何权限请求");
    assert_eq!(approval_trail(&w.store), before, "没有新的审批或执行类事件");
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

/// 交出 prompt 之前认取消（T10a 方案 D1a、7.2 第 ① 条）。
#[path = "turn_barriers/pre_prompt_cancel.rs"]
mod pre_prompt_cancel;

/// 丢掉会话线之前先了结（T10a 方案 D8a、7.2 第 ② 条）。
#[path = "turn_barriers/line_swap.rs"]
mod line_swap;

/// 回合状态两类写者撞车（T10a 方案 A4、G1；7.2a 的 K3）。
#[path = "turn_barriers/a4_interleavings.rs"]
mod a4_interleavings;
