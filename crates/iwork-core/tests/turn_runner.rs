//! 回合编排对着一个可控的假运行时（进程内，不起进程）：审批结果绑定回合、取消优先、
//! 编排出错时收尾、定时检查点。这些都是时序问题，假运行时让每一步都由测试决定。

use std::collections::VecDeque;
use std::future::Future;
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use iwork_core::turn::{
    Decided, PermissionDecider, SessionLine, TURN_GONE_MESSAGE, TurnCanceller, TurnPlan, TurnPorts,
    TurnRunError, VoidPlanError,
};
use iwork_domain::action::ToolRef;
use iwork_domain::approval::VoidReason;
use iwork_domain::common::{ModelId, ModelSelection, Timestamp, ToolGroups};
use iwork_domain::event::{DomainEvent, ToolCallId};
use iwork_domain::id::{AssistantId, ConversationId, MessageId, RawUlid, RuntimeId, Seq, TurnId};
use iwork_domain::session::{AllowWrite, NetworkPolicy, SessionConstraints, SessionToolSet};
use iwork_domain::turn::{Turn, TurnKind, TurnState};
use iwork_ports::agent::{
    AgentError, AgentPort, AgentSession, AgentUpdate, ContentBlock, PermissionDecision,
    PermissionHandler, PermissionRequest, RawToolInput, RuntimeCapabilities, SandboxStatus,
    SessionSpec, TurnOutcome, UpdateStream,
};
use iwork_ports::clock::{Clock, IdGenerator, MonotonicInstant, Timer};
use iwork_ports::live::{LivePublisher, MessageDelta};
use iwork_ports::store::{
    CommitReceipt, CommitRequest, DeliveryState, EventFilter, OutboxId, OutboxItem, RebuildReport,
    StorageError, StoragePort, StoredEvent,
};
use tokio::sync::{mpsc, oneshot};

mod support;

// ---------- 假运行时 ----------

type Handler = Arc<
    dyn Fn(PermissionRequest) -> Pin<Box<dyn Future<Output = PermissionDecision> + Send>>
        + Send
        + Sync,
>;

/// 测试对假运行时的控制面。
#[derive(Default)]
struct Ctl {
    handler: Mutex<Option<Handler>>,
    updates: Mutex<Option<mpsc::UnboundedSender<AgentUpdate>>>,
    prompts: AtomicUsize,
    /// 本回合已发出的更新条数（不含终态），和真适配器一样作为权限请求的前序边界
    sent: std::sync::atomic::AtomicU64,
    cancels: AtomicUsize,
    closes: AtomicUsize,
    /// 为真时更新流永远就绪（不停输出文字），直到收到取消
    flood: AtomicBool,
    /// 调过几次 prompt（含返回错误的）
    prompt_calls: AtomicUsize,
    /// 依次给接下来的 prompt 调用返回这些错误；空了就正常受理
    prompt_errors: Mutex<VecDeque<AgentError>>,
    /// 设了就让下一次 open_session 返回它
    fail_next_open: Mutex<Option<AgentError>>,
    opens: AtomicUsize,
}

impl Ctl {
    fn send(&self, update: AgentUpdate) {
        if !matches!(update, AgentUpdate::Finished(_) | AgentUpdate::Failed(_)) {
            self.sent.fetch_add(1, Ordering::SeqCst);
        }
        self.updates
            .lock()
            .unwrap()
            .as_ref()
            .expect("已 prompt")
            .send(update)
            .unwrap();
    }

    /// 以运行时的身份发一个权限请求，返回它最终拿到的答复。
    fn ask(&self) -> tokio::task::JoinHandle<PermissionDecision> {
        self.ask_bash("{}")
    }

    /// 同上，工具输入由测试给。
    fn ask_bash(&self, raw_input: &str) -> tokio::task::JoinHandle<PermissionDecision> {
        let handler = self.handler.lock().unwrap().clone().expect("已开会话");
        tokio::spawn(handler(PermissionRequest {
            updates_before: self.sent.load(Ordering::SeqCst),
            tool: ToolRef("Bash".into()),
            raw_input: RawToolInput(raw_input.into()),
            bypass_sandbox: false,
            cwd: None,
        }))
    }
}

struct FakeAgent(Arc<Ctl>);
struct FakeSession(Arc<Ctl>);
struct FakeUpdates {
    ctl: Arc<Ctl>,
    rx: mpsc::UnboundedReceiver<AgentUpdate>,
    flood_done: bool,
}

impl AgentPort for FakeAgent {
    type Session = FakeSession;

    async fn capabilities(&self) -> Result<RuntimeCapabilities, AgentError> {
        unimplemented!()
    }

    async fn open_session<H: PermissionHandler + 'static>(
        &self,
        _spec: SessionSpec,
        on_permission: H,
    ) -> Result<FakeSession, AgentError> {
        self.0.opens.fetch_add(1, Ordering::SeqCst);
        if let Some(error) = self.0.fail_next_open.lock().unwrap().take() {
            return Err(error);
        }
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
        None
    }

    fn sandbox(&self) -> SandboxStatus {
        SandboxStatus::Enforced
    }

    async fn terminate(&self) -> Result<(), AgentError> {
        Ok(())
    }

    async fn prompt(&self, _blocks: Vec<ContentBlock>) -> Result<FakeUpdates, AgentError> {
        self.0.prompt_calls.fetch_add(1, Ordering::SeqCst);
        if let Some(error) = self.0.prompt_errors.lock().unwrap().pop_front() {
            return Err(error);
        }
        let (tx, rx) = mpsc::unbounded_channel();
        *self.0.updates.lock().unwrap() = Some(tx);
        self.0.sent.store(0, Ordering::SeqCst);
        self.0.prompts.fetch_add(1, Ordering::SeqCst);
        Ok(FakeUpdates {
            ctl: self.0.clone(),
            rx,
            flood_done: false,
        })
    }

    async fn cancel(&self) -> Result<(), AgentError> {
        self.0.cancels.fetch_add(1, Ordering::SeqCst);
        Ok(())
    }

    async fn close(self) -> Result<(), AgentError> {
        self.0.closes.fetch_add(1, Ordering::SeqCst);
        Ok(())
    }
}

impl UpdateStream for FakeUpdates {
    async fn next(&mut self) -> Option<AgentUpdate> {
        if self.ctl.flood.load(Ordering::SeqCst) {
            if self.flood_done {
                return None;
            }
            if self.ctl.cancels.load(Ordering::SeqCst) > 0 {
                self.flood_done = true;
                return Some(AgentUpdate::Finished(TurnOutcome::Cancelled));
            }
            return Some(AgentUpdate::Text {
                offset: 0,
                text: "x".into(),
            });
        }
        self.rx.recv().await
    }
}

