//! 回合编排的最小闭环（技术设计 6.4、3.5）：开会话或续接 → 提交「开始执行」→ prompt → 把更新流转成
//! 领域事件写入存储、把正文增量推出去 → 按回合转换表推进状态 → 结束。
//!
//! 派发顺序守着重启后核对的两条事实（需求 12「不静默重放」；T04 方案 1.1 的 P1、P2）：
//! 1. 先确保有会话（新开或续接），拿到运行时会话 ID；
//! 2. 一次提交写下会话绑定（`TurnSessionBound`）、回合进 `Running`、空的助手消息；
//! 3. 然后才发 prompt。
//!
//! 所以库里是 `Queued` 的回合一定没发过 prompt，`Running` 及之后的回合会话 ID 已落盘（运行时报不出
//! ID 时除外）。第 1、2 步之间崩溃：回合还是 `Queued`、没有 ID，运行时只开了会话、没收到 prompt，
//! 重启后照常派发是安全的。prompt 发不出去（如进程已退出且续接失败）时回合已是 `Running`，按失败收尾。
//!
//! 回合状态只在 [`SessionLine::run_turn`] 的循环里写。它同时等四路输入，优先次序：
//! 1. **取消**：最先处理，更新再多也不会被饿死；回合结束前还会再看一眼有没有已收到的取消；
//! 2. **检查点定时**：上次检查点之后最多 500ms 把未落盘的正文刷出去，即使没有新的更新；
//! 3. **更新流**与**权限信箱**：一般先更新、后信箱；连续处理 [`FAIR_BURST`] 条更新后先让出一次
//!    执行权、再先看一次信箱。
//!    「开始等审批」带着前序边界（适配器给出的「这次请求之前已交出的更新条数」）：编排先把本回合
//!    的更新处理到这个条数再受理，不管积压多少，「工具调用开始」都排在「等待审批」之前。等边界
//!    期间取消照样优先、照样按批让出。
//!
//! 运行时进程意外退出（后台服务还活着）时回合怎么收尾：
//! - `Running` 或 `WaitingApproval`：回合 `Failed`，原因是进程退出（`AgentError::ProcessExited`
//!   可原样重试），助手消息 `Failed`；
//! - `Cancelling`：本来就要取消，算 `Cancelled`。
//!
//! `Interrupted(Crash)` 只留给后台服务重启后发现进程已不在的情况（崩溃恢复，不在这里）。
//! 进程退出后丢掉这条会话，下一回合用记下的运行时会话 id 续接。
//!
//! 编排自己出错（写不进存储、表外转换）时：先撤掉回合信箱（迟到的审批回调一律被拒），再关闭会话
//! （结束运行时，免得工具继续改文件），然后把原始错误交给上层；不重发 prompt。

use std::collections::BTreeSet;
use std::future::{Future, poll_fn};
use std::pin::{Pin, pin};
use std::sync::Arc;
use std::task::{Context, Poll};
use std::time::Duration;

use futures_channel::mpsc;
use futures_core::Stream;
use iwork_domain::action::{Action, ActionFlags, ActionKind, ActionTarget};
use iwork_domain::common::{Actor, Usage};
use iwork_domain::event::{
    DomainEvent, MessageAppended, MessageCompleted, QuotaObserved, ToolCallFinished,
    ToolCallStarted, TurnFinished, TurnModelReported, TurnSessionBound, TurnStateChanged,
    UsageReported,
};
use iwork_domain::id::{CommandId, MessageId, ProjectId, TurnId};
use iwork_domain::message::{Author, Message, MessageBody, MessageStatus};
use iwork_domain::turn::{Turn, TurnIllegal, TurnState, TurnTrigger};
use iwork_ports::agent::{
    AgentError, AgentPort, AgentSession, AgentUpdate, ContentBlock, SessionSpec, TurnOutcome,
    UpdateStream,
};
use iwork_ports::clock::{Clock, IdGenerator, MonotonicInstant, Timer};
use iwork_ports::live::{LivePublisher, MessageDelta};
use iwork_ports::store::{CommitRequest, StorageError, StoragePort};

use super::permission::{
    PermissionDecider, PermissionGate, Signal, Ticket, TurnInbox, TurnSlot, new_ticket,
};

/// 回合编排写入的事件以它为 actor：这些事件由后台服务按运行时的输出产生，不是用户的命令。
pub const ORCHESTRATOR_ACTOR: &str = "system";

