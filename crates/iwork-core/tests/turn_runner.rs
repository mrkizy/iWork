//! 回合编排对着一个可控的假运行时（进程内，不起进程）：审批结果绑定回合、取消优先、
//! 编排出错时收尾、定时检查点。这些都是时序问题，假运行时让每一步都由测试决定。

use std::collections::VecDeque;
use std::future::Future;
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use iwork_core::turn::{
    PermissionDecider, SessionLine, TURN_GONE_MESSAGE, TurnCanceller, TurnPlan, TurnPorts,
    TurnRunError,
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
    PermissionHandler, PermissionRequest, RawToolInput, RuntimeCapabilities, SessionSpec,
    TurnOutcome, UpdateStream,
};
use iwork_ports::clock::{Clock, IdGenerator, MonotonicInstant, Timer};
use iwork_ports::live::{LivePublisher, MessageDelta};
use iwork_ports::store::{
    CommitReceipt, CommitRequest, DeliveryState, EventFilter, OutboxId, OutboxItem, RebuildReport,
    StorageError, StoragePort, StoredEvent,
};
use tokio::sync::{mpsc, oneshot};

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
    cancels: AtomicUsize,
    closes: AtomicUsize,
    /// 为真时更新流永远就绪（不停输出文字），直到收到取消
    flood: AtomicBool,
}

impl Ctl {
    fn send(&self, update: AgentUpdate) {
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
        let handler = self.handler.lock().unwrap().clone().expect("已开会话");
        tokio::spawn(handler(PermissionRequest {
            tool: ToolRef("Bash".into()),
            raw_input: RawToolInput("{}".into()),
            bypass_sandbox: false,
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

    async fn prompt(&self, _blocks: Vec<ContentBlock>) -> Result<FakeUpdates, AgentError> {
        let (tx, rx) = mpsc::unbounded_channel();
        *self.0.updates.lock().unwrap() = Some(tx);
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
    async fn decide(&self, turn_id: TurnId, _req: &PermissionRequest) -> PermissionDecision {
        let (tx, rx) = oneshot::channel();
        self.waiting.lock().unwrap().push_back((turn_id, tx));
        rx.await.expect("测试答复")
    }

    async fn void_pending(&self, turn_id: TurnId, reason: VoidReason) -> Vec<DomainEvent> {
        self.voided.lock().unwrap().push((turn_id, reason));
        Vec::new()
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
}

impl Store {
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
        b.wait_prompts(1).await;
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
        assert_eq!(b.ctl.closes.load(Ordering::SeqCst), 1, "运行时被关闭");
        assert!(line.session().is_none(), "不能继续用的会话已清掉");
        if let Some(ask) = pending_ask {
            assert_eq!(ask.await.unwrap(), gone(), "挂着的权限请求被拒");
        }
        assert_eq!(b.ctl.ask().await.unwrap(), gone(), "迟到的权限回调被拒");
        assert_eq!(b.ctl.prompts.load(Ordering::SeqCst), 1, "不重发 prompt");
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