// ---------- 判定方：由测试逐个答复 ----------

#[derive(Default)]
struct Gated {
    waiting: Mutex<VecDeque<(TurnId, oneshot::Sender<PermissionDecision>)>>,
    voided: Mutex<Vec<(TurnId, VoidReason)>>,
    committed_events: Mutex<Vec<DomainEvent>>,
    /// 作废计划怎么给
    void_mode: Mutex<VoidMode>,
    /// 作废句柄被丢掉的次数
    holds_dropped: Arc<AtomicUsize>,
    /// 为真时 `committed` 永不返回
    committed_hangs: AtomicBool,
    /// `committed` 被调用那一刻运行时已收到的取消次数（看次序：先停运行时、后提醒）
    cancels_seen_by_committed: Mutex<Vec<usize>>,
    /// 测试把 `Ctl` 交进来，`committed` 才能看取消次数
    ctl: Mutex<Option<Arc<Ctl>>>,
}

/// 假判定方的作废计划。
#[derive(Default)]
enum VoidMode {
    /// 空计划
    #[default]
    Empty,
    /// 给这些事件
    Plan(Vec<DomainEvent>),
    /// 报这个错
    Fail(fn() -> VoidPlanError),
    /// 永不返回（令牌一直被占着）
    Hang,
}

/// 作废句柄：丢掉时记一次。
struct HoldProbe(Arc<AtomicUsize>);

impl Drop for HoldProbe {
    fn drop(&mut self) {
        self.0.fetch_add(1, Ordering::SeqCst);
    }
}

impl Gated {
    /// 答复第一个还在等的请求，返回它属于哪个回合。
    fn answer(&self, decision: PermissionDecision) -> TurnId {
        let (turn, tx) = self
            .waiting
            .lock()
            .unwrap()
            .pop_front()
            .expect("有在等的请求");
        let _ = tx.send(decision);
        turn
    }
}

impl PermissionDecider for Gated {
    type VoidHold = HoldProbe;

    async fn committed(&self, events: &[DomainEvent]) {
        if let Some(ctl) = self.ctl.lock().unwrap().as_ref() {
            self.cancels_seen_by_committed
                .lock()
                .unwrap()
                .push(ctl.cancels.load(Ordering::SeqCst));
        }
        self.committed_events
            .lock()
            .unwrap()
            .extend_from_slice(events);
        if self.committed_hangs.load(Ordering::SeqCst) {
            std::future::pending::<()>().await;
        }
    }
    async fn decide(&self, turn_id: TurnId, _req: &PermissionRequest) -> Decided {
        let (tx, rx) = oneshot::channel();
        self.waiting.lock().unwrap().push_back((turn_id, tx));
        Decided {
            decision: rx.await.expect("测试答复"),
            approval: None,
        }
    }

    async fn void_pending(
        &self,
        turn_id: TurnId,
        reason: VoidReason,
    ) -> Result<(Vec<DomainEvent>, HoldProbe), VoidPlanError> {
        self.voided.lock().unwrap().push((turn_id, reason));
        let plan = match &*self.void_mode.lock().unwrap() {
            VoidMode::Empty => Ok(Vec::new()),
            VoidMode::Plan(events) => Ok(events.clone()),
            VoidMode::Fail(error) => Err(error()),
            VoidMode::Hang => Err(VoidPlanError::Unavailable(String::new())),
        };
        if matches!(*self.void_mode.lock().unwrap(), VoidMode::Hang) {
            std::future::pending::<()>().await;
        }
        Ok((plan?, HoldProbe(self.holds_dropped.clone())))
    }
}

// ---------- 存储、时钟、ID、推送、定时器 ----------

#[derive(Default)]
struct Store {
    events: Mutex<Vec<DomainEvent>>,
    checkpoints: Mutex<Vec<String>>,
    commits: AtomicUsize,
    /// 第几次提交（从 1 数）返回 Busy
    fail_commit: Mutex<Option<usize>>,
    fail_checkpoint: AtomicBool,
    /// 接下来这么多笔带 `ApprovalVoided` 的提交按状态前提不成立拒掉
    reject_voids: AtomicUsize,
    /// 为真时带 `ApprovalVoided` 的提交永不返回
    hang_voids: AtomicBool,
}

impl Store {
    /// 某个回合的状态、会话绑定、助手消息与结束事件，渲染成便于比对的短文本。
    fn trail_of(&self, turn: TurnId) -> Vec<String> {
        let events = self.events.lock().unwrap();
        // MessageCompleted 不带回合，按本回合追加的助手消息认
        let messages: Vec<MessageId> = events
            .iter()
            .filter_map(|e| match e {
                DomainEvent::MessageAppended(m) if m.message.turn_id == Some(turn) => {
                    Some(m.message.id)
                }
                _ => None,
            })
            .collect();
        events
            .iter()
            .filter_map(|e| match e {
                DomainEvent::TurnSessionBound(b) if b.turn_id == turn => {
                    Some(format!("bound {}", b.runtime_session_id))
                }
                DomainEvent::TurnStateChanged(c) if c.turn_id == turn => {
                    Some(format!("{:?} -> {:?}", c.from, c.to))
                }
                DomainEvent::MessageAppended(m) if m.message.turn_id == Some(turn) => {
                    Some(format!("message {:?}", m.message.status))
                }
                DomainEvent::MessageCompleted(m) if messages.contains(&m.message_id) => {
                    Some(format!("completed {:?}", m.status))
                }
                DomainEvent::TurnFinished(f) if f.turn_id == turn => {
                    Some(format!("finished {:?}", f.state))
                }
                _ => None,
            })
            .collect()
    }

    /// 某个回合最后一次状态变化后的状态。
    fn state_of(&self, turn: TurnId) -> Option<TurnState> {
        self.events
            .lock()
            .unwrap()
            .iter()
            .rev()
            .find_map(|e| match e {
                DomainEvent::TurnStateChanged(c) if c.turn_id == turn => Some(c.to),
                _ => None,
            })
    }

    fn checkpoints(&self) -> Vec<String> {
        self.checkpoints.lock().unwrap().clone()
    }
}

