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
//! **续接失败**（T06 方案 5.2a；需求第 12 章 v1.78）：带着旧会话 id 开会话、运行时回 `SessionLost`（记录不在、
//! 坏了、版本不认）时，放下旧 id、用同一份 spec 新开一次，只试这一次；开成了就用 iWork 存的对话拼摘要，接在这一轮
//! 第一条输入的前面（续接的回合再接上调用方给的请求原文），`TurnSessionBound.replaced` 记下旧编号与原因。新开也
//! 失败、或续接回合的请求取不全，才判回合失败。开会话的两处（回合开头、沿用的进程已退出后重开）都经
//! [`SessionLine::open_session`] 这一个函数。新编号在首条 prompt 被收下之前不算数：中间任何一步失败（读摘要、
//! 提交绑定、发 prompt），或者 `run_turn` 的 future 半路被丢掉、panic，新会话照 `retire` 的规矩丢掉、会话线退回
//! 旧编号；已经给回合落盘了新编号的，补一条 `TurnSessionBound` 记回旧编号（重启后从回合上取到的也是旧编号）。
//! 下一回合重新走一遍恢复，不会不带上文接着用新会话。这件事在 `run_turn` 的入口和收尾各查一次（
//! [`SessionLine::settle`]）：收尾管正常返回，入口管 future 被丢掉、panic 被接住之后接着用这条线。调用方丢掉
//! 一条会话线之前也先调它（T10a 方案 D8a），[`SessionLine::is_settled`] 说这条线还有没有没了结的东西。
//!
//! **交出 prompt 之前认取消**（T10a 方案 D1a）：取消可以在交接表建好之前就到（记在 `TurnSlot` 上，建表时带上）。
//! 建完交接表、开完会话、读完摘要之后各看一眼，只为少做无用功；保证靠的是两处 `session.prompt()` 之前的裁决点
//! （锁里看取消标记）。看到取消时照这一趟已经提交到哪收尾：新回合还没提交第一笔 → `Queued` → `Cancelled`；
//! 第一笔已提交 → 照取消的回合收尾（`Cancelling` → `Cancelled`）；续接的回合还没提交第一笔 → 什么都不提交，
//! 原样停在 `Interrupted`。
//!
//! **改动摘要**（T10a 方案 D10）：回合以 `Completed` 收尾时，提交之前限时问一次 `TurnPlan.changes`
//! （[`CHANGE_SUMMARY_LIMIT`]）；给不出来（出错、panic、到时）就不带摘要、不盖章，回合照常收尾。
//!
//! **续接**（T06 方案 2.6 第 4 条）：`TurnPlan.entry` 是 [`TurnEntry::Resume`] 时，第一步用 `Resume` 而不是
//! `Started`，另起一条助手消息（同一个回合）；说明文字由调用方用 [`super::resume_note`] 生成放进 `blocks`。
//! 入口和回合状态对不上（如给 `Queued` 的回合填了 `Resume`）时，第一步报表外转换。
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
use std::panic::AssertUnwindSafe;
use std::pin::pin;
use std::sync::Arc;
use std::task::Poll;
use std::time::Duration;

use iwork_domain::action::{Action, ActionFlags, ActionKind, ActionTarget};
use iwork_domain::approval::VoidReason;
use iwork_domain::common::{Actor, Usage};
use iwork_domain::event::{
    ApprovalDelivered, ChangeSummary, DomainEvent, MessageAppended, MessageCompleted,
    QuotaObserved, SessionReplaced, ToolCallFinished, ToolCallStarted, TurnFinished,
    TurnModelReported, TurnSessionBound, TurnStateChanged, UsageReported,
};
use iwork_domain::id::{CommandId, MessageId, ProjectId, TurnId};
use iwork_domain::message::{Author, Message, MessageBody, MessageStatus};
use iwork_domain::session::SessionLostReason;
use iwork_domain::turn::{Turn, TurnIllegal, TurnState, TurnTrigger};
use iwork_ports::agent::{
    AgentError, AgentPort, AgentSession, AgentUpdate, ContentBlock, SessionSpec, TurnOutcome,
    UpdateStream,
};
use iwork_ports::clock::{Clock, IdGenerator, MonotonicInstant, Timer};
use iwork_ports::live::{LivePublisher, MessageDelta};
use iwork_ports::read_model::ReadModelPort;
use iwork_ports::store::{CommitRequest, StorageError, StoragePort};
use iwork_ports::vcs::ChangeProbe;