/// 流式正文的检查点间隔（技术设计 3.5：崩溃后最多丢这么久的显示文字）。
pub const CHECKPOINT_INTERVAL: Duration = Duration::from_millis(500);

/// 连续处理这么多条更新后，先让出一次执行权、再先看一次权限信箱。
pub const FAIR_BURST: u32 = 32;

/// 动作摘要最多保留的字符数。
const SUMMARY_CHARS: usize = 200;

/// 回合编排用到的端口。
#[derive(Debug)]
pub struct TurnPorts<'a, S, C, I, L, T> {
    pub store: &'a S,
    pub clock: &'a C,
    pub ids: &'a I,
    pub live: &'a L,
    pub timer: &'a T,
}

/// 一个回合要做什么。`turn` 处于 `Queued`，`TurnQueued` 已由受理命令的一方写入。
#[derive(Debug, Clone)]
pub struct TurnPlan {
    pub turn: Turn,
    /// 助手当时的名字，写进助手消息的作者快照
    pub assistant_name: String,
    pub context_project_id: Option<ProjectId>,
    /// 开会话用；`resume` 由会话线自己填（它记着上一条运行时会话 id）
    pub spec: SessionSpec,
    pub blocks: Vec<ContentBlock>,
}

/// 编排失败：写不进存储，或碰到转换表外的转换（程序缺陷）。回合本身的失败不在这里，
/// 而是回合以 `Failed` 等状态正常收尾。出这种错时会话已被关闭。
#[derive(Debug, thiserror::Error)]
pub enum TurnRunError {
    #[error(transparent)]
    Storage(#[from] StorageError),
    #[error(transparent)]
    Illegal(#[from] TurnIllegal),
}

/// 一条运行时会话线（技术设计 6.4）：一个对话（或「助手 × 项目」）对应一条，跨回合续接。
pub struct SessionLine<A: AgentPort, D> {
    session: Option<A::Session>,
    /// 最近一次的运行时会话 id；会话没了（进程退出、取消后、后台服务重启）时用它续接
    resume: Option<String>,
    slot: Arc<TurnSlot>,
    decider: Arc<D>,
}

/// 取消某个回合的句柄，可在任何任务里用。
#[derive(Debug, Clone)]
pub struct TurnCanceller {
    slot: Arc<TurnSlot>,
}

impl TurnCanceller {
    /// 请求取消。回合已经结束或换了就什么都不做。
    pub fn cancel(&self, turn_id: TurnId) {
        if let Some(inbox) = self.slot.current(Some(turn_id)) {
            let _ = inbox.cancel.unbounded_send(());
        }
    }
}

/// 回合信箱的生命周期：回合开始时打开，函数以任何方式返回时撤掉。
struct SlotGuard<'a>(&'a TurnSlot);

impl Drop for SlotGuard<'_> {
    fn drop(&mut self) {
        self.0.close();
    }
}

impl<A: AgentPort, D: PermissionDecider> SessionLine<A, D> {
    /// `resume`：已知的运行时会话 id（如后台服务重启前记下的），没有就新开。
    pub fn new(decider: Arc<D>, resume: Option<String>) -> Self {
        Self {
            session: None,
            resume,
            slot: Arc::new(TurnSlot::default()),
            decider,
        }
    }

    pub fn canceller(&self) -> TurnCanceller {
        TurnCanceller {
            slot: self.slot.clone(),
        }
    }

    /// 当前打开的会话；没有打开时为 `None`。
    pub fn session(&self) -> Option<&A::Session> {
        self.session.as_ref()
    }

    /// 下一回合续接用的运行时会话 id。
    pub fn resume_id(&self) -> Option<&str> {
        self.resume.as_deref()
    }

    /// 跑一个回合，返回它停在的状态。返回 `Err` 时会话已关闭，回合信箱已撤。
    pub async fn run_turn<S, C, I, L, T>(
        &mut self,
        agent: &A,
        ports: TurnPorts<'_, S, C, I, L, T>,
        plan: TurnPlan,
    ) -> Result<TurnState, TurnRunError>
    where
        S: StoragePort,
        C: Clock,
        I: IdGenerator,
        L: LivePublisher,
        T: Timer,
    {
        let result = self.run_turn_inner(agent, ports, plan).await;
        if result.is_err()
            && let Some(session) = self.session.take()
        {
            // 信箱已随 run_turn_inner 返回撤掉；关掉运行时，免得它在无人记录时继续动手
            let _ = session.close().await;
        }
        result
    }