impl StoragePort for Store {
    async fn commit(&self, req: CommitRequest) -> Result<CommitReceipt, StorageError> {
        let n = self.commits.fetch_add(1, Ordering::SeqCst) + 1;
        if *self.fail_commit.lock().unwrap() == Some(n) {
            return Err(StorageError::Busy);
        }
        let voids = req
            .events
            .iter()
            .any(|e| matches!(e, DomainEvent::ApprovalVoided(_)));
        if voids && self.hang_voids.load(Ordering::SeqCst) {
            std::future::pending::<()>().await;
        }
        if voids
            && self
                .reject_voids
                .try_update(Ordering::SeqCst, Ordering::SeqCst, |n| n.checked_sub(1))
                .is_ok()
        {
            return Err(StorageError::Projection {
                seq: Seq(0),
                message: "审批已不在待审批".into(),
            });
        }
        let mut events = self.events.lock().unwrap();
        events.extend(req.events);
        let seq = Seq(events.len() as u64);
        Ok(CommitReceipt {
            first_seq: seq,
            last_seq: seq,
            replayed: false,
        })
    }
    async fn head_seq(&self) -> Result<Seq, StorageError> {
        unimplemented!()
    }
    async fn read_events(
        &self,
        _f: EventFilter,
        _from: Seq,
        _limit: usize,
    ) -> Result<Vec<StoredEvent>, StorageError> {
        unimplemented!()
    }
    async fn checkpoint_message(
        &self,
        _id: MessageId,
        _offset: u64,
        text: &str,
    ) -> Result<(), StorageError> {
        if self.fail_checkpoint.load(Ordering::SeqCst) {
            return Err(StorageError::Io {
                message: "磁盘满".into(),
            });
        }
        self.checkpoints.lock().unwrap().push(text.to_owned());
        Ok(())
    }
    async fn outbox_pending(&self, _limit: usize) -> Result<Vec<OutboxItem>, StorageError> {
        unimplemented!()
    }
    async fn outbox_requeue_dispatched(
        &self,
        _redeliver_prefixes: Vec<String>,
        _fail_prefixes: Vec<String>,
    ) -> Result<iwork_ports::store::RequeueReport, StorageError> {
        unimplemented!()
    }
    async fn outbox_pending_where(
        &self,
        _selector: iwork_ports::store::OutboxSelector,
        _limit: usize,
    ) -> Result<Vec<OutboxItem>, StorageError> {
        unimplemented!()
    }
    async fn outbox_mark(&self, _id: OutboxId, _s: DeliveryState) -> Result<(), StorageError> {
        unimplemented!()
    }
    async fn rebuild_projections(&self) -> Result<RebuildReport, StorageError> {
        unimplemented!()
    }
}

struct RealClock(Instant);

impl Clock for RealClock {
    fn now(&self) -> Timestamp {
        Timestamp(self.0.elapsed().as_millis() as i64)
    }
    fn monotonic(&self) -> MonotonicInstant {
        MonotonicInstant(self.0.elapsed().as_nanos() as u64)
    }
}

#[derive(Default)]
struct Ids(AtomicUsize);

impl IdGenerator for Ids {
    fn next(&self) -> RawUlid {
        RawUlid::from_u128(self.0.fetch_add(1, Ordering::SeqCst) as u128 + 1000)
    }
}

#[derive(Default)]
struct Live(AtomicUsize);

impl LivePublisher for Live {
    fn message_delta(&self, _delta: MessageDelta) {
        self.0.fetch_add(1, Ordering::SeqCst);
    }
}

struct TokioTimer;

impl Timer for TokioTimer {
    async fn sleep(&self, duration: Duration) {
        tokio::time::sleep(duration).await;
    }
}

// ---------- 测试台 ----------

struct Bench {
    ctl: Arc<Ctl>,
    decider: Arc<Gated>,
    store: Store,
    clock: RealClock,
    ids: Ids,
    live: Live,
    timer: TokioTimer,
}

type Line = SessionLine<FakeAgent, Gated>;

impl Bench {
    fn new() -> Arc<Self> {
        Arc::new(Self {
            ctl: Arc::new(Ctl::default()),
            decider: Arc::new(Gated::default()),
            store: Store::default(),
            clock: RealClock(Instant::now()),
            ids: Ids::default(),
            live: Live::default(),
            timer: TokioTimer,
        })
    }

    fn line(&self) -> (Line, TurnCanceller) {
        let line = SessionLine::new(self.decider.clone(), None);
        let canceller = line.canceller();
        (line, canceller)
    }

    fn plan(&self) -> TurnPlan {
        let id = |n| RawUlid::from_u128(self.ids.0.fetch_add(1, Ordering::SeqCst) as u128 + n);
        TurnPlan {
            turn: Turn {
                id: TurnId::from_raw(id(1)),
                conversation_id: ConversationId::from_raw(id(1)),
                assistant_id: AssistantId::from_raw(id(1)),
                assistant_version: 1,
                runtime_id: RuntimeId::from_raw(id(1)),
                model: ModelSelection {
                    model_id: ModelId("m".into()),
                    effort: None,
                },
                runtime_session_id: None,
                kind: TurnKind::Assistant,
                state: TurnState::Queued,
                started_at: Timestamp(0),
                ended_at: None,
            },
            assistant_name: "小助手".into(),
            context_project_id: None,
            spec: SessionSpec {
                cwd: "/work".into(),
                model: ModelSelection {
                    model_id: ModelId("m".into()),
                    effort: None,
                },
                system_prompt: String::new(),
                mcp: Vec::new(),
                constraints: SessionConstraints {
                    deny_read_write: Vec::new(),
                    read_only: Vec::new(),
                    allow_write: AllowWrite::Anywhere,
                    excluded_commands: Vec::new(),
                    allow_unsandboxed_retry: false,
                    network: NetworkPolicy::Open,
                },
                tool_set: SessionToolSet {
                    enabled_groups: ToolGroups::new(),
                    include_always_on: true,
                },
                disabled_tools: Vec::new(),
                plugin_dirs: Vec::new(),
                resume: None,
                kind: TurnKind::Assistant,
            },
            blocks: vec![ContentBlock::Text("hi".into())],
        }
    }

    /// 在后台跑一个回合，把会话线还回来。
    fn spawn(
        self: &Arc<Self>,
        mut line: Line,
        plan: TurnPlan,
    ) -> tokio::task::JoinHandle<(Result<TurnState, TurnRunError>, Line)> {
        let b = self.clone();
        tokio::spawn(async move {
            let ports = TurnPorts {
                store: &b.store,
                clock: &b.clock,
                ids: &b.ids,
                live: &b.live,
                timer: &b.timer,
            };
            let result = line.run_turn(&FakeAgent(b.ctl.clone()), ports, plan).await;
            (result, line)
        })
    }

    async fn wait_state(&self, turn: TurnId, state: TurnState) {
        wait_until(|| self.store.state_of(turn) == Some(state)).await;
    }

    async fn wait_prompts(&self, n: usize) {
        wait_until(|| self.ctl.prompts.load(Ordering::SeqCst) >= n).await;
    }
}