use super::binding::{Rebind, mismatch_reason, rebind};
use super::permission::{
    AcceptedAt, PermissionDecider, PermissionGate, Ticket, TurnSlot, VoidPlanError,
};
use super::resume_summary::{SummaryLimits, resume_summary};
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

/// 回合以 `Completed` 收尾时等改动探针最多这么久；到时不带摘要照常收尾（T10a 方案 D10）。
pub const CHANGE_SUMMARY_LIMIT: Duration = Duration::from_secs(2);

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

/// 拼续接摘要时最多取这么多条此前的消息（T06 方案 5.2a）；再多也装不进摘要的字数上限。
pub const SUMMARY_RECENT_LIMIT: u32 = 100;

/// 回合编排用到的端口。
#[derive(Debug)]
pub struct TurnPorts<'a, S, C, I, L, T, H> {
    pub store: &'a S,
    pub clock: &'a C,
    pub ids: &'a I,
    pub live: &'a L,
    pub timer: &'a T,
    /// 读这条会话线此前的对话：只在续接失败、用摘要新开会话时读一次（T06 方案 5.2a）
    pub history: &'a H,
}

// 全是引用，可以复制；手写是为了不给端口类型加 Clone 约束（derive 会加）
impl<S, C, I, L, T, H> Clone for TurnPorts<'_, S, C, I, L, T, H> {
    fn clone(&self) -> Self {
        *self
    }
}
impl<S, C, I, L, T, H> Copy for TurnPorts<'_, S, C, I, L, T, H> {}

/// 一个回合要做什么。`turn` 处于 `Queued`（新回合，`TurnQueued` 已由受理命令的一方写入）或
/// `Interrupted`（续接），和 `entry` 对应。
#[derive(Clone)]
pub struct TurnPlan {
    pub turn: Turn,
    pub entry: TurnEntry,
    /// 助手当时的名字，写进助手消息的作者快照
    pub assistant_name: String,
    pub context_project_id: Option<ProjectId>,
    /// 开会话用；`resume` 由会话线自己填（它记着上一条运行时会话 id）
    pub spec: SessionSpec,
    /// 新回合是用户的请求；续接的回合是续接说明
    pub blocks: Vec<ContentBlock>,
    /// 这一趟的改动探针（开始时的样子已经拍在里面）；没有就不出改动摘要（T10a 方案 D10）
    pub changes: Option<Arc<dyn ChangeProbe>>,
}

// 探针是端口对象，不要求它能打印；只说有没有
impl std::fmt::Debug for TurnPlan {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TurnPlan")
            .field("turn", &self.turn)
            .field("entry", &self.entry)
            .field("assistant_name", &self.assistant_name)
            .field("context_project_id", &self.context_project_id)
            .field("spec", &self.spec)
            .field("blocks", &self.blocks)
            .field("changes", &self.changes.is_some())
            .finish()
    }
}

/// 回合从哪儿进。
#[derive(Debug, Clone)]
pub enum TurnEntry {
    /// 新回合（`Queued`）：请求就在 `blocks` 里
    New,
    /// 中断后续接（`Interrupted`）。`request` 只在会话记录用不了、用摘要新开时才用（T06 方案 5.2a）：
    /// 新会话里没有这一轮的请求，要原样再给一次
    Resume { request: ResumeRequest },
}

