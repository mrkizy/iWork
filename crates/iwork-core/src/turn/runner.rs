//! 回合编排的最小闭环（技术设计 6.4、3.5）：开会话或续接 → prompt → 把更新流转成领域事件写入存储、
//! 把正文增量推出去 → 按回合转换表推进状态 → 结束。
//!
//! 回合状态只在这里写：运行时的更新、权限请求的两步、取消请求都汇到 [`SessionLine::run_turn`] 的
//! 一个循环里按到达顺序处理（同时就绪时先处理更新：适配器保证权限请求之前的更新已在更新流里，
//! 这样「工具调用开始」一定排在「等待审批」之前）。
//!
//! 运行时进程意外退出（后台服务还活着）时回合怎么收尾：
//! - `Running` 或 `WaitingApproval`：回合 `Failed`，原因是进程退出（`AgentError::ProcessExited`
//!   可原样重试），助手消息 `Failed`；
//! - `Cancelling`：本来就要取消，算 `Cancelled`。
//!
//! `Interrupted(Crash)` 只留给后台服务重启后发现进程已不在的情况（崩溃恢复，不在这里）。
//! 进程退出后丢掉这条会话，下一回合用记下的运行时会话 id 续接。

use std::future::{Future, poll_fn};
use std::pin::{Pin, pin};
use std::sync::Arc;
use std::task::{Context, Poll};

use futures_channel::mpsc;
use futures_core::Stream;
use iwork_domain::action::{Action, ActionFlags, ActionKind, ActionTarget};
use iwork_domain::common::{Actor, Usage};
use iwork_domain::event::{
    DomainEvent, MessageAppended, MessageCompleted, QuotaObserved, ToolCallFinished,
    ToolCallStarted, TurnFinished, TurnModelReported, TurnStateChanged, UsageReported,
};
use iwork_domain::id::{CommandId, MessageId, ProjectId, TurnId};
use iwork_domain::message::{Author, Message, MessageBody, MessageStatus};
use iwork_domain::turn::{Turn, TurnIllegal, TurnState, TurnTrigger};
use iwork_ports::agent::{
    AgentError, AgentPort, AgentSession, AgentUpdate, ContentBlock, SessionSpec, TurnOutcome,
    UpdateStream,
};
use iwork_ports::clock::{Clock, IdGenerator, MonotonicInstant};
use iwork_ports::live::{LivePublisher, MessageDelta};
use iwork_ports::store::{CommitRequest, StorageError, StoragePort};

use super::permission::{PermissionDecider, PermissionGate, Signal};

/// 回合编排写入的事件以它为 actor：这些事件由后台服务按运行时的输出产生，不是用户的命令。
pub const ORCHESTRATOR_ACTOR: &str = "system";

/// 流式正文的检查点间隔（技术设计 3.5：崩溃后最多丢这么久的显示文字）。
pub const CHECKPOINT_INTERVAL_NANOS: u64 = 500_000_000;

/// 动作摘要最多保留的字符数。
const SUMMARY_CHARS: usize = 200;

/// 回合编排用到的端口。
#[derive(Debug, Clone, Copy)]
pub struct TurnPorts<'a, S, C, I, L> {
    pub store: &'a S,
    pub clock: &'a C,
    pub ids: &'a I,
    pub live: &'a L,
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
/// 而是回合以 `Failed` 等状态正常收尾。
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
    /// 最近一次的运行时会话 id；会话没了（进程退出、后台服务重启）时用它续接
    resume: Option<String>,
    mailbox: mpsc::UnboundedSender<Signal>,
    inbox: mpsc::UnboundedReceiver<Signal>,
    decider: Arc<D>,
}

/// 取消某个回合的句柄，可在任何任务里用。
#[derive(Debug, Clone)]
pub struct TurnCanceller {
    mailbox: mpsc::UnboundedSender<Signal>,
}

impl TurnCanceller {
    /// 请求取消。回合已经结束或换了就什么都不做。
    pub fn cancel(&self, turn_id: TurnId) {
        let _ = self.mailbox.unbounded_send(Signal::Cancel(turn_id));
    }
}