    async fn run_turn_inner<S, C, I, L, T>(
        &mut self,
        agent: &A,
        ports: TurnPorts<'_, S, C, I, L, T>,
        plan: TurnPlan,
    ) -> Result<TurnState, TurnRunError>
    where
        S: StoragePort,
        C: Clock,
        I: IdGenerator,
        L: LivePublisher,
        T: Timer,
    {
        let TurnPlan {
            turn,
            assistant_name,
            context_project_id,
            spec,
            blocks,
        } = plan;
        let (signals_tx, mut signals) = mpsc::unbounded();
        let (cancel_tx, mut cancels) = mpsc::unbounded();
        self.slot.open(TurnInbox {
            turn_id: turn.id,
            signals: signals_tx,
            cancel: cancel_tx,
        });
        let slot = self.slot.clone();
        let _guard = SlotGuard(&slot);
        let decider = self.decider.clone();
        let timer = ports.timer;
        let mut run = TurnRun::new(turn, ports, &*decider);

        let reused = match self.ensure_session(agent, &spec).await {
            Ok(reused) => reused,
            Err(e) => {
                run.fail_before_start(&e).await?;
                return Ok(run.turn.state);
            }
        };
        run.started(self.resume.clone(), assistant_name, context_project_id)
            .await?;
        let mut updates = match self.prompt(agent, spec, blocks, reused, &mut run).await? {
            Ok(updates) => updates,
            Err(e) => {
                let process_gone = matches!(e, AgentError::ProcessExited { .. });
                run.finish(End::Failed(e)).await?;
                if process_gone {
                    self.session = None;
                }
                return Ok(run.turn.state);
            }
        };

        let session = self.session.as_ref().expect("prompt 成功后一定有会话");
        let mut sched = Scheduler::default();
        let end = loop {
            let mut next = pin!(updates.next());
            let update = loop {
                if sched.flush_timer.is_none()
                    && let Some(delay) = run.checkpoint_delay()
                {
                    sched.flush_timer = Some(Box::pin(timer.sleep(delay)));
                }
                let input =
                    poll_fn(|cx| sched.poll(next.as_mut(), &mut signals, &mut cancels, cx)).await;
                match input {
                    Input::Update(update) => break update,
                    Input::Cancel => run.on_cancel(session).await?,
                    Input::FlushDue => run.checkpoint().await?,
                    Input::Signal(signal) => run.on_signal(signal).await?,
                }
            };
            let end = match update {
                Some(update) => run.on_update(update).await?,
                None => Some(End::Failed(AgentError::Protocol {
                    message: "运行时的更新流在回合结束前断了".into(),
                })),
            };
            if let Some(end) = end {
                // 结束前先处理已经收到的取消：用户要的是停下
                if cancels.try_recv().is_ok() {
                    run.on_cancel(session).await?;
                }
                break end;
            }
        };
        let process_gone = matches!(&end, End::Failed(AgentError::ProcessExited { .. }));
        run.finish(end).await?;
        if process_gone {
            // 丢弃即结束进程树；下一回合按记下的 id 续接
            self.session = None;
        }
        Ok(run.turn.state)
    }

    /// 确保有会话：已有就沿用（返回 `true`），没有就按记下的 ID 续接或新开（返回 `false`）。
    async fn ensure_session(&mut self, agent: &A, spec: &SessionSpec) -> Result<bool, AgentError> {
        if self.session.is_some() {
            return Ok(true);
        }
        self.open_session(agent, spec.clone()).await?;
        Ok(false)
    }

    async fn open_session(&mut self, agent: &A, mut spec: SessionSpec) -> Result<(), AgentError> {
        spec.resume = self.resume.clone();
        let gate = PermissionGate {
            slot: self.slot.clone(),
            decider: self.decider.clone(),
        };
        let session = agent.open_session(spec, gate).await?;
        self.resume = session.runtime_session_id().map(str::to_owned);
        self.session = Some(session);
        Ok(())
    }

