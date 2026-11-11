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
//! **续接**（T06 方案 2.6 第 4 条）：`TurnPlan.turn` 处于 `Interrupted` 时，第一步用 `Resume` 而不是
//! `Started`，另起一条助手消息（同一个回合）；说明文字由调用方用 [`super::resume_note`] 生成放进 `blocks`。
//! 入口完全由回合状态决定，不另设字段。
//!
//! 回合状态只在 [`SessionLine::run_turn`] 的循环里写。它同时等几路输入，优先次序：
//! 1. **取消**：最先处理，更新再多也不会被饿死；回合结束前还会再看一眼有没有已收到的取消；
//! 2. **检查点定时**：上次检查点之后最多 500ms 把未落盘的正文刷出去，即使没有新的更新；
//! 3. **等待上限**：有在等判定方的票、会话声明了上限（`permission_wait_limit`）时每秒查一次期限；
//! 4. **更新流**与**交接表**（受理请求、采纳结论）：一般先更新、后交接表；连续处理 [`FAIR_BURST`] 条更新后
//!    先让出一次执行权、再先看一次交接表。
//!    权限请求带着前序边界（适配器给出的「这次请求之前已交出的更新条数」）：编排先把本回合的更新处理到
//!    这个条数再受理，不管积压多少，「工具调用开始」都排在「等待审批」之前。
//!
//! **审批的提交、交付、等待上限由提交顺序裁决**（T06 方案 4.2）：采纳一条结论时先提交（经审批的带
//! `ApprovalDelivered`），再回执；等待上限到时先采纳表里已有的结论，还有超期的票就提交
//! `Interrupted{ApprovalWaitLimit}`，然后停止受理（交接表在锁里逐格回绝）、请运行时停下。用户取消同样是
//! 提交 → 停止受理 → 停运行时 → 提醒判定方。
//!
//! 运行时进程意外退出（后台服务还活着）时回合怎么收尾：
//! - `Running` 或 `WaitingApproval`：回合 `Failed`，原因是进程退出（`AgentError::ProcessExited`
//!   可原样重试），助手消息 `Failed`；
//! - `Cancelling`：本来就要取消，算 `Cancelled`。
//!
//! **丢掉一条会话只有一条路 [`SessionLine::retire`]**（T06 方案 4.2a）：确认整棵进程树清空才放掉会话。
//! 清不掉时会话留着、记下「旧树未确认清空」，下一次开会话之前先再清一次，仍不行就不开；因等待上限中断的回合
//! 清不掉时改记 `Failed`，不能续接。
//!
//! 编排自己出错（写不进存储、表外转换）时：先撤掉交接表（迟到的审批回调一律被拒），再丢掉会话（结束运行时，
//! 免得工具继续改文件），然后把原始错误交给上层；不重发 prompt。

use std::future::{Future, poll_fn};
use std::pin::pin;
use std::sync::Arc;
use std::task::Poll;
use std::time::Duration;