impl<A: AgentPort, D: PermissionDecider> SessionLine<A, D> {
    /// `resume`：已知的运行时会话 id（如后台服务重启前记下的），没有就新开。
    pub fn new(decider: Arc<D>, resume: Option<String>) -> Self {
        let (mailbox, inbox) = mpsc::unbounded();
        Self {
            session: None,
            resume,
            mailbox,
            inbox,
            decider,
        }
    }

    pub fn canceller(&self) -> TurnCanceller {
        TurnCanceller {
            mailbox: self.mailbox.clone(),
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

    /// 跑一个回合，返回它停在的状态。
    pub async fn run_turn<S, C, I, L>(
        &mut self,
        agent: &A,
        ports: TurnPorts<'_, S, C, I, L>,
        plan: TurnPlan,
    ) -> Result<TurnState, TurnRunError>
    where
        S: StoragePort,
        C: Clock,
        I: IdGenerator,
        L: LivePublisher,
    {
        self.drain_stale_signals();
        let TurnPlan {
            turn,
            assistant_name,
            context_project_id,
            spec,
            blocks,
        } = plan;
        let decider = self.decider.clone();
        let mut run = TurnRun::new(turn, ports, &*decider);

        let mut updates = match self.start(agent, spec, blocks).await {
            Ok(updates) => updates,
            Err(e) => {
                run.fail_before_start(&e).await?;
                return Ok(run.turn.state);
            }
        };
        run.started(assistant_name, context_project_id).await?;

        let Self { session, inbox, .. } = self;
        let session = session.as_ref().expect("start 成功后一定有会话");
        let end = loop {
            let mut next = pin!(updates.next());
            let update = loop {
                match poll_fn(|cx| poll_input(next.as_mut(), inbox, cx)).await {
                    Input::Update(update) => break update,
                    Input::Signal(signal) => run.on_signal(signal, session).await?,
                }
            };
            match update {
                Some(update) => {
                    if let Some(end) = run.on_update(update).await? {
                        break end;
                    }
                }
                None => {
                    break End::Failed(AgentError::Protocol {
                        message: "运行时的更新流在回合结束前断了".into(),
                    });
                }
            }
        };
        let process_gone = matches!(&end, End::Failed(AgentError::ProcessExited { .. }));
        run.finish(end).await?;
        if process_gone {
            // 丢弃即结束进程树；下一回合按记下的 id 续接
            self.session = None;
        }
        self.drain_stale_signals();
        Ok(run.turn.state)
    }

    /// 确保有会话并发出 prompt。已有会话的进程退出了就续接一次再发。
    async fn start(
        &mut self,
        agent: &A,
        mut spec: SessionSpec,
        blocks: Vec<ContentBlock>,
    ) -> Result<<A::Session as AgentSession>::Updates, AgentError> {
        if let Some(session) = &self.session {
            match session.prompt(blocks.clone()).await {
                Err(AgentError::ProcessExited { .. }) => self.session = None,
                other => return other,
            }
        }
        spec.resume = self.resume.clone();
        let gate = PermissionGate {
            mailbox: self.mailbox.clone(),
            decider: self.decider.clone(),
        };
        let session = agent.open_session(spec, gate).await?;
        self.resume = session.runtime_session_id().map(str::to_owned);
        let session = self.session.insert(session);
        session.prompt(blocks).await
    }

    /// 清掉不属于任何进行中回合的输入：等审批的答「没有回合」，取消请求丢弃。
    fn drain_stale_signals(&mut self) {
        while let Ok(signal) = self.inbox.try_recv() {
            match signal {
                Signal::PermissionAsked { reply } => {
                    let _ = reply.send(None);
                }
                // 丢掉回执，权限处理方按「回合已结束」拒绝
                Signal::PermissionDecided { .. } | Signal::Cancel(_) => {}
            }
        }
    }
}

enum Input {
    Update(Option<AgentUpdate>),
    Signal(Signal),
}

/// 同时等更新流和信箱，更新优先。信箱不会关闭：会话线自己持有一个发送端。
fn poll_input<F: Future<Output = Option<AgentUpdate>>>(
    next: Pin<&mut F>,
    inbox: &mut mpsc::UnboundedReceiver<Signal>,
    cx: &mut Context<'_>,
) -> Poll<Input> {
    if let Poll::Ready(update) = next.poll(cx) {
        return Poll::Ready(Input::Update(update));
    }
    match Pin::new(inbox).poll_next(cx) {
        Poll::Ready(Some(signal)) => Poll::Ready(Input::Signal(signal)),
        Poll::Ready(None) | Poll::Pending => Poll::Pending,
    }
}

/// 回合怎么结束。
enum End {
    Finished(TurnOutcome),
    Failed(AgentError),
}

/// 一个进行中的回合：状态、正文、用量都在这里，只有它写事件。
struct TurnRun<'a, S, C, I, L, D> {
    turn: Turn,
    ports: TurnPorts<'a, S, C, I, L>,
    decider: &'a D,
    message_id: Option<MessageId>,
    body: String,
    usage: Usage,
    /// 还没答复的权限请求数；回到 0 才离开 `WaitingApproval`
    pending_permissions: u32,
    last_checkpoint: Option<MonotonicInstant>,
    checkpointed_len: usize,
}

impl<'a, S, C, I, L, D> TurnRun<'a, S, C, I, L, D>
where
    S: StoragePort,
    C: Clock,
    I: IdGenerator,
    L: LivePublisher,
    D: PermissionDecider,
{
    fn new(turn: Turn, ports: TurnPorts<'a, S, C, I, L>, decider: &'a D) -> Self {
        Self {
            turn,
            ports,
            decider,
            message_id: None,
            body: String::new(),
            usage: Usage::default(),
            pending_permissions: 0,
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

    async fn started(
        &mut self,
        assistant_name: String,
        context_project_id: Option<ProjectId>,
    ) -> Result<(), TurnRunError> {
        let message_id = MessageId::from_raw(self.ports.ids.next());
        self.message_id = Some(message_id);
        let mut events = self.step(TurnTrigger::Started, None).await?;
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
        let now = self.ports.clock.monotonic();
        let due = self
            .last_checkpoint
            .is_none_or(|last| now.0.saturating_sub(last.0) >= CHECKPOINT_INTERVAL_NANOS);
        if due && self.body.len() > self.checkpointed_len {
            self.ports
                .store
                .checkpoint_message(message_id, self.body.len() as u64, &self.body)
                .await?;
            self.last_checkpoint = Some(now);
            self.checkpointed_len = self.body.len();
        }
        Ok(())
    }

    async fn on_signal<Sess: AgentSession>(
        &mut self,
        signal: Signal,
        session: &Sess,
    ) -> Result<(), TurnRunError> {
        match signal {
            Signal::PermissionAsked { reply } => match self.turn.state {
                TurnState::Running => {
                    let events = self.step(TurnTrigger::PermissionAsked, None).await?;
                    self.commit(events).await?;
                    self.pending_permissions += 1;
                    let _ = reply.send(Some(self.turn.id));
                }
                TurnState::WaitingApproval => {
                    self.pending_permissions += 1;
                    let _ = reply.send(Some(self.turn.id));
                }
                // 正在取消：不再受理新的权限请求，处理方按「回合已结束」拒绝
                _ => {
                    let _ = reply.send(None);
                }
            },
            Signal::PermissionDecided { ack } => {
                self.pending_permissions = self.pending_permissions.saturating_sub(1);
                if self.turn.state == TurnState::WaitingApproval && self.pending_permissions == 0 {
                    let events = self.step(TurnTrigger::PermissionDecided, None).await?;
                    self.commit(events).await?;
                }
                let _ = ack.send(());
            }
            Signal::Cancel(turn_id) => {
                let cancellable = matches!(
                    self.turn.state,
                    TurnState::Running | TurnState::WaitingApproval
                );
                if turn_id == self.turn.id && cancellable {
                    let events = self.step(TurnTrigger::Cancel, None).await?;
                    self.commit(events).await?;
                    // 通知发不出去也没关系：适配器 3 秒后结束进程树，更新流照样以取消收尾
                    let _ = session.cancel().await;
                }
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