/// 续接回合的请求，由调用方用和新回合同一个「消息 → 输入块」的函数转好。
#[derive(Debug, Clone)]
pub enum ResumeRequest {
    /// 和新回合会收到的一样：正文全文，附件照新回合的方式附上
    Ready(Vec<ContentBlock>),
    /// 取不全（制品读不到、附件没了）：原因。真要用到它时回合失败，不发残缺的请求
    Unavailable(String),
}

/// 续接回合用摘要新开会话时，请求前面那一句。
pub const REQUEST_HEADER: &str = "[iWork] 这一轮用户的请求是：";

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
    /// 用摘要新开了会话、首条 prompt 还没被收下。被收下才清掉；`run_turn` 收尾或下一次入口时还在，就丢掉新会话、
    /// 退回旧编号（见模块文档「续接失败」）
    unconfirmed: Option<Unconfirmed>,
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
            unconfirmed: None,
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

    /// 这条线身上没有没了结的东西：用摘要新开的会话没被收下、要补记旧编号（[`Self::settle`] 管）；旧进程树没确认
    /// 清空（[`Self::retire`] 管）。调用方丢掉这条线之前要它为真（T10a 方案 D8a）。
    pub fn is_settled(&self) -> bool {
        self.unconfirmed.is_none() && self.unclean.is_none()
    }

    /// 跑一个回合，返回它停在的状态。返回 `Err` 时交接表已撤，会话已丢掉（清不掉的记着，见模块文档）。
    pub async fn run_turn<S, C, I, L, T, H>(
        &mut self,
        agent: &A,
        ports: TurnPorts<'_, S, C, I, L, T, H>,
        plan: TurnPlan,
    ) -> Result<TurnState, TurnRunError>
    where
        S: StoragePort,
        C: Clock,
        I: IdGenerator,
        L: LivePublisher,
        T: Timer,
        H: ReadModelPort,
    {
        // 上一次 run_turn 半路被丢掉或 panic 了：收尾没跑到，在这里补上
        self.settle(ports).await?;
        let result = self.run_turn_inner(agent, ports, plan).await;
        if result.is_err() {
            // 交接表已随 run_turn_inner 返回撤掉；结束运行时，免得它在无人记录时继续动手。
            // 清不掉的记在 unclean 里，下一次开会话前再清；这里交回的是原始错误
            let _ = self.retire().await;
        }
        let settled = self.settle(ports).await;
        let state = result?;
        settled?;
        Ok(state)
    }

    /// 用摘要新开的会话没收下首条 prompt：丢掉它（清不掉的照常记在 unclean 里）、会话线退回旧编号；已给回合落盘了
    /// 新编号的，补提交一条 `TurnSessionBound` 记回旧编号。补提交失败是编排自己的错误（同读历史失败）：
    /// `unconfirmed` 留着，下一次再补，丢会话、退编号这两步不会重做。没有要了结的就什么都不做。
    ///
    /// `run_turn` 的入口与收尾都调它；调用方丢掉这条线之前也调它（T10a 方案 D8a）。
    pub async fn settle<S, C, I, L, T, H>(
        &mut self,
        ports: TurnPorts<'_, S, C, I, L, T, H>,
    ) -> Result<(), TurnRunError>
    where
        S: StoragePort,
        I: IdGenerator,
    {
        let Some((old, bound)) = self.unconfirmed.as_ref().map(|p| (p.old.clone(), p.bound)) else {
            return Ok(());
        };
        if self.resume.as_deref() != Some(old.as_str()) {
            let _ = self.retire().await;
            self.resume = Some(old.clone());
        }
        if let Some(turn_id) = bound {
            let event = TurnSessionBound {
                turn_id,
                runtime_session_id: old,
                replaced: None,
            };
            ports
                .store
                .commit(CommitRequest {
                    command_id: CommandId::from_raw(ports.ids.next()),
                    actor: Actor(ORCHESTRATOR_ACTOR.into()),
                    events: vec![event.into()],
                    outbox: Vec::new(),
                })
                .await?;
        }
        self.unconfirmed = None;
        Ok(())
    }

    async fn run_turn_inner<S, C, I, L, T, H>(
        &mut self,
        agent: &A,
        ports: TurnPorts<'_, S, C, I, L, T, H>,
        plan: TurnPlan,
    ) -> Result<TurnState, TurnRunError>
    where
        S: StoragePort,
        C: Clock,
        I: IdGenerator,
        L: LivePublisher,
        T: Timer,
        H: ReadModelPort,
    {
        let TurnPlan {
            turn,
            entry,
            assistant_name,
            context_project_id,
            spec,
            blocks,
            changes,
        } = plan;
        // 和回合状态对不上的在第一步里报表外转换
        let (first, request) = match entry {
            TurnEntry::New => (TurnTrigger::Started, None),
            TurnEntry::Resume { request } => (TurnTrigger::Resume, Some(request)),
        };
        let mut cancels = self.slot.open(turn.id);
        let slot = self.slot.clone();
        let _guard = SlotGuard(&slot);
        let decider = self.decider.clone();
        let timer = ports.timer;
        let mut run = TurnRun::new(turn, ports, &*decider, &slot, context_project_id, request);
        run.changes = changes;
        let resuming = first == TurnTrigger::Resume;

        // 交出 prompt 之前的三次「看一眼」只为少做无用功；保证在 prompt() 里的裁决点（T10a 方案 D1a）
        if slot.cancel_requested() {
            run.cancel_before_start(resuming).await?;
            return Ok(run.turn.state);
        }
        let (reused, replaced) = match self.ensure_session(agent, &spec).await {
            Ok(Ensured::Reused) => (true, None),
            Ok(Ensured::Opened { replaced }) => (false, replaced),
            Err(reason) => {
                run.fail_before_start(reason).await?;
                return Ok(run.turn.state);
            }
        };
        if slot.cancel_requested() {
            run.cancel_before_start(resuming).await?;
            return Ok(run.turn.state);
        }
        let blocks = match &replaced {
            None => blocks,
            Some(r) => match run.with_summary(blocks, r.reason).await? {
                Ok(blocks) => blocks,
                Err(reason) => {
                    run.fail_before_start(reason).await?;
                    return Ok(run.turn.state);
                }
            },
        };
        if slot.cancel_requested() {
            run.cancel_before_start(resuming).await?;
            return Ok(run.turn.state);
        }
        if replaced.is_some() {
            self.mark_bound(run.turn.id);
        }
        run.started(first, self.resume.clone(), replaced, assistant_name)
            .await?;
        let mut updates = match self.prompt(agent, spec, blocks, reused, &mut run).await? {
            Ok(updates) => {
                // 摘要已送到新会话：新编号从此算数
                self.unconfirmed = None;
                updates
            }
            Err(end) => {
                let process_gone = matches!(end, End::Failed(AgentError::ProcessExited { .. }));
                run.finish(end).await?;
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

    /// 用摘要新开的会话的编号要提交到回合上了（新会话报得出编号时才有那条绑定）。在提交**之前**记：提交在途时
    /// future 被丢掉，那笔照样可能落盘；提交失败时多补记一条旧编号，没有害处。
    fn mark_bound(&mut self, turn_id: TurnId) {
        if self.resume.is_some()
            && let Some(pending) = self.unconfirmed.as_mut()
        {
            pending.bound = Some(turn_id);
        }
    }

    /// 丢掉当前会话：确认整棵进程树清空才放掉。清不掉时会话留着、记下原因，返回错误。没有会话时直接成功。
    /// 调用方丢掉这条线之前、删除对话收线时都经它（T10a 方案 D8a、X3）。
    pub async fn retire(&mut self) -> Result<(), AgentError> {
        let Some(session) = self.session.as_ref() else {
            self.unclean = None;
            return Ok(());
        };
        tracing::debug!("retire: enter");
        if let Err(error) = session.terminate().await {
            tracing::debug!(%error, "retire: exit, tree not cleared");
            self.unclean = Some(error.to_string());
            return Err(error);
        }
        self.unclean = None;
        if let Some(session) = self.session.take() {
            let _ = session.close().await;
        }
        tracing::debug!("retire: exit, session dropped");
        Ok(())
    }

    /// 确保有会话：已有且绑定项相同就沿用，否则按记下的 ID 续接或新开。
    async fn ensure_session(
        &mut self,
        agent: &A,
        spec: &SessionSpec,
    ) -> Result<Ensured, StartRefused> {
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
            return Ok(Ensured::Reused);
        }
        let replaced = self
            .open_session(agent, spec.clone())
            .await
            .map_err(|e| e.to_string())?;
        Ok(Ensured::Opened { replaced })
    }

    /// 开会话，带着记下的旧 id 续接（没有就新开）。续接时运行时回 `SessionLost`：放下旧 id、用同一份 spec 新开
    /// 一次，只试这一次（T06 方案 5.2a），开成了返回被换掉的那条。新开也失败时旧 id 留着：记录已经用不了，
    /// 下一回合会再走这条路、再拼一次摘要，而不是不带上文悄悄新开。
    async fn open_session(
        &mut self,
        agent: &A,
        spec: SessionSpec,
    ) -> Result<Option<SessionReplaced>, OpenError> {
        let old = self.resume.clone();
        let lost = match self.open_with(agent, spec.clone(), old.clone()).await {
            Ok(()) => return Ok(None),
            Err(AgentError::SessionLost { reason }) => reason,
            Err(error) => return Err(OpenError::Agent(error)),
        };
        // 没带旧 id 也报「记录用不了」是适配器的错，按普通失败处理
        let Some(old) = old else {
            return Err(OpenError::Agent(AgentError::SessionLost { reason: lost }));
        };
        match self.open_with(agent, spec, None).await {
            Ok(()) => {
                self.unconfirmed = Some(Unconfirmed {
                    old: old.clone(),
                    bound: None,
                });
                Ok(Some(SessionReplaced {
                    old_runtime_session_id: old,
                    reason: lost,
                }))
            }
            Err(error) => Err(OpenError::Unrecovered { lost, error }),
        }
    }

    async fn open_with(
        &mut self,
        agent: &A,
        mut spec: SessionSpec,
        resume: Option<String>,
    ) -> Result<(), AgentError> {
        spec.resume = resume;
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
    /// prompt 没送到：丢掉旧会话、续接一次（记录用不了就用摘要新开，同 [`Self::open_session`]），先把新会话的
    /// 绑定落盘，再发。外层错误是编排自己的错误，内层是回合的失败结局。
    async fn prompt<S, C, I, L, T, H>(
        &mut self,
        agent: &A,
        spec: SessionSpec,
        blocks: Vec<ContentBlock>,
        reused: bool,
        run: &mut TurnRun<'_, S, C, I, L, T, H, D>,
    ) -> Result<Result<<A::Session as AgentSession>::Updates, End>, TurnRunError>
    where
        S: StoragePort,
        C: Clock,
        I: IdGenerator,
        L: LivePublisher,
        T: Timer,
        H: ReadModelPort,
    {
        let session = self
            .session
            .as_ref()
            .expect("ensure_session 成功后一定有会话");
        if !run.slot.pass_prompt() {
            return Ok(Err(End::Finished(TurnOutcome::Cancelled)));
        }
        let first = session.prompt(blocks.clone()).await;
        if !(reused && matches!(first, Err(AgentError::ProcessExited { .. }))) {
            return Ok(first.map_err(End::Failed));
        }
        if let Err(e) = self.retire().await {
            return Ok(Err(End::Failed(e)));
        }
        let replaced = match self.open_session(agent, spec).await {
            Ok(replaced) => replaced,
            Err(e) => return Ok(Err(e.into())),
        };
        let blocks = match &replaced {
            None => blocks,
            Some(r) => match run.with_summary(blocks, r.reason).await? {
                Ok(blocks) => blocks,
                Err(reason) => return Ok(Err(End::Unrecovered(reason))),
            },
        };
        if replaced.is_some() {
            self.mark_bound(run.turn.id);
        }
        run.bind_session(self.resume.clone(), replaced).await?;
        let session = self.session.as_ref().expect("刚开好会话");
        if !run.slot.pass_prompt() {
            return Ok(Err(End::Finished(TurnOutcome::Cancelled)));
        }
        Ok(session.prompt(blocks).await.map_err(End::Failed))
    }
}

/// 用摘要新开、首条 prompt 还没被收下的会话。
struct Unconfirmed {
    /// 被换掉的旧编号
    old: String,
    /// 新编号已经落盘到这个回合上（`TurnSessionBound`）
    bound: Option<TurnId>,
}

/// `ensure_session` 的结果。
enum Ensured {
    /// 沿用已开着的会话
    Reused,
    /// 开了会话；`replaced`：续接时记录用不了、换成了新会话（T06 方案 5.2a）
    Opened { replaced: Option<SessionReplaced> },
}

/// 开会话失败。
#[derive(Debug)]
enum OpenError {
    Agent(AgentError),
    /// 续接时记录用不了（`lost`），用摘要新开的那一次也没成（`error`）。只试一次，回合失败
    Unrecovered {
        lost: SessionLostReason,
        error: AgentError,
    },
}

impl std::fmt::Display for OpenError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Agent(error) => error.fmt(f),
            Self::Unrecovered { lost, error } => write!(
                f,
                "续接失败（{}），用摘要新开会话也没成：{error}",
                lost_label(*lost)
            ),
        }
    }
}