    /// 发 prompt（回合已提交为 `Running`）。沿用的会话进程已经退出（崩溃、取消时被结束）时，
    /// prompt 没送到：续接一次，先把新会话的绑定落盘，再发。外层错误是编排自己的错误，
    /// 内层是运行时的错误（回合按失败收尾）。
    async fn prompt<S, C, I, L, T>(
        &mut self,
        agent: &A,
        spec: SessionSpec,
        blocks: Vec<ContentBlock>,
        reused: bool,
        run: &mut TurnRun<'_, S, C, I, L, T, D>,
    ) -> Result<Result<<A::Session as AgentSession>::Updates, AgentError>, TurnRunError>
    where
        S: StoragePort,
        C: Clock,
        I: IdGenerator,
        L: LivePublisher,
        T: Timer,
    {
        let session = self
            .session
            .as_ref()
            .expect("ensure_session 成功后一定有会话");
        let first = session.prompt(blocks.clone()).await;
        if !(reused && matches!(first, Err(AgentError::ProcessExited { .. }))) {
            return Ok(first);
        }
        self.session = None;
        if let Err(e) = self.open_session(agent, spec).await {
            return Ok(Err(e));
        }
        run.bind_session(self.resume.clone()).await?;
        let session = self.session.as_ref().expect("刚开好会话");
        Ok(session.prompt(blocks).await)
    }
}

enum Input {
    Update(Option<AgentUpdate>),
    Cancel,
    FlushDue,
    Signal(Signal),
}

/// 多路输入的调度状态，见模块文档的优先次序。
#[derive(Default)]
struct Scheduler<'t> {
    flush_timer: Option<Pin<Box<dyn Future<Output = ()> + Send + 't>>>,
    /// 连续处理的更新条数
    burst: u32,
    /// 这一轮连续更新之后是否已经让出过
    yielded: bool,
    /// 本回合已处理的更新条数，与审批的前序边界比较
    consumed: u64,
    /// 收到、还没到前序边界的「开始等审批」
    deferred_ask: Option<(Signal, u64)>,
}

impl Scheduler<'_> {
    fn poll<F: Future<Output = Option<AgentUpdate>>>(
        &mut self,
        mut next: Pin<&mut F>,
        signals: &mut mpsc::UnboundedReceiver<Signal>,
        cancels: &mut mpsc::UnboundedReceiver<()>,
        cx: &mut Context<'_>,
    ) -> Poll<Input> {
        // 两个信箱的发送端在回合信箱里，回合结束前不会全部关闭；关闭了也只是不再有输入
        if let Poll::Ready(Some(())) = Pin::new(cancels).poll_next(cx) {
            return Poll::Ready(Input::Cancel);
        }
        if let Some(timer) = self.flush_timer.as_mut()
            && timer.as_mut().poll(cx).is_ready()
        {
            self.flush_timer = None;
            return Poll::Ready(Input::FlushDue);
        }
        loop {
            if let Some((_, after)) = &self.deferred_ask {
                if self.consumed >= *after {
                    let (ask, _) = self.deferred_ask.take().expect("刚检查过");
                    return Poll::Ready(Input::Signal(ask));
                }
                // 还没到边界：只处理更新（其余信号留在信箱里排队），照样按批让出
                if self.burst >= FAIR_BURST {
                    if !self.yielded {
                        self.yielded = true;
                        cx.waker().wake_by_ref();
                        return Poll::Pending;
                    }
                    self.yielded = false;
                    self.burst = 0;
                }
                return match next.as_mut().poll(cx) {
                    Poll::Ready(update) => Poll::Ready(self.update(update)),
                    Poll::Pending => Poll::Pending,
                };
            }
            let signal_first = self.burst >= FAIR_BURST;
            if !signal_first && let Poll::Ready(update) = next.as_mut().poll(cx) {
                return Poll::Ready(self.update(update));
            }
            if signal_first && !self.yielded {
                // 更新一直就绪时主动让出一次，别的任务（含运行时的其他会话）才有机会跑
                self.yielded = true;
                cx.waker().wake_by_ref();
                return Poll::Pending;
            }
            self.yielded = false;
            self.burst = 0;
            match Pin::new(&mut *signals).poll_next(cx) {
                Poll::Ready(Some(Signal::PermissionAsked { reply, after })) => {
                    // 先把边界之前的更新处理完再受理，下一轮循环走上面的分支
                    self.deferred_ask = Some((Signal::PermissionAsked { reply, after }, after));
                    continue;
                }
                Poll::Ready(Some(signal)) => return Poll::Ready(Input::Signal(signal)),
                Poll::Ready(None) | Poll::Pending => {}
            }
            if signal_first && let Poll::Ready(update) = next.as_mut().poll(cx) {
                return Poll::Ready(self.update(update));
            }
            return Poll::Pending;
        }
    }

    fn update(&mut self, update: Option<AgentUpdate>) -> Input {
        self.burst += 1;
        if update.is_some() {
            self.consumed += 1;
        }
        Input::Update(update)
    }
}