use iwork_domain::action::{Action, ActionFlags, ActionKind, ActionTarget};
use iwork_domain::approval::VoidReason;
use iwork_domain::common::{Actor, Usage};
use iwork_domain::event::{
    ApprovalDelivered, DomainEvent, MessageAppended, MessageCompleted, QuotaObserved,
    ToolCallFinished, ToolCallStarted, TurnFinished, TurnModelReported, TurnSessionBound,
    TurnStateChanged, UsageReported,
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

use super::binding::{Rebind, mismatch_reason, rebind};
use super::permission::{
    AcceptedAt, PermissionDecider, PermissionGate, Ticket, TurnSlot, VoidPlanError,
};
use super::scheduler::{Input, Scheduler};

/// 回合编排写入的事件以它为 actor：这些事件由后台服务按运行时的输出产生，不是用户的命令。
pub const ORCHESTRATOR_ACTOR: &str = "system";

/// 流式正文的检查点间隔（技术设计 3.5：崩溃后最多丢这么久的显示文字）。
pub const CHECKPOINT_INTERVAL: Duration = Duration::from_millis(500);

/// 连续处理这么多条更新后，先让出一次执行权、再先看一次交接表。
pub const FAIR_BURST: u32 = 32;

/// 有在等判定方的票时，隔这么久查一次等待上限（T06 方案 2.6 第 4 条）。
pub const LIMIT_CHECK_INTERVAL: Duration = Duration::from_secs(1);

/// 一笔带作废的提交从头到尾最多等这么久：要作废计划（含等令牌）、带着句柄等提交、被拒后的重算，合起来
/// 共用这一个期限（T07 方案 6.10.8）。到期就按编排出错收场：撤交接表、丢掉会话，运行时先停下。
pub const STOP_LIMIT: Duration = Duration::from_secs(5);

/// 提交之后提醒判定方，最多等这么久；到期就算了，不是错误（判定方下一个决定自己会追平）。
pub const NUDGE_LIMIT: Duration = Duration::from_secs(1);

/// 因等待上限中断的回合，旧进程树没能确认清空时，改记 `Failed` 的原因。
pub const TREE_NOT_CLEARED: &str = "运行时进程树没能结束";

/// 带作废的提交被投影器拒（计划过期）后重新要计划，最多这么多次。
const VOID_COMMIT_ATTEMPTS: u32 = 3;

/// 一笔提交里要作废的：原因，以及作废事件插在第几条之后（紧跟触发它的那条状态变化）。
#[derive(Debug, Clone, Copy)]
struct Voiding {
    reason: VoidReason,
    after: usize,
}

impl Voiding {
    /// `trigger` 是这笔提交里第 `index` 条事件（状态变化）的触发。
    fn of(trigger: TurnTrigger, index: usize) -> Option<Self> {
        trigger.void_reason().map(|reason| Self {
            reason,
            after: index + 1,
        })
    }
}

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

/// 一个回合要做什么。`turn` 处于 `Queued`（新回合，`TurnQueued` 已由受理命令的一方写入）或
/// `Interrupted`（续接）。
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
/// 而是回合以 `Failed` 等状态正常收尾。出这种错时会话已被丢掉。
#[derive(Debug, thiserror::Error)]
pub enum TurnRunError {
    #[error(transparent)]
    Storage(#[from] StorageError),
    #[error(transparent)]
    Illegal(#[from] TurnIllegal),
    /// 判定方算不出作废计划
    #[error(transparent)]
    VoidPlan(#[from] VoidPlanError),
    /// 带作废的提交没能在 `STOP_LIMIT` 里记完账；`step` 是 `"void_plan"` 或 `"void_commit"`
    #[error("回合停止的记账超时（{step}）")]
    StopTimedOut { step: &'static str },
}

/// `fut` 和 `limit` 后到期的定时器赛跑；到期返回 `None` 并丢掉 `fut`。
async fn within<T: Timer, F: Future>(timer: &T, limit: Duration, fut: F) -> Option<F::Output> {
    let mut fut = pin!(fut);
    let mut sleep = pin!(timer.sleep(limit));
    poll_fn(|cx| {
        if let Poll::Ready(out) = fut.as_mut().poll(cx) {
            return Poll::Ready(Some(out));
        }
        if sleep.as_mut().poll(cx).is_ready() {
            return Poll::Ready(None);
        }
        Poll::Pending
    })
    .await
}

/// 一条运行时会话线（技术设计 6.4）：一个对话（或「助手 × 项目」）对应一条，跨回合续接。
/// 同一条线上的回合一个接一个：`run_turn` 拿 `&mut self`，续接只能排在上一次 `run_turn` 返回之后。
pub struct SessionLine<A: AgentPort, D> {
    session: Option<A::Session>,
    /// 最近一次的运行时会话 id；会话没了（进程退出、取消后、后台服务重启）时用它续接
    resume: Option<String>,
    /// 打开当前（或上一条）会话时用的 spec：会话绑定项守卫拿它比（`binding.rs`）
    bound: Option<SessionSpec>,
    /// 丢掉会话时进程树没能确认清空：原因。有它时 `session` 还留着，开新会话前先再清一次
    unclean: Option<String>,
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
        self.slot.cancel(turn_id);
    }
}

/// 交接表的生命周期：回合开始时打开，函数以任何方式返回时停止受理并撤掉。
struct SlotGuard<'a>(&'a TurnSlot);

impl Drop for SlotGuard<'_> {
    fn drop(&mut self) {
        self.0.close();
    }
}

/// 开会话之前就不能开始的原因（回合以 `Failed` 收尾，原因写进状态变化）。
type StartRefused = String;

impl<A: AgentPort, D: PermissionDecider> SessionLine<A, D> {
    /// `resume`：已知的运行时会话 id（如后台服务重启前记下的），没有就新开。
    pub fn new(decider: Arc<D>, resume: Option<String>) -> Self {
        Self {
            session: None,
            resume,
            bound: None,
            unclean: None,
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

    /// 跑一个回合，返回它停在的状态。返回 `Err` 时交接表已撤，会话已丢掉（清不掉的记着，见模块文档）。
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
        if result.is_err() {
            // 交接表已随 run_turn_inner 返回撤掉；结束运行时，免得它在无人记录时继续动手。
            // 清不掉的记在 unclean 里，下一次开会话前再清；这里交回的是原始错误
            let _ = self.retire().await;
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
        let first = match turn.state {
            TurnState::Interrupted { .. } => TurnTrigger::Resume,
            // Queued 之外的状态在第一步里报表外转换
            _ => TurnTrigger::Started,
        };
        let mut cancels = self.slot.open(turn.id);
        let slot = self.slot.clone();
        let _guard = SlotGuard(&slot);
        let decider = self.decider.clone();
        let timer = ports.timer;
        let mut run = TurnRun::new(turn, ports, &*decider, &slot);

        let reused = match self.ensure_session(agent, &spec).await {
            Ok(reused) => reused,
            Err(reason) => {
                run.fail_before_start(reason).await?;
                return Ok(run.turn.state);
            }
        };
        run.started(
            first,
            self.resume.clone(),
            assistant_name,
            context_project_id,
        )
        .await?;
        let mut updates = match self.prompt(agent, spec, blocks, reused, &mut run).await? {
            Ok(updates) => updates,
            Err(e) => {
                let process_gone = matches!(e, AgentError::ProcessExited { .. });
                run.finish(End::Failed(e)).await?;
                if process_gone {
                    let _ = self.retire().await;
                }
                return Ok(run.turn.state);
            }
        };

        let session = self.session.as_ref().expect("prompt 成功后一定有会话");
        let limit = session.permission_wait_limit();
        let mut sched = Scheduler::default();
        let end = loop {
            let mut next = pin!(updates.next());
            let update = loop {
                if sched.flush_timer.is_none()
                    && let Some(delay) = run.checkpoint_delay()
                {
                    sched.flush_timer = Some(Box::pin(timer.sleep(delay)));
                }
                let input = poll_fn(|cx| sched.poll(next.as_mut(), &slot, &mut cancels, cx)).await;
                match input {
                    Input::Update(update) => break update,
                    Input::Cancel => run.on_cancel(session).await?,
                    Input::FlushDue => run.checkpoint().await?,
                    Input::Admit(ticket) => run.on_admit(ticket).await?,
                    Input::Adopt(ticket) => run.on_adopt(ticket).await?,
                    Input::LimitCheck => {
                        if let Some(limit) = limit {
                            run.on_limit_check(session, limit).await?;
                        }
                    }
                }
                // 有在等判定方的票就按时查期限；票是编排受理的，受理之后、查过之后都走到这里
                if limit.is_some()
                    && sched.limit_timer.is_none()
                    && run.turn.state == TurnState::WaitingApproval
                    && slot.inspect(|h| h.has_deciding()).unwrap_or(false)
                {
                    sched.limit_timer = Some(Box::pin(timer.sleep(LIMIT_CHECK_INTERVAL)));
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
        let interrupted = matches!(run.turn.state, TurnState::Interrupted { .. });
        // 进程没了、因等待上限中断：旧树不会再用，确认清空再放掉。取消的回合会话留着（适配器取消时已确认
        // 清树，清不掉会报进程退出、走上面那条），下一回合 prompt 发现进程已退出时经 retire 换一棵
        if (process_gone || interrupted) && self.retire().await.is_err() && interrupted {
            // 清理失败留在事件里：这个回合不能续接
            run.fail_after_interrupt().await?;
        }
        Ok(run.turn.state)
    }

    /// 丢掉当前会话：确认整棵进程树清空才放掉。清不掉时会话留着、记下原因，返回错误。
    async fn retire(&mut self) -> Result<(), AgentError> {
        let Some(session) = self.session.as_ref() else {
            self.unclean = None;
            return Ok(());
        };
        if let Err(error) = session.terminate().await {
            self.unclean = Some(error.to_string());
            return Err(error);
        }
        self.unclean = None;
        if let Some(session) = self.session.take() {
            let _ = session.close().await;
        }
        Ok(())
    }

    /// 确保有会话：已有且绑定项相同就沿用（返回 `true`），否则按记下的 ID 续接或新开（返回 `false`）。
    async fn ensure_session(
        &mut self,
        agent: &A,
        spec: &SessionSpec,
    ) -> Result<bool, StartRefused> {
        if let Some(reason) = self.unclean.clone() {
            self.retire().await.map_err(|e| {
                format!("上一棵运行时进程树没能确认清空（{reason}），不开新会话：{e}")
            })?;
        }
        if let Some(bound) = &self.bound {
            match rebind(bound, spec) {
                Rebind::Same => {}
                Rebind::Mismatch(field) => return Err(mismatch_reason(field)),
                // 模型、系统提示改了：下一轮生效，关掉会话带 resume 重开
                Rebind::Reopen => self.retire().await.map_err(|e| e.to_string())?,
            }
        }
        if self.session.is_some() {
            return Ok(true);
        }
        self.open_session(agent, spec.clone())
            .await
            .map_err(|e| e.to_string())?;
        Ok(false)
    }

    async fn open_session(&mut self, agent: &A, mut spec: SessionSpec) -> Result<(), AgentError> {
        spec.resume = self.resume.clone();
        let gate = PermissionGate {
            slot: self.slot.clone(),
            decider: self.decider.clone(),
        };
        let bound = spec.clone();
        let session = agent.open_session(spec, gate).await?;
        self.resume = session.runtime_session_id().map(str::to_owned);
        self.session = Some(session);
        self.bound = Some(bound);
        Ok(())
    }

    /// 发 prompt（回合已提交为 `Running`）。沿用的会话进程已经退出（崩溃、取消时被结束）时，
    /// prompt 没送到：丢掉旧会话、续接一次，先把新会话的绑定落盘，再发。外层错误是编排自己的错误，
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
        if let Err(e) = self.retire().await {
            return Ok(Err(e));
        }
        if let Err(e) = self.open_session(agent, spec).await {
            return Ok(Err(e));
        }
        run.bind_session(self.resume.clone()).await?;
        let session = self.session.as_ref().expect("刚开好会话");
        Ok(session.prompt(blocks).await)
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
    /// 这个回合的交接表：在途的权限请求只记在这里
    slot: &'a TurnSlot,
    message_id: Option<MessageId>,
    body: String,
    usage: Usage,
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
    fn new(
        turn: Turn,
        ports: TurnPorts<'a, S, C, I, L, T>,
        decider: &'a D,
        slot: &'a TurnSlot,
    ) -> Self {
        Self {
            turn,
            ports,
            decider,
            slot,
            message_id: None,
            body: String::new(),
            usage: Usage::default(),
            last_checkpoint: None,
            checkpointed_len: 0,
        }
    }

    fn request(&self, events: Vec<DomainEvent>) -> CommitRequest {
        CommitRequest {
            command_id: CommandId::from_raw(self.ports.ids.next()),
            actor: Actor(ORCHESTRATOR_ACTOR.into()),
            events,
            outbox: Vec::new(),
        }
    }

    /// 提交，返回已落盘的全部事件。`void` 有值时向判定方要作废计划，和这些事件同一笔提交：要计划、
    /// 带着句柄等提交、被拒后的重算共用一个 `STOP_LIMIT`。**不在这里提醒判定方**：由调用方在该做的事做完
    /// 之后调 [`Self::nudge`]（停止路径上排在 `session.cancel()` 之后）。
    async fn commit(
        &self,
        events: Vec<DomainEvent>,
        void: Option<Voiding>,
    ) -> Result<Vec<DomainEvent>, TurnRunError> {
        let Some(Voiding { reason, after }) = void else {
            self.ports
                .store
                .commit(self.request(events.clone()))
                .await?;
            return Ok(events);
        };
        let deadline = self.ports.clock.monotonic().0 + STOP_LIMIT.as_nanos() as u64;
        let mut attempt = 0;
        loop {
            attempt += 1;
            let (voids, hold) = self
                .until(deadline, self.decider.void_pending(self.turn.id, reason))
                .await
                .ok_or(TurnRunError::StopTimedOut { step: "void_plan" })??;
            let mut all = events.clone();
            all.splice(after..after, voids);
            let result = self
                .until(deadline, self.ports.store.commit(self.request(all.clone())))
                .await;
            // 到期也在这里丢：令牌放了，那笔提交去向不明，下一位过屏障
            drop(hold);
            match result.ok_or(TurnRunError::StopTimedOut {
                step: "void_commit",
            })? {
                Ok(_) => return Ok(all),
                // 计划过期（有审批或卡片已被别的提交改了）：回合自己的事件不重算，重新要作废那一段
                Err(StorageError::Projection { .. }) if attempt < VOID_COMMIT_ATTEMPTS => {}
                Err(error) => return Err(error.into()),
            }
        }
    }

    /// 在单调时钟的 `deadline` 之前等 `fut`；已经过了就立刻到期。
    async fn until<F: Future>(&self, deadline: u64, fut: F) -> Option<F::Output> {
        let left = deadline.saturating_sub(self.ports.clock.monotonic().0);
        within(self.ports.timer, Duration::from_nanos(left), fut).await
    }

    /// 提醒判定方「有新事件」。带期限，到期丢掉、记一条 debug 日志，从不返回错误。
    async fn nudge(&self, events: &[DomainEvent]) {
        if within(
            self.ports.timer,
            NUDGE_LIMIT,
            self.decider.committed(events),
        )
        .await
        .is_none()
        {
            tracing::debug!(turn = %self.turn.id, "提醒判定方超时，丢掉这一次");
        }
    }

    /// 提交然后提醒：不带作废、也不在停止路径上的提交都走这里。
    async fn commit_and_nudge(
        &self,
        events: Vec<DomainEvent>,
        void: Option<Voiding>,
    ) -> Result<(), TurnRunError> {
        let committed = self.commit(events, void).await?;
        self.nudge(&committed).await;
        Ok(())
    }

    /// 推进一步状态，返回状态变化事件。这一步要作废的审批由调用方带着 `Voiding::of(trigger, 位置)` 交给 `commit`。
    fn step(
        &mut self,
        trigger: TurnTrigger,
        reason: Option<String>,
    ) -> Result<DomainEvent, TurnIllegal> {
        let from = self.turn.state;
        let to = self.turn.apply(trigger, self.ports.clock.now())?;
        Ok(TurnStateChanged {
            turn_id: self.turn.id,
            from,
            to,
            reason,
        }
        .into())
    }

    /// 会话开不起来（运行时错误、旧树清不掉、绑定项不一致）：回合从 `Queued` 或 `Interrupted` 直接失败。
    async fn fail_before_start(&mut self, reason: String) -> Result<(), TurnRunError> {
        let events = vec![
            self.step(TurnTrigger::Failed, Some(reason))?,
            self.finished_event(),
        ];
        self.commit_and_nudge(events, Voiding::of(TurnTrigger::Failed, 0))
            .await
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
            self.commit_and_nudge(vec![event], None).await?;
        }
        Ok(())
    }

    /// 发 prompt 之前的那次提交：会话绑定、进 `Running`、空的助手消息。
    /// `first`：新回合是 `Started`，续接是 `Resume`。续接也另起一条助手消息（同一个回合）。
    async fn started(
        &mut self,
        first: TurnTrigger,
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
        events.push(self.step(first, None)?);
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
        self.commit_and_nudge(events, None).await
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
            AgentUpdate::Quota(readings) => {
                let observed_at = self.ports.clock.now();
                QuotaObserved {
                    runtime_id: self.turn.runtime_id,
                    windows: readings
                        .into_iter()
                        .map(|r| r.observed_at(observed_at))
                        .collect(),
                    observed_at,
                }
                .into()
            }
            AgentUpdate::Finished(outcome) => return Ok(Some(End::Finished(outcome))),
            AgentUpdate::Failed(error) => return Ok(Some(End::Failed(error))),
        };
        self.commit_and_nudge(vec![event], None).await?;
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

    /// 受理那一刻的两种时钟读数。
    fn now(&self) -> AcceptedAt {
        AcceptedAt {
            mono: self.ports.clock.monotonic(),
            wall: self.ports.clock.now(),
        }
    }

    /// 用户取消。次序：提交（带作废）→ 停止受理 → 停运行时 → 提醒。提交一返回就停止受理：交接表在锁里逐格
    /// 回绝，名下每个权限请求都已有结论，`cancel()` 等它们写出答复时不会等到编排身上。
    async fn on_cancel<Sess: AgentSession>(&mut self, session: &Sess) -> Result<(), TurnRunError> {
        if matches!(
            self.turn.state,
            TurnState::Running | TurnState::WaitingApproval
        ) {
            let events = vec![self.step(TurnTrigger::Cancel, None)?];
            let committed = self
                .commit(events, Voiding::of(TurnTrigger::Cancel, 0))
                .await?;
            self.slot.stop_accepting();
            // 通知发不出去也没关系：适配器到时结束进程树，更新流照样以取消收尾
            let _ = session.cancel().await;
            self.nudge(&committed).await;
        }
        Ok(())
    }

    /// 受理一张票：回合还在跑就先提交「等待审批」，再把票交回处理方，处理方这才去问判定方。
    async fn on_admit(&mut self, ticket: Ticket) -> Result<(), TurnRunError> {
        match self.turn.state {
            TurnState::Running => {
                let events = vec![self.step(TurnTrigger::PermissionAsked, None)?];
                self.commit_and_nudge(events, None).await?;
            }
            TurnState::WaitingApproval => {}
            // 停止受理之后表里没有「等受理」的格子；到这里说明状态与表对不上，按不受理处理
            _ => {
                self.slot.refuse(ticket);
                return Ok(());
            }
        }
        self.slot.admit(ticket, self.now());
        Ok(())
    }

    /// 采纳一张票的结论：状态是 `WaitingApproval` 且票在「有结论」才采纳。先提交（经审批的带
    /// `ApprovalDelivered`；没有别的票在等时和回到 `Running` 同一笔），再回执 `true`。回执就是交接点。
    async fn on_adopt(&mut self, ticket: Ticket) -> Result<(), TurnRunError> {
        let found = self
            .slot
            .inspect(|h| {
                h.decided(ticket)
                    .map(|approval| (approval, h.others_await_approval(ticket)))
            })
            .flatten();
        let Some((approval, others)) =
            found.filter(|_| self.turn.state == TurnState::WaitingApproval)
        else {
            self.slot.refuse(ticket);
            return Ok(());
        };
        // 结果出自一条审批：交付事件是「这条批准已经用掉」的唯一依据（T06 方案 2.6 第 4 条）
        let mut events = Vec::new();
        if let Some(approval_id) = approval {
            events.push(
                ApprovalDelivered {
                    approval_id,
                    turn_id: self.turn.id,
                }
                .into(),
            );
        }
        if !others {
            events.push(self.step(TurnTrigger::PermissionDecided, None)?);
        }
        if !events.is_empty() {
            self.commit_and_nudge(events, None).await?;
        }
        let adopted = self.slot.adopt(ticket);
        debug_assert!(adopted.is_some(), "采纳之前格子只有编排能拿走");
        Ok(())
    }

    /// 查等待上限（T06 方案 4.2 第三行）。有票超期时：先把表里已到的结论采纳完（用户已经做了决定的，算数）；
    /// 还有超期的票 → 提交 `Interrupted{ApprovalWaitLimit}` → 停止受理 → 停运行时 → 提醒。审批不作废。
    async fn on_limit_check<Sess: AgentSession>(
        &mut self,
        session: &Sess,
        limit: Duration,
    ) -> Result<(), TurnRunError> {
        let overdue = |run: &Self| {
            let now = run.now();
            run.slot
                .inspect(|h| h.deciding_overdue(limit, now))
                .unwrap_or(false)
        };
        if self.turn.state != TurnState::WaitingApproval || !overdue(self) {
            return Ok(());
        }
        while let Some(ticket) = self.slot.inspect(|h| h.any_decided()).flatten() {
            self.on_adopt(ticket).await?;
        }
        if self.turn.state != TurnState::WaitingApproval || !overdue(self) {
            return Ok(());
        }
        let events = vec![self.step(TurnTrigger::WaitLimitReached, None)?];
        let committed = self.commit(events, None).await?;
        self.slot.stop_accepting();
        let _ = session.cancel().await;
        self.nudge(&committed).await;
        Ok(())
    }

    /// 因等待上限中断的回合，旧进程树没能确认清空：改记 `Failed`（带作废），回合不能续接。
    async fn fail_after_interrupt(&mut self) -> Result<(), TurnRunError> {
        let events = vec![
            self.step(TurnTrigger::Failed, Some(TREE_NOT_CLEARED.into()))?,
            self.finished_event(),
        ];
        self.commit_and_nudge(events, Voiding::of(TurnTrigger::Failed, 0))
            .await
    }

    /// 按结局推进到最终状态，写入消息结局与回合结束事件。
    async fn finish(&mut self, end: End) -> Result<(), TurnRunError> {
        // 更新流已经结束：还在等的请求不会再有人采纳
        self.slot.stop_accepting();
        let mut triggers = Vec::new();
        let state = self.turn.state;
        match end {
            // 已因等待上限中断：业务状态不再随更新流变，只把助手消息收尾。失败原因不进事件，记一条日志
            // （进程退出由 run_turn_inner 去清树，那里会再见到它）
            end if matches!(state, TurnState::Interrupted { .. }) => {
                if let End::Failed(error) = end {
                    tracing::warn!(turn = %self.turn.id, %error, "回合已因等待上限中断，运行时随后报了失败");
                }
            }
            End::Finished(TurnOutcome::Completed) => {
                if state == TurnState::WaitingApproval {
                    triggers.push((TurnTrigger::PermissionDecided, None));
                }
                // 取消请求到达前运行时已经做完了：按取消收尾，用户要的是停下
                let last = if state == TurnState::Cancelling {
                    TurnTrigger::ProcessEnded
                } else {
                    TurnTrigger::Completed
                };
                triggers.push((last, None));
            }
            End::Finished(TurnOutcome::Cancelled) => {
                if state != TurnState::Cancelling {
                    triggers.push((TurnTrigger::Cancel, None));
                }
                triggers.push((TurnTrigger::ProcessEnded, None));
            }
            End::Failed(error) => {
                let trigger = match state {
                    TurnState::Cancelling => TurnTrigger::ProcessEnded,
                    _ => TurnTrigger::Failed,
                };
                triggers.push((trigger, Some(error.to_string())));
            }
        }
        let mut events = Vec::new();
        let mut void = None;
        for (trigger, reason) in triggers {
            events.push(self.step(trigger, reason)?);
            if let Some(v) = Voiding::of(trigger, events.len() - 1) {
                debug_assert!(void.is_none(), "一笔提交里最多一个会作废的触发");
                void = Some(v);
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
        self.commit_and_nudge(events, void).await
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