impl From<OpenError> for End {
    fn from(e: OpenError) -> Self {
        match e {
            OpenError::Agent(error) => End::Failed(error),
            unrecovered => End::Unrecovered(unrecovered.to_string()),
        }
    }
}

/// 回合失败原因里怎么说这三类。
fn lost_label(reason: SessionLostReason) -> &'static str {
    match reason {
        SessionLostReason::NotFound => "会话记录不存在",
        SessionLostReason::Corrupt => "会话记录损坏",
        SessionLostReason::Incompatible => "会话记录版本不兼容",
    }
}

/// 回合怎么结束。
enum End {
    Finished(TurnOutcome),
    Failed(AgentError),
    /// 会话记录用不了，新开会话或补齐请求没成：原因两段都在这句话里
    Unrecovered(String),
}

/// 一个进行中的回合：状态、正文、用量都在这里，只有它写事件。
struct TurnRun<'a, S, C, I, L, T, H, D> {
    turn: Turn,
    ports: TurnPorts<'a, S, C, I, L, T, H>,
    /// 这条会话线的项目（助手私聊里按「助手 × 项目」分线）：写进助手消息，读摘要时按它筛
    line: Option<ProjectId>,
    /// 续接回合的请求（入口是 `Resume` 时才有）；只在用摘要新开会话时用
    request: Option<ResumeRequest>,
    decider: &'a D,
    /// 这个回合的交接表：在途的权限请求只记在这里
    slot: &'a TurnSlot,
    message_id: Option<MessageId>,
    body: String,
    usage: Usage,
    last_checkpoint: Option<MonotonicInstant>,
    checkpointed_len: usize,
    /// 这一趟的改动探针（`TurnPlan.changes`）
    changes: Option<Arc<dyn ChangeProbe>>,
}