/// 回合怎么结束。
enum End {
    Finished(TurnOutcome),
    Failed(AgentError),
}

/// 一个进行中的回合：状态、正文、用量都在这里，只有它写事件。
struct TurnRun<'a, S, C, I, L, T, D> {
    turn: Turn,
    ports: TurnPorts<'a, S, C, I, L, T>,
    decider: &'a D,
    message_id: Option<MessageId>,
    body: String,
    usage: Usage,
    /// 已受理、还没有采纳结果的审批票；空了才离开 `WaitingApproval`
    pending: BTreeSet<Ticket>,
    last_checkpoint: Option<MonotonicInstant>,
    checkpointed_len: usize,
}

impl<'a, S, C, I, L, T, D> TurnRun<'a, S, C, I, L, T, D>
where
    S: StoragePort,
    C: Clock,
    I: IdGenerator,
    L: LivePublisher,
    T: Timer,
    D: PermissionDecider,
{
    fn new(turn: Turn, ports: TurnPorts<'a, S, C, I, L, T>, decider: &'a D) -> Self {
        Self {
            turn,
            ports,
            decider,
            message_id: None,
            body: String::new(),
            usage: Usage::default(),
            pending: BTreeSet::new(),
            last_checkpoint: None,
            checkpointed_len: 0,
        }
    }

    async fn commit(&self, events: Vec<DomainEvent>) -> Result<(), StorageError> {
        let request = CommitRequest {
            command_id: CommandId::from_raw(self.ports.ids.next()),
            actor: Actor(ORCHESTRATOR_ACTOR.into()),
            events,
            outbox: Vec::new(),
        };
        self.ports.store.commit(request).await.map(|_| ())
    }

    /// 推进一步状态，返回要写入的事件：状态变化，以及这一步作废的审批（取消、失败时）。
    async fn step(
        &mut self,
        trigger: TurnTrigger,
        reason: Option<String>,
    ) -> Result<Vec<DomainEvent>, TurnIllegal> {
        let from = self.turn.state;
        let to = self.turn.apply(trigger, self.ports.clock.now())?;
        let mut events = vec![
            TurnStateChanged {
                turn_id: self.turn.id,
                from,
                to,
                reason,
            }
            .into(),
        ];
        if let Some(void) = trigger.void_reason() {
            events.extend(self.decider.void_pending(self.turn.id, void).await);
        }
        Ok(events)
    }

    async fn fail_before_start(&mut self, error: &AgentError) -> Result<(), TurnRunError> {
        let mut events = self
            .step(TurnTrigger::Failed, Some(error.to_string()))
            .await?;
        events.push(self.finished_event());
        self.commit(events).await?;
        Ok(())
    }

    /// 会话 ID 与回合上记着的不同时，返回要写的绑定事件，并记到回合上。
    fn session_binding(&mut self, runtime_session_id: Option<String>) -> Option<DomainEvent> {
        let id = runtime_session_id?;
        if self.turn.runtime_session_id.as_deref() == Some(id.as_str()) {
            return None;
        }
        self.turn.runtime_session_id = Some(id.clone());
        Some(
            TurnSessionBound {
                turn_id: self.turn.id,
                runtime_session_id: id,
            }
            .into(),
        )
    }

    /// 回合换了会话（续接时运行时给了新 ID）：发 prompt 之前单独落盘。
    async fn bind_session(
        &mut self,
        runtime_session_id: Option<String>,
    ) -> Result<(), TurnRunError> {
        if let Some(event) = self.session_binding(runtime_session_id) {
            self.commit(vec![event]).await?;
        }
        Ok(())
    }

    /// 发 prompt 之前的那次提交：会话绑定、进 `Running`、空的助手消息。
    async fn started(
        &mut self,
        runtime_session_id: Option<String>,
        assistant_name: String,
        context_project_id: Option<ProjectId>,
    ) -> Result<(), TurnRunError> {
        let message_id = MessageId::from_raw(self.ports.ids.next());
        self.message_id = Some(message_id);
        let mut events: Vec<DomainEvent> = self
            .session_binding(runtime_session_id)
            .into_iter()
            .collect();
        events.extend(self.step(TurnTrigger::Started, None).await?);
        let author = Author::Assistant {
            id: self.turn.assistant_id,
            name_snapshot: assistant_name,
        };
        let message = Message {
            id: message_id,
            conversation_id: self.turn.conversation_id,
            status: MessageStatus::initial_for(&author),
            author,
            context_project_id,
            body: MessageBody::Inline(String::new()),
            attachments: Vec::new(),
            created_at: self.ports.clock.now(),
            turn_id: Some(self.turn.id),
        };
        events.push(MessageAppended { message }.into());
        self.commit(events).await?;
        Ok(())
    }

    /// 处理一条更新；回合到此结束时返回结局。
    async fn on_update(&mut self, update: AgentUpdate) -> Result<Option<End>, TurnRunError> {
        let turn_id = self.turn.id;
        let event: DomainEvent = match update {
            AgentUpdate::Text { offset, text } => {
                self.on_text(offset, text).await?;
                return Ok(None);
            }
            AgentUpdate::ToolCallStarted {
                tool_call_id,
                tool,
                raw_input,
            } => ToolCallStarted {
                turn_id,
                tool_call_id,
                tool_name: tool.0.clone(),
                // 动作归一属于审批引擎（技术设计 7.1，T07）；在那之前只记工具名
                action: Action {
                    kind: ActionKind::Unclassified,
                    target: ActionTarget::ToolName(tool.0.clone()),
                    raw_summary: raw_input.0.chars().take(SUMMARY_CHARS).collect(),
                    tool,
                    flags: ActionFlags::default(),
                },
            }
            .into(),
            AgentUpdate::ToolCallFinished {
                tool_call_id,
                ok,
                output: _,
            } => ToolCallFinished {
                turn_id,
                tool_call_id,
                ok,
                // 大段输出进制品存储（技术设计 3.7）随附件与制品功能接入
                output_artifact: None,
            }
            .into(),
            AgentUpdate::ModelReported(model) => TurnModelReported { turn_id, model }.into(),
            AgentUpdate::Usage(usage) => {
                self.usage = usage;
                return Ok(None);
            }
            AgentUpdate::Quota(windows) => QuotaObserved {
                runtime_id: self.turn.runtime_id,
                windows,
                observed_at: self.ports.clock.now(),
            }
            .into(),
            AgentUpdate::Finished(outcome) => return Ok(Some(End::Finished(outcome))),
            AgentUpdate::Failed(error) => return Ok(Some(End::Failed(error))),
        };
        self.commit(vec![event]).await?;
        Ok(None)
    }

    async fn on_text(&mut self, offset: u64, text: String) -> Result<(), TurnRunError> {
        let message_id = self.message_id.expect("started 之后才有更新");
        self.body.push_str(&text);
        self.ports.live.message_delta(MessageDelta {
            message_id,
            offset,
            text,
        });
        if self.checkpoint_delay() == Some(Duration::ZERO) {
            self.checkpoint().await?;
        }
        Ok(())
    }

    /// 有未落盘的正文时，离下一次检查点还有多久（已到期为 0）；没有就是 `None`。
    fn checkpoint_delay(&self) -> Option<Duration> {
        if self.body.len() <= self.checkpointed_len {
            return None;
        }
        let Some(last) = self.last_checkpoint else {
            return Some(Duration::ZERO);
        };
        let elapsed = Duration::from_nanos(self.ports.clock.monotonic().0.saturating_sub(last.0));
        Some(CHECKPOINT_INTERVAL.saturating_sub(elapsed))
    }

    /// 把未落盘的正文写成检查点。
    async fn checkpoint(&mut self) -> Result<(), TurnRunError> {
        let Some(message_id) = self.message_id else {
            return Ok(());
        };
        if self.body.len() <= self.checkpointed_len {
            return Ok(());
        }
        self.ports
            .store
            .checkpoint_message(message_id, self.body.len() as u64, &self.body)
            .await?;
        self.last_checkpoint = Some(self.ports.clock.monotonic());
        self.checkpointed_len = self.body.len();
        Ok(())
    }

    async fn on_cancel<Sess: AgentSession>(&mut self, session: &Sess) -> Result<(), TurnRunError> {
        if matches!(
            self.turn.state,
            TurnState::Running | TurnState::WaitingApproval
        ) {
            let events = self.step(TurnTrigger::Cancel, None).await?;
            self.commit(events).await?;
            // 等着的审批不再有结果可采纳
            self.pending.clear();
            // 通知发不出去也没关系：适配器到时结束进程树，更新流照样以取消收尾
            let _ = session.cancel().await;
        }
        Ok(())
    }

    async fn on_signal(&mut self, signal: Signal) -> Result<(), TurnRunError> {
        match signal {
            Signal::PermissionAsked { reply, .. } => {
                let accept = match self.turn.state {
                    TurnState::Running => {
                        let events = self.step(TurnTrigger::PermissionAsked, None).await?;
                        self.commit(events).await?;
                        true
                    }
                    TurnState::WaitingApproval => true,
                    // 正在取消：不再受理新的权限请求，处理方按「回合已结束」拒绝
                    _ => false,
                };
                if accept {
                    let ticket = new_ticket();
                    self.pending.insert(ticket);
                    let _ = reply.send(Some(ticket));
                } else {
                    let _ = reply.send(None);
                }
            }
            Signal::PermissionDecided { ticket, ack } => {
                // 只采纳这一回合、还在等审批、确有这张票的结果；否则拒绝且不动计数
                let accepted =
                    self.turn.state == TurnState::WaitingApproval && self.pending.remove(&ticket);
                if accepted && self.pending.is_empty() {
                    let events = self.step(TurnTrigger::PermissionDecided, None).await?;
                    self.commit(events).await?;
                }
                let _ = ack.send(accepted);
            }
        }
        Ok(())
    }

    /// 按结局推进到最终状态，写入消息结局与回合结束事件。
    async fn finish(&mut self, end: End) -> Result<(), TurnRunError> {
        let mut events = Vec::new();
        let state = self.turn.state;
        match end {
            End::Finished(TurnOutcome::Completed) => {
                self.pending.clear();
                if state == TurnState::WaitingApproval {
                    events.extend(self.step(TurnTrigger::PermissionDecided, None).await?);
                }
                if self.turn.state == TurnState::Cancelling {
                    // 取消请求到达前运行时已经做完了：按取消收尾，用户要的是停下
                    events.extend(self.step(TurnTrigger::ProcessEnded, None).await?);
                } else {
                    events.extend(self.step(TurnTrigger::Completed, None).await?);
                }
            }
            End::Finished(TurnOutcome::Cancelled) => {
                if state != TurnState::Cancelling {
                    events.extend(self.step(TurnTrigger::Cancel, None).await?);
                }
                events.extend(self.step(TurnTrigger::ProcessEnded, None).await?);
            }
            End::Failed(error) => {
                let trigger = match state {
                    TurnState::Cancelling => TurnTrigger::ProcessEnded,
                    _ => TurnTrigger::Failed,
                };
                events.extend(self.step(trigger, Some(error.to_string())).await?);
            }
        }
        let message_id = self.message_id.expect("started 之后才会 finish");
        events.push(
            MessageCompleted {
                conversation_id: self.turn.conversation_id,
                message_id,
                body: MessageBody::Inline(std::mem::take(&mut self.body)),
                status: message_status(self.turn.state),
            }
            .into(),
        );
        if self.usage != Usage::default() {
            events.push(
                UsageReported {
                    runtime_id: self.turn.runtime_id,
                    turn_id: self.turn.id,
                    usage: self.usage,
                }
                .into(),
            );
        }
        if self.turn.state.is_terminal() {
            events.push(self.finished_event());
        }
        self.commit(events).await?;
        Ok(())
    }

    fn finished_event(&self) -> DomainEvent {
        TurnFinished {
            turn_id: self.turn.id,
            state: self.turn.state,
            usage: self.usage,
            // 「搞定」章要看改动摘要（技术设计 9.7），改动统计随版本控制接入
            celebrate: false,
            change_summary: None,
        }
        .into()
    }
}

/// 回合停在的状态 → 助手消息的结局。
fn message_status(state: TurnState) -> MessageStatus {
    match state {
        TurnState::Completed => MessageStatus::Completed,
        TurnState::Failed => MessageStatus::Failed,
        _ => MessageStatus::Interrupted,
    }
}