async fn wait_until(mut ok: impl FnMut() -> bool) {
    let start = Instant::now();
    while !ok() {
        assert!(
            start.elapsed() < Duration::from_secs(5),
            "5 秒内条件仍不成立"
        );
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
}

fn gone() -> PermissionDecision {
    PermissionDecision::Deny {
        message: TURN_GONE_MESSAGE.into(),
    }
}

// ---------- F-8：审批结果绑定回合 ----------

/// 回合 A 等审批时失败、会话保留；回合 B 也在等审批。A 迟到的放行不能落到 B 头上：
/// 运行时拿到拒绝，B 仍在等审批；B 自己的结果照常生效。
#[tokio::test(flavor = "multi_thread")]
async fn late_answer_from_previous_turn_is_rejected() {
    let b = Bench::new();
    let (line, _) = b.line();
    let plan_a = b.plan();
    let a = plan_a.turn.id;
    let task = b.spawn(line, plan_a);
    b.wait_prompts(1).await;
    let ask_a = b.ctl.ask();
    b.wait_state(a, TurnState::WaitingApproval).await;
    b.ctl.send(AgentUpdate::Failed(AgentError::Protocol {
        message: "坏了".into(),
    }));
    let (result, line) = task.await.unwrap();
    assert_eq!(result.unwrap(), TurnState::Failed);

    let plan_b = b.plan();
    let turn_b = plan_b.turn.id;
    let task = b.spawn(line, plan_b);
    b.wait_prompts(2).await;
    let ask_b = b.ctl.ask();
    b.wait_state(turn_b, TurnState::WaitingApproval).await;

    assert_eq!(b.decider.answer(PermissionDecision::Allow), a);
    assert_eq!(ask_a.await.unwrap(), gone(), "A 迟到的放行被拒");
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert_eq!(
        b.store.state_of(turn_b),
        Some(TurnState::WaitingApproval),
        "B 不受影响"
    );

    assert_eq!(b.decider.answer(PermissionDecision::Allow), turn_b);
    assert_eq!(ask_b.await.unwrap(), PermissionDecision::Allow);
    b.wait_state(turn_b, TurnState::Running).await;
    b.ctl.send(AgentUpdate::Finished(TurnOutcome::Completed));
    assert_eq!(task.await.unwrap().0.unwrap(), TurnState::Completed);
    assert_eq!(
        *b.decider.voided.lock().unwrap(),
        [(a, VoidReason::TurnFailed)]
    );
}

/// 取消中到达的审批结果：拒绝，且回合不被拉回 Running。
#[tokio::test(flavor = "multi_thread")]
async fn answer_arriving_while_cancelling_is_rejected() {
    let b = Bench::new();
    let (line, canceller) = b.line();
    let plan = b.plan();
    let t = plan.turn.id;
    let task = b.spawn(line, plan);
    b.wait_prompts(1).await;
    let ask = b.ctl.ask();
    b.wait_state(t, TurnState::WaitingApproval).await;
    canceller.cancel(t);
    b.wait_state(t, TurnState::Cancelling).await;

    b.decider.answer(PermissionDecision::Allow);
    assert_eq!(ask.await.unwrap(), gone());
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert_eq!(b.store.state_of(t), Some(TurnState::Cancelling));

    b.ctl.send(AgentUpdate::Finished(TurnOutcome::Cancelled));
    assert_eq!(task.await.unwrap().0.unwrap(), TurnState::Cancelled);
}

// ---------- 第 2 轮 F-1：审批的前序边界 ----------

/// 更新流里积压了远超一批的文字，后面跟着「工具调用开始」，然后才来权限请求：
/// 「工具调用开始」一定先于「等待审批」落盘。
#[tokio::test(flavor = "multi_thread")]
async fn tool_start_precedes_waiting_even_behind_a_large_backlog() {
    let b = Bench::new();
    let (line, canceller) = b.line();
    let plan = b.plan();
    let t = plan.turn.id;
    let task = b.spawn(line, plan);
    b.wait_prompts(1).await;
    for i in 0..4096u64 {
        b.ctl.send(AgentUpdate::Text {
            offset: i,
            text: "x".into(),
        });
    }
    b.ctl.send(AgentUpdate::ToolCallStarted {
        tool_call_id: ToolCallId("t1".into()),
        tool: ToolRef("Bash".into()),
        raw_input: RawToolInput("{}".into()),
    });
    let ask = b.ctl.ask();
    b.wait_state(t, TurnState::WaitingApproval).await;
    let order: Vec<&str> = b
        .store
        .events
        .lock()
        .unwrap()
        .iter()
        .filter_map(|e| match e {
            DomainEvent::ToolCallStarted(_) => Some("tool"),
            DomainEvent::TurnStateChanged(c) if c.to == TurnState::WaitingApproval => {
                Some("waiting")
            }
            _ => None,
        })
        .collect();
    assert_eq!(order, ["tool", "waiting"]);
    assert_eq!(
        b.live.0.load(Ordering::SeqCst),
        4096,
        "积压的文字都在受理审批之前处理完"
    );

    canceller.cancel(t);
    b.ctl.send(AgentUpdate::Finished(TurnOutcome::Cancelled));
    assert_eq!(task.await.unwrap().0.unwrap(), TurnState::Cancelled);
    b.decider.answer(PermissionDecision::Allow);
    assert_eq!(ask.await.unwrap(), gone());
}

// ---------- F-9：取消优先 ----------

/// 运行时不停输出、更新流永远就绪：取消照样被处理。
#[tokio::test(flavor = "multi_thread")]
async fn cancel_is_handled_while_updates_keep_flowing() {
    let b = Bench::new();
    b.ctl.flood.store(true, Ordering::SeqCst);
    let (line, canceller) = b.line();
    let plan = b.plan();
    let t = plan.turn.id;
    let task = b.spawn(line, plan);
    wait_until(|| b.live.0.load(Ordering::SeqCst) > 1000).await;
    canceller.cancel(t);
    let (result, _) = tokio::time::timeout(Duration::from_secs(5), task)
        .await
        .expect("持续输出时取消也要在 5 秒内生效")
        .unwrap();
    assert_eq!(result.unwrap(), TurnState::Cancelled);
}

/// 取消与「完成」同时就绪：先处理取消，回合按取消收尾，取消不会被丢掉。
#[tokio::test(flavor = "current_thread")]
async fn cancel_ready_together_with_finish_wins() {
    let b = Bench::new();
    let (line, canceller) = b.line();
    let plan = b.plan();
    let t = plan.turn.id;
    let task = b.spawn(line, plan);
    b.wait_state(t, TurnState::Running).await;
    // 单线程运行时：两件事都在编排下一次被调度前就绪
    b.ctl.send(AgentUpdate::Finished(TurnOutcome::Completed));
    canceller.cancel(t);
    assert_eq!(task.await.unwrap().0.unwrap(), TurnState::Cancelled);
    assert_eq!(b.ctl.cancels.load(Ordering::SeqCst), 1);
}

// ---------- F-10：编排出错时收尾 ----------

/// 各个提交阶段出存储错误：原样交给上层，会话被关闭，回合信箱撤掉（迟到的权限回调被拒）。
#[tokio::test(flavor = "multi_thread")]
async fn storage_failure_at_any_stage_stops_the_runtime() {
    enum Stage {
        Started,
        ToolCall,
        PermissionAsked,
        Checkpoint,
        Finish,
    }
    for stage in [
        Stage::Started,
        Stage::ToolCall,
        Stage::PermissionAsked,
        Stage::Checkpoint,
        Stage::Finish,
    ] {
        let b = Bench::new();
        match stage {
            Stage::Started => *b.store.fail_commit.lock().unwrap() = Some(1),
            Stage::ToolCall | Stage::PermissionAsked | Stage::Finish => {
                *b.store.fail_commit.lock().unwrap() = Some(2)
            }
            Stage::Checkpoint => b.store.fail_checkpoint.store(true, Ordering::SeqCst),
        }
        let (line, _) = b.line();
        let task = b.spawn(line, b.plan());
        // 「开始执行」写不进去时 prompt 不发：库里还是 Queued，就一定没发过 prompt
        let expected_prompts = match stage {
            Stage::Started => 0,
            _ => 1,
        };
        if expected_prompts > 0 {
            b.wait_prompts(expected_prompts).await;
        }
        let mut pending_ask = None;
        match stage {
            Stage::Started => {}
            Stage::ToolCall => b.ctl.send(AgentUpdate::ToolCallStarted {
                tool_call_id: ToolCallId("t1".into()),
                tool: ToolRef("Bash".into()),
                raw_input: RawToolInput("{}".into()),
            }),
            Stage::PermissionAsked => pending_ask = Some(b.ctl.ask()),
            Stage::Checkpoint => b.ctl.send(AgentUpdate::Text {
                offset: 0,
                text: "a".into(),
            }),
            Stage::Finish => b.ctl.send(AgentUpdate::Finished(TurnOutcome::Completed)),
        }
        let (result, line) = task.await.unwrap();
        match result {
            Err(TurnRunError::Storage(StorageError::Busy | StorageError::Io { .. })) => {}
            other => panic!("要原样交回存储错误：{other:?}"),
        }
        assert_eq!(
            *b.decider.committed_events.lock().unwrap(),
            *b.store.events.lock().unwrap(),
            "确认回调只包含成功提交的事务"
        );
        assert_eq!(b.ctl.closes.load(Ordering::SeqCst), 1, "运行时被关闭");
        assert!(line.session().is_none(), "不能继续用的会话已清掉");
        if let Some(ask) = pending_ask {
            assert_eq!(ask.await.unwrap(), gone(), "挂着的权限请求被拒");
        }
        assert_eq!(b.ctl.ask().await.unwrap(), gone(), "迟到的权限回调被拒");
        assert_eq!(
            b.ctl.prompts.load(Ordering::SeqCst),
            expected_prompts,
            "不重发 prompt"
        );
    }
}

// ---------- F-11：定时检查点 ----------

/// 最后一段正文之后长时间没有新文字（这里是在等审批）：500ms 内也要落盘。
#[tokio::test(flavor = "multi_thread")]
async fn trailing_text_is_checkpointed_without_further_updates() {
    let b = Bench::new();
    let (line, canceller) = b.line();
    let plan = b.plan();
    let t = plan.turn.id;
    let task = b.spawn(line, plan);
    b.wait_prompts(1).await;
    b.ctl.send(AgentUpdate::Text {
        offset: 0,
        text: "a".into(),
    });
    wait_until(|| b.store.checkpoints() == ["a"]).await;
    tokio::time::sleep(Duration::from_millis(100)).await;
    b.ctl.send(AgentUpdate::Text {
        offset: 1,
        text: "b".into(),
    });
    let ask = b.ctl.ask();
    b.wait_state(t, TurnState::WaitingApproval).await;
    let sent = Instant::now();
    wait_until(|| b.store.checkpoints().last().map(String::as_str) == Some("ab")).await;
    assert!(
        sent.elapsed() < Duration::from_millis(600),
        "{:?}",
        sent.elapsed()
    );

    canceller.cancel(t);
    b.ctl.send(AgentUpdate::Finished(TurnOutcome::Cancelled));
    assert_eq!(task.await.unwrap().0.unwrap(), TurnState::Cancelled);
    // 判定方此时才答复：回合已结束，运行时拿到拒绝
    b.decider.answer(PermissionDecision::Allow);
    assert_eq!(ask.await.unwrap(), gone());
}

// ---------- T07 E5：作废计划带句柄、记账有期限、提醒不挡停止（T07 方案 6.10.8） ----------

fn void_event() -> DomainEvent {
    iwork_domain::event::ApprovalVoided {
        approval_id: iwork_domain::id::ApprovalId::from_raw(RawUlid::from_u128(77)),
        reason: VoidReason::TurnCancelled,
    }
    .into()
}

/// 跑到「运行时已 prompt」，返回回合 id、任务与取消句柄。
async fn running(
    b: &Arc<Bench>,
) -> (
    TurnId,
    tokio::task::JoinHandle<(Result<TurnState, TurnRunError>, Line)>,
    TurnCanceller,
) {
    let (line, canceller) = b.line();
    let plan = b.plan();
    let t = plan.turn.id;
    let task = b.spawn(line, plan);
    b.wait_prompts(1).await;
    b.wait_state(t, TurnState::Running).await;
    (t, task, canceller)
}

fn has_cancelling(b: &Bench, t: TurnId) -> bool {
    b.store.events.lock().unwrap().iter().any(|e| {
        matches!(e, DomainEvent::TurnStateChanged(c) if c.turn_id == t && c.to == TurnState::Cancelling)
    })
}

/// 作废计划过期（投影器拒）：回合自己的事件不重算，重新要作废那一段，第 3 次成功。
#[tokio::test(flavor = "multi_thread")]
async fn runner_replans_void_on_projection_error() {
    let b = Bench::new();
    *b.decider.void_mode.lock().unwrap() = VoidMode::Plan(vec![void_event()]);
    b.store.reject_voids.store(2, Ordering::SeqCst);
    let (t, task, canceller) = running(&b).await;
    canceller.cancel(t);
    b.wait_state(t, TurnState::Cancelling).await;
    assert_eq!(b.decider.voided.lock().unwrap().len(), 3, "要了 3 次计划");
    assert_eq!(b.decider.holds_dropped.load(Ordering::SeqCst), 3);
    assert_eq!(b.ctl.cancels.load(Ordering::SeqCst), 1);
    b.ctl.send(AgentUpdate::Finished(TurnOutcome::Cancelled));
    assert_eq!(task.await.unwrap().0.unwrap(), TurnState::Cancelled);
}

/// 被拒 3 次就放弃：按编排出错收场，会话被关闭，日志里没有 Cancelling。
#[tokio::test(flavor = "multi_thread")]
async fn runner_gives_up_after_three() {
    let b = Bench::new();
    *b.decider.void_mode.lock().unwrap() = VoidMode::Plan(vec![void_event()]);
    b.store.reject_voids.store(3, Ordering::SeqCst);
    let (t, task, canceller) = running(&b).await;
    canceller.cancel(t);
    let (result, _) = task.await.unwrap();
    assert!(matches!(
        result,
        Err(TurnRunError::Storage(StorageError::Projection { .. }))
    ));
    assert_eq!(b.ctl.closes.load(Ordering::SeqCst), 1);
    assert!(!has_cancelling(&b, t));
}

/// 算不出作废计划（屏障 Busy、读失败、折叠对不上）：`VoidPlan`，会话被关闭、信箱已撤，没有 Cancelling。
#[tokio::test(flavor = "multi_thread")]
async fn cancel_closes_session_when_void_plan_fails() {
    for error in [
        (|| VoidPlanError::Storage(StorageError::Busy)) as fn() -> VoidPlanError,
        || {
            VoidPlanError::Storage(StorageError::Io {
                message: "读失败".into(),
            })
        },
        || VoidPlanError::Unavailable("seq 9 的 approval_decided 对不上".into()),
    ] {
        let b = Bench::new();
        *b.decider.void_mode.lock().unwrap() = VoidMode::Fail(error);
        let (t, task, canceller) = running(&b).await;
        let ask = b.ctl.ask();
        b.wait_state(t, TurnState::WaitingApproval).await;
        canceller.cancel(t);
        let (result, _) = task.await.unwrap();
        assert!(
            matches!(result, Err(TurnRunError::VoidPlan(_))),
            "{result:?}"
        );
        assert_eq!(b.ctl.closes.load(Ordering::SeqCst), 1);
        assert!(!has_cancelling(&b, t));
        // 之后到的审批结果回执 false：运行时拿到拒绝
        b.decider.answer(PermissionDecision::Allow);
        assert_eq!(ask.await.unwrap(), gone());
    }
}

/// 令牌一直被占着（要计划永不返回）：5 秒后 `StopTimedOut{void_plan}`，会话被关闭。
#[tokio::test(flavor = "current_thread", start_paused = true)]
async fn cancel_closes_session_when_token_never_frees() {
    let b = Bench::new();
    *b.decider.void_mode.lock().unwrap() = VoidMode::Hang;
    let (t, task, canceller) = running(&b).await;
    canceller.cancel(t);
    let (result, _) = task.await.unwrap();
    assert!(matches!(
        result,
        Err(TurnRunError::StopTimedOut { step: "void_plan" })
    ));
    assert_eq!(b.ctl.closes.load(Ordering::SeqCst), 1);
}

/// 计划拿到了，编排那一笔提交永不返回：5 秒后 `StopTimedOut{void_commit}`，句柄已丢，会话被关闭。
#[tokio::test(flavor = "current_thread", start_paused = true)]
async fn cancel_closes_session_when_void_commit_never_returns() {
    let b = Bench::new();
    *b.decider.void_mode.lock().unwrap() = VoidMode::Plan(vec![void_event()]);
    b.store.hang_voids.store(true, Ordering::SeqCst);
    let (t, task, canceller) = running(&b).await;
    canceller.cancel(t);
    let (result, _) = task.await.unwrap();
    assert!(matches!(
        result,
        Err(TurnRunError::StopTimedOut {
            step: "void_commit"
        })
    ));
    assert_eq!(
        b.decider.holds_dropped.load(Ordering::SeqCst),
        1,
        "句柄已丢"
    );
    assert_eq!(b.ctl.closes.load(Ordering::SeqCst), 1);
}

/// 判定方的 `committed` 永不返回：作废提交一返回就停运行时（提醒在后），1 秒后提醒被丢，回合照常收尾。
#[tokio::test(flavor = "current_thread", start_paused = true)]
async fn cancel_stops_runtime_when_committed_never_returns() {
    let b = Bench::new();
    *b.decider.void_mode.lock().unwrap() = VoidMode::Plan(vec![void_event()]);
    *b.decider.ctl.lock().unwrap() = Some(b.ctl.clone());
    let (t, task, canceller) = running(&b).await;
    b.decider.committed_hangs.store(true, Ordering::SeqCst);
    canceller.cancel(t);
    b.wait_state(t, TurnState::Cancelling).await;
    wait_until(|| b.ctl.cancels.load(Ordering::SeqCst) == 1).await;
    assert_eq!(
        b.decider.cancels_seen_by_committed.lock().unwrap().last(),
        Some(&1),
        "提醒判定方时运行时已经被叫停"
    );
    b.ctl.send(AgentUpdate::Finished(TurnOutcome::Cancelled));
    assert_eq!(task.await.unwrap().0.unwrap(), TurnState::Cancelled);
    assert!(has_cancelling(&b, t));
    assert!(
        b.decider
            .committed_events
            .lock()
            .unwrap()
            .contains(&void_event())
    );
}

/// 不带作废的提交（PermissionAsked）之后 `committed` 永不返回：1 秒后编排继续、发出票；取消照样被处理。
#[tokio::test(flavor = "current_thread", start_paused = true)]
async fn nudge_timeout_does_not_stall_turn() {
    let b = Bench::new();
    let (t, task, canceller) = running(&b).await;
    b.decider.committed_hangs.store(true, Ordering::SeqCst);
    let started = tokio::time::Instant::now();
    let ask = b.ctl.ask();
    wait_until(|| !b.decider.waiting.lock().unwrap().is_empty()).await;
    assert!(
        started.elapsed() >= Duration::from_secs(1),
        "等满了提醒的期限才发票"
    );
    canceller.cancel(t);
    b.wait_state(t, TurnState::Cancelling).await;
    // 票已清：判定方迟到的结论被拒
    b.decider.answer(PermissionDecision::Allow);
    assert_eq!(ask.await.unwrap(), gone());
    b.ctl.send(AgentUpdate::Finished(TurnOutcome::Cancelled));
    assert_eq!(task.await.unwrap().0.unwrap(), TurnState::Cancelled);
}

/// 两个对话各一个回合，对话 1 的令牌卡死；同时取消：对话 2 正常收尾，对话 1 5 秒后关会话。
#[tokio::test(flavor = "current_thread", start_paused = true)]
async fn cancel_all_is_not_blocked_by_one_stuck_conversation() {
    let stuck = Bench::new();
    *stuck.decider.void_mode.lock().unwrap() = VoidMode::Hang;
    let fine = Bench::new();
    let (t1, task1, c1) = running(&stuck).await;
    let (t2, task2, c2) = running(&fine).await;
    c1.cancel(t1);
    c2.cancel(t2);
    fine.wait_state(t2, TurnState::Cancelling).await;
    assert_eq!(fine.ctl.cancels.load(Ordering::SeqCst), 1);
    fine.ctl.send(AgentUpdate::Finished(TurnOutcome::Cancelled));
    assert_eq!(task2.await.unwrap().0.unwrap(), TurnState::Cancelled);
    let (result, _) = task1.await.unwrap();
    assert!(matches!(result, Err(TurnRunError::StopTimedOut { .. })));
    assert_eq!(
        stuck.ctl.closes.load(Ordering::SeqCst),
        1,
        "卡住的那个也停了"
    );
}

// ---------- T07 E5 合入门槛：真实编排 + 真实引擎 + 假运行时（T07 方案 6.10.8「共同启用的门槛」） ----------

mod delivery_gate {
    use super::support::engine_fakes::{
        FakeReviewer, FakeTimer, Ids as EngineIds, MemStore, TickClock,
    };
    use super::support::fake_fs::FakeFs;
    use super::support::{WS, conversation, cp, env, fold_fx, fs, project, snapshot_on};
    use super::*;
    use iwork_core::approval::engine::{ApprovalEngine, EnginePorts, SessionKeys, UserDecision};
    use iwork_core::approval::session::tool_set_for;
    use iwork_domain::approval::ApprovalState;
    use iwork_domain::approval_mode::{ApprovalMode, RememberScope};
    use iwork_domain::common::Actor;
    use iwork_domain::event::{TurnQueued, TurnStateChanged};
    use iwork_domain::id::ApprovalId;
    use iwork_domain::project::AiReviewSettings;
    use iwork_domain::turn::InterruptReason;

    type Engine = ApprovalEngine<TickClock, EngineIds, MemStore, FakeReviewer, FakeTimer, FakeFs>;
    type RealLine = SessionLine<
        FakeAgent,
        iwork_core::approval::engine::SessionDecider<
            TickClock,
            EngineIds,
            MemStore,
            FakeReviewer,
            FakeTimer,
            FakeFs,
        >,
    >;

    fn assistant() -> AssistantId {
        super::support::assistant()
    }

    fn keys() -> SessionKeys {
        SessionKeys {
            conversation: conversation(),
            assistant: assistant(),
            runtime_id: RuntimeId::from_raw(RawUlid::from_u128(0x888)),
            assistant_version: 1,
            project: Some(project()),
            turn_kind: TurnKind::Assistant,
            snapshot: Arc::new(snapshot_on(
                ApprovalMode::Manual,
                &[],
                &env(),
                Seq(1),
                &fs(),
            )),
            tool_set: tool_set_for(TurnKind::Assistant, &ToolGroups::new()),
            disabled_groups: ToolGroups::new(),
            cwd: cp(WS),
            paths: env(),
            ai_review: AiReviewSettings {
                enabled: false,
                runtime_id: None,
                model: None,
                timeout_secs: 60,
            },
            review_model: ModelSelection {
                model_id: ModelId("m".into()),
                effort: None,
            },
        }
    }

    struct World {
        b: Arc<Bench>,
        store: MemStore,
        engine: Arc<Engine>,
    }

    /// 布景（项目、助手、对话、排队的回合）+ 真实引擎 + 真实会话线，跑起来等到运行时 prompt。
    async fn world() -> (
        World,
        TurnId,
        tokio::task::JoinHandle<Result<TurnState, TurnRunError>>,
    ) {
        let b = Bench::new();
        let store = MemStore::default();
        store.seed(fold_fx::world_with(
            project(),
            conversation(),
            &[assistant()],
            None,
        ));
        let mut plan = b.plan();
        plan.turn.conversation_id = conversation();
        plan.turn.assistant_id = assistant();
        let t = plan.turn.id;
        store.seed(vec![
            TurnQueued {
                turn: plan.turn.clone(),
            }
            .into(),
        ]);
        let (engine, recovery) = Engine::start(
            EnginePorts {
                clock: TickClock::default(),
                ids: EngineIds::default(),
                store: store.clone(),
                reviewer: FakeReviewer::default(),
                timer: FakeTimer::default(),
                fs: fs(),
            },
            Default::default(),
            |_, _| None,
        )
        .await
        .unwrap();
        assert!(recovery.is_empty());
        let mut line: RealLine = SessionLine::new(Arc::new(engine.decider_for(keys())), None);
        let task = {
            let b = b.clone();
            let store = store.clone();
            tokio::spawn(async move {
                let ports = TurnPorts {
                    store: &store,
                    clock: &b.clock,
                    ids: &b.ids,
                    live: &b.live,
                    timer: &b.timer,
                };
                line.run_turn(&FakeAgent(b.ctl.clone()), ports, plan).await
            })
        };
        b.wait_prompts(1).await;
        (World { b, store, engine }, t, task)
    }

    async fn requested(store: &MemStore) -> ApprovalId {
        let mut id = None;
        wait_until(|| {
            id = store.all().into_iter().find_map(|e| match e {
                DomainEvent::ApprovalRequested(r) => Some(r.approval.id),
                _ => None,
            });
            id.is_some()
        })
        .await;
        id.unwrap()
    }

    fn interrupted(t: TurnId, from: TurnState) -> DomainEvent {
        TurnStateChanged {
            turn_id: t,
            from,
            to: TurnState::Interrupted {
                reason: InterruptReason::ApprovalWaitLimit,
            },
            reason: None,
        }
        .into()
    }

    fn allow_once() -> UserDecision {
        UserDecision::Allow {
            remember: RememberScope::Once,
        }
    }

    /// 请求 → 用户允许 → 编排采纳时提交 `ApprovalDelivered` → 随后回合中断 → 没有一次性放行。
    #[tokio::test(flavor = "current_thread")]
    async fn delivery_event_committed_on_adoption() {
        let (w, t, task) = world().await;
        let ask = w.b.ctl.ask_bash(r#"{"command":"npm test"}"#);
        let id = requested(&w.store).await;
        w.engine
            .decide(id, allow_once(), Actor("user".into()))
            .await
            .unwrap();
        assert_eq!(ask.await.unwrap(), PermissionDecision::Allow);
        let events = w.store.all();
        let delivered = events
            .iter()
            .position(|e| matches!(e, DomainEvent::ApprovalDelivered(d) if d.approval_id == id && d.turn_id == t))
            .expect("编排采纳时提交了交付事件");
        assert!(
            matches!(&events[delivered + 1], DomainEvent::TurnStateChanged(c) if c.to == TurnState::Running),
            "没有别的票在等：和回到 Running 同一笔"
        );
        assert!(events.iter().any(|e| matches!(e, DomainEvent::ApprovalDecided(d) if d.approval_id == id && matches!(d.state, ApprovalState::Approved { .. }))));
        // 随后回合中断（扮演等待上限）
        w.store.seed(vec![interrupted(t, TurnState::Running)]);
        w.engine.sync().await.unwrap();
        assert!(w.engine.grant(id).is_none(), "交付过的批准不留一次性放行");
        assert!(w.engine.turns_to_resume().is_empty());
        w.b.ctl.send(AgentUpdate::Finished(TurnOutcome::Completed));
        task.await.unwrap().unwrap();
    }

    /// 对照：批准落盘了、编排的交付那笔还没入队，回合就中断了 → 一次性放行，并要求续接。
    #[tokio::test(flavor = "current_thread")]
    async fn undelivered_approval_becomes_grant_on_interrupt() {
        let (w, t, task) = world().await;
        let ask = w.b.ctl.ask_bash(r#"{"command":"npm test"}"#);
        let id = requested(&w.store).await;
        w.engine
            .decide(id, allow_once(), Actor("user".into()))
            .await
            .unwrap();
        // 单线程运行时：编排要等测试让出才动，交付那笔会停在入队门口
        let gate = Arc::new(tokio::sync::Notify::new());
        *w.store.enqueue_gate.lock().unwrap() = Some(gate);
        w.store
            .seed(vec![interrupted(t, TurnState::WaitingApproval)]);
        w.engine.sync().await.unwrap();
        wait_until(|| w.store.at_enqueue_gate.load(Ordering::SeqCst)).await;
        assert!(
            !w.store
                .all()
                .iter()
                .any(|e| matches!(e, DomainEvent::ApprovalDelivered(_))),
            "交付事件没有落盘"
        );
        assert_eq!(
            w.engine.grant(id).map(|g| g.turn),
            Some(t),
            "没交付的批准在中断时变成一次性放行"
        );
        assert_eq!(w.engine.turns_to_resume(), vec![t]);
        task.abort();
        ask.abort();
    }
}

// ---------- 回合已进 Running、prompt 发不出去：按失败收尾 ----------

/// 提交了「开始执行」之后 prompt 才失败，回合从 Running 收尾为 Failed（不再是 Queued -> Failed）。
fn failed_after_start() -> Vec<String> {
    [
        "bound fake",
        "Queued -> Running",
        "message Streaming",
        "Running -> Failed",
        "completed Failed",
        "finished Failed",
    ]
    .map(String::from)
    .to_vec()
}

fn exited() -> AgentError {
    AgentError::ProcessExited {
        message: "退出了".into(),
    }
}

/// 先跑完一个回合，让会话线留着一条会话（下一回合沿用它）。
async fn line_with_session(b: &Arc<Bench>) -> Line {
    let (line, _) = b.line();
    let task = b.spawn(line, b.plan());
    b.wait_prompts(1).await;
    b.ctl.send(AgentUpdate::Finished(TurnOutcome::Completed));
    let (result, line) = task.await.unwrap();
    assert_eq!(result.unwrap(), TurnState::Completed);
    assert!(line.session().is_some(), "对照：会话留着");
    line
}

/// ① 新开的会话上 prompt 报进程退出：不重试，回合失败；会话丢掉，续接 ID 留着。
#[tokio::test(flavor = "multi_thread")]
async fn prompt_failure_on_new_session_fails_running_turn() {
    let b = Bench::new();
    b.ctl.prompt_errors.lock().unwrap().push_back(exited());
    let (line, _) = b.line();
    let plan = b.plan();
    let t = plan.turn.id;
    let (result, line) = b.spawn(line, plan).await.unwrap();
    assert_eq!(result.unwrap(), TurnState::Failed);
    assert_eq!(b.store.trail_of(t), failed_after_start());
    assert_eq!(b.ctl.prompt_calls.load(Ordering::SeqCst), 1, "新会话不重试");
    assert_eq!(b.ctl.opens.load(Ordering::SeqCst), 1);
    assert!(line.session().is_none(), "进程退出的会话丢掉");
    assert_eq!(line.resume_id(), Some("fake"), "下一回合还能续接");
}

/// ② 沿用的会话进程已退出，续接时 open_session 失败：回合失败，没有第二次 prompt。
#[tokio::test(flavor = "multi_thread")]
async fn reopen_failure_after_exited_session_fails_running_turn() {
    let b = Bench::new();
    let line = line_with_session(&b).await;
    let calls_before = b.ctl.prompt_calls.load(Ordering::SeqCst);
    b.ctl.prompt_errors.lock().unwrap().push_back(exited());
    *b.ctl.fail_next_open.lock().unwrap() = Some(exited());
    let plan = b.plan();
    let t = plan.turn.id;
    let (result, line) = b.spawn(line, plan).await.unwrap();
    assert_eq!(result.unwrap(), TurnState::Failed);
    assert_eq!(b.store.trail_of(t), failed_after_start());
    assert_eq!(
        b.ctl.prompt_calls.load(Ordering::SeqCst) - calls_before,
        1,
        "只在死掉的会话上试过一次"
    );
    assert_eq!(b.ctl.opens.load(Ordering::SeqCst), 2, "续接试了一次");
    assert!(line.session().is_none());
    assert_eq!(line.resume_id(), Some("fake"));
}

/// ③ 续接成功，第二次 prompt 仍报进程退出：回合失败，不再续接第三次。
#[tokio::test(flavor = "multi_thread")]
async fn second_prompt_failure_after_reopen_fails_running_turn() {
    let b = Bench::new();
    let line = line_with_session(&b).await;
    let calls_before = b.ctl.prompt_calls.load(Ordering::SeqCst);
    b.ctl
        .prompt_errors
        .lock()
        .unwrap()
        .extend([exited(), exited()]);
    let plan = b.plan();
    let t = plan.turn.id;
    let (result, line) = b.spawn(line, plan).await.unwrap();
    assert_eq!(result.unwrap(), TurnState::Failed);
    assert_eq!(b.store.trail_of(t), failed_after_start());
    assert_eq!(b.ctl.prompt_calls.load(Ordering::SeqCst) - calls_before, 2);
    assert_eq!(b.ctl.opens.load(Ordering::SeqCst), 2);
    assert!(line.session().is_none());
    assert_eq!(line.resume_id(), Some("fake"));
}