impl<'a, S, C, I, L, T, H, D> TurnRun<'a, S, C, I, L, T, H, D>
where
    S: StoragePort,
    C: Clock,
    I: IdGenerator,
    L: LivePublisher,
    T: Timer,
    H: ReadModelPort,
    D: PermissionDecider,
{
    fn new(
        turn: Turn,
        ports: TurnPorts<'a, S, C, I, L, T, H>,
        decider: &'a D,
        slot: &'a TurnSlot,
        line: Option<ProjectId>,
        request: Option<ResumeRequest>,
    ) -> Self {
        Self {
            request,
            turn,
            ports,
            line,
            decider,
            slot,
            message_id: None,
            body: String::new(),
            usage: Usage::default(),
            last_checkpoint: None,
            checkpointed_len: 0,
            changes: None,
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
            self.finished_event(None),
        ];
        self.commit_and_nudge(events, Voiding::of(TurnTrigger::Failed, 0))
            .await
    }

    /// 第一笔提交之前看到取消（T10a 方案 D1a ①③）。新回合：`Queued` → `Cancelled`，带 `TurnFinished`。续接的回合：
    /// 什么都不提交，原样停在 `Interrupted`（它的继续意图什么时候失效由宿主管）。
    async fn cancel_before_start(&mut self, resuming: bool) -> Result<(), TurnRunError> {
        if resuming {
            return Ok(());
        }
        let events = vec![
            self.step(TurnTrigger::Cancel, None)?,
            self.finished_event(None),
        ];
        self.commit_and_nudge(events, Voiding::of(TurnTrigger::Cancel, 0))
            .await
    }

    /// 会话 ID 与回合上记着的不同时，返回要写的绑定事件，并记到回合上。`replaced`：这条会话是续接失败后
    /// 新开的，旧编号与原因随绑定一起记下（审计）。
    fn session_binding(
        &mut self,
        runtime_session_id: Option<String>,
        replaced: Option<SessionReplaced>,
    ) -> Option<DomainEvent> {
        let Some(id) = runtime_session_id else {
            if replaced.is_some() {
                tracing::warn!(turn = %self.turn.id, "用摘要新开的会话没报会话 id，换会话的审计记不下来");
            }
            return None;
        };
        if self.turn.runtime_session_id.as_deref() == Some(id.as_str()) {
            return None;
        }
        self.turn.runtime_session_id = Some(id.clone());
        Some(
            TurnSessionBound {
                turn_id: self.turn.id,
                runtime_session_id: id,
                replaced,
            }
            .into(),
        )
    }

    /// 续接失败、用摘要新开了会话：这一轮的第一条输入前面接上摘要；续接的回合再接上调用方给的请求原文
    /// （新会话里没有它，原来的 `blocks` 只有续接说明）。顺序固定：摘要、请求、原来的 `blocks`。
    /// 内层 `Err` 是回合失败的原因：续接回合的请求取不全，不发残缺的请求。
    async fn with_summary(
        &self,
        blocks: Vec<ContentBlock>,
        lost: SessionLostReason,
    ) -> Result<Result<Vec<ContentBlock>, String>, TurnRunError> {
        let request = match &self.request {
            None => None,
            Some(ResumeRequest::Ready(request)) => Some(request),
            Some(ResumeRequest::Unavailable(why)) => {
                return Ok(Err(format!(
                    "续接失败（{}），这一轮的请求取不全：{why}",
                    lost_label(lost)
                )));
            }
        };
        let source = self
            .ports
            .history
            .summary_source(self.turn.id, self.line, SUMMARY_RECENT_LIMIT)
            .await?;
        let mut first: Vec<ContentBlock> = resume_summary(&source, SummaryLimits::DEFAULT)
            .map(ContentBlock::Text)
            .into_iter()
            .collect();
        if let Some(request) = request {
            first.push(ContentBlock::Text(REQUEST_HEADER.to_owned()));
            first.extend(request.iter().cloned());
        }
        first.extend(blocks);
        Ok(Ok(first))
    }

    /// 回合换了会话（续接时运行时给了新 ID）：发 prompt 之前单独落盘。
    async fn bind_session(
        &mut self,
        runtime_session_id: Option<String>,
        replaced: Option<SessionReplaced>,
    ) -> Result<(), TurnRunError> {
        if let Some(event) = self.session_binding(runtime_session_id, replaced) {
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
        replaced: Option<SessionReplaced>,
        assistant_name: String,
    ) -> Result<(), TurnRunError> {
        let message_id = MessageId::from_raw(self.ports.ids.next());
        self.message_id = Some(message_id);
        let mut events: Vec<DomainEvent> = self
            .session_binding(runtime_session_id, replaced)
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
            context_project_id: self.line,
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
            self.finished_event(None),
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
        let failed = match state {
            TurnState::Cancelling => TurnTrigger::ProcessEnded,
            _ => TurnTrigger::Failed,
        };
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
            End::Failed(error) => triggers.push((failed, Some(error.to_string()))),
            End::Unrecovered(reason) => triggers.push((failed, Some(reason))),
        }
        // 只有做完了的回合问探针；排在「先处理已经收到的取消」之后、最后一笔提交之前（T10a 方案 D10）
        let summary = if triggers.last().map(|(t, _)| *t) == Some(TurnTrigger::Completed) {
            self.change_summary().await
        } else {
            None
        };
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
            events.push(self.finished_event(summary));
        }
        self.commit_and_nudge(events, void).await
    }

    /// 限时问一次改动探针。没有探针 → `None`；探针给不出（出错）、panic、到时 → `None`，记一条 warn 点名回合。
    /// 到时就丢掉探针的 future：探针自己的读由它的实现放在不随这里丢掉的地方跑完（D10 的在途上限）。
    async fn change_summary(&self) -> Option<ChangeSummary> {
        let probe = self.changes.as_ref()?;
        let asked = within(
            self.ports.timer,
            CHANGE_SUMMARY_LIMIT,
            catch_unwind(probe.summary()),
        )
        .await;
        let why = match asked {
            Some(Ok(Some(summary))) => return Some(summary),
            Some(Ok(None)) => "探针给不出改动摘要",
            Some(Err(())) => "改动探针 panic",
            None => "改动探针超时",
        };
        tracing::warn!(turn = %self.turn.id, why, "回合收尾不带改动摘要");
        None
    }

    /// 回合结束事件。`summary` 只在以 `Completed` 收尾时可能有；有改动的文件才盖「搞定」章（技术设计 9.7）。
    fn finished_event(&self, summary: Option<ChangeSummary>) -> DomainEvent {
        TurnFinished {
            turn_id: self.turn.id,
            state: self.turn.state,
            usage: self.usage,
            celebrate: summary.as_ref().is_some_and(|s| s.files_changed > 0),
            change_summary: summary,
        }
        .into()
    }
}

/// 接住 `fut` 在 poll 里的 panic，变成 `Err(())`。探针是端口对象，它 panic 不能带倒整趟编排。
async fn catch_unwind<F: Future>(fut: F) -> Result<F::Output, ()> {
    let mut fut = pin!(fut);
    poll_fn(
        |cx| match std::panic::catch_unwind(AssertUnwindSafe(|| fut.as_mut().poll(cx))) {
            Ok(Poll::Ready(out)) => Poll::Ready(Ok(out)),
            Ok(Poll::Pending) => Poll::Pending,
            Err(_) => Poll::Ready(Err(())),
        },
    )
    .await
}

/// 回合停在的状态 → 助手消息的结局。
fn message_status(state: TurnState) -> MessageStatus {
    match state {
        TurnState::Completed => MessageStatus::Completed,
        TurnState::Failed => MessageStatus::Failed,
        _ => MessageStatus::Interrupted,
    }
}
