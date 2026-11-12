//! 权限请求的接缝：运行时问「能不能做」，由谁来答。
//!
//! - [`PermissionDecider`] 是判定方。T07 的审批引擎实现它；测试与过渡期用 [`StubDecider`]（全部放行 /
//!   全部拒绝，可切换）。
//! - [`PermissionGate`] 是交给适配器的 `PermissionHandler`：它在回合的交接表里登记、等编排受理，再问判定方，
//!   最后把结论交回交接表、等编排的回执。回执 `true` 才把结论交回运行时。写回合状态的只有编排一处。
//!
//! **请求交接是一把锁护着的一张表**（T06 方案 2.6 第 3 条）。处理方（别的线程）与编排只在
//! [`TurnSlot`] 的锁里读写 [`Handoff`]，锁里没有 `await`。「看还受不受理」与「写表」在同一个
//! `&mut Handoff` 方法体里，中间放不了锁；`&mut Handoff` 只能从锁守卫里借出来，所以「同一段锁」由类型保证。
//! 编排停止受理（[`Handoff::stop_accepting`]）在锁里关标志、取走整张表，然后逐格回绝：这一刻每个在等编排的
//! 请求都拿到结论，之后才来的在 `register` / `submit` 里当场被拒。一张票的结局只有三种：被采纳（回执
//! `true`）、被停止取走、停止之后才来。
//!
//! **审批结果绑定回合**：每个回合一张表，回合结束即撤掉。交结论时回合换了、受理已停、票不在「等判定方」，
//! 一律按拒绝答复运行时。迟到的放行因此不会落到别的回合头上。

use std::collections::BTreeMap;
use std::future::{Future, poll_fn};
use std::pin::pin;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use std::task::{Context, Poll, Waker};
use std::time::Duration;

use futures_channel::{mpsc, oneshot};
use iwork_domain::approval::VoidReason;
use iwork_domain::common::Timestamp;
use iwork_domain::event::DomainEvent;
use iwork_domain::id::{ApprovalId, TurnId};
use iwork_ports::agent::{PermissionDecision, PermissionHandler, PermissionRequest};
use iwork_ports::clock::MonotonicInstant;
use iwork_ports::store::StorageError;

/// 一次判定的结论和它对应的审批（T06 方案 2.6 第 3 条）。`approval`：规则直接放行或拒绝、没有审批实体时
/// 为 `None`；经审批得到的答复、一次性授权命中时为 `Some`（T07 方案 6.10.6 第 8 步）。编排采纳结论时
/// 凭它提交 `ApprovalDelivered`。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Decided {
    pub decision: PermissionDecision,
    pub approval: Option<ApprovalId>,
}

/// 判定方算不出作废计划（T07 方案 6.10.8，方案审查 1/F-3）。算不出就报错，不返回空计划——
/// 空计划的意思是「确实没有要作废的」。
#[derive(Debug, thiserror::Error)]
pub enum VoidPlanError {
    /// 屏障的空提交或读事件失败（`Busy`、`Io`）
    #[error(transparent)]
    Storage(#[from] StorageError),
    /// 判定方的状态不可用：折叠对不上日志。文字里点名 seq 与事件种类
    #[error("判定方状态不可用：{0}")]
    Unavailable(String),
}

/// 权限判定方。
pub trait PermissionDecider: Send + Sync + 'static {
    /// 作废计划交出期间占着的东西，丢弃即释放。桩用 `()`。
    type VoidHold: Send;

    /// 判定一次请求。可以一直不返回（等用户），调用方不设超时；回合停下时调用方丢掉这个 future。
    fn decide(
        &self,
        turn_id: TurnId,
        req: &PermissionRequest,
    ) -> impl Future<Output = Decided> + Send;

    /// 回合因 `reason` 结束等待：只算作废计划，不改任何状态。成功时返回的事件由编排和回合状态变化写进
    /// 同一笔提交；`VoidHold` 留到那笔提交返回或到期再丢，期间编排不等别的东西。
    fn void_pending(
        &self,
        turn_id: TurnId,
        reason: VoidReason,
    ) -> impl Future<Output = Result<(Vec<DomainEvent>, Self::VoidHold), VoidPlanError>> + Send;

    /// 编排提交成功后调：只是提醒判定方「有新事件」，漏调不影响正确性；判定方在里面出了错自己记日志。
    /// 编排调它时带期限（`NUDGE_LIMIT`），到期就丢。
    fn committed(&self, _events: &[DomainEvent]) -> impl Future<Output = ()> + Send {
        async {}
    }
}

/// 审批桩：全部放行或全部拒绝，运行中可切换。
#[derive(Debug)]
pub struct StubDecider {
    allow: AtomicBool,
}

/// 桩拒绝时回给模型的话。
pub const STUB_DENY_MESSAGE: &str = "iWork 审批桩当前设为全部拒绝";

impl StubDecider {
    pub fn allow_all() -> Self {
        Self {
            allow: AtomicBool::new(true),
        }
    }

    pub fn deny_all() -> Self {
        Self {
            allow: AtomicBool::new(false),
        }
    }

    /// `true` 全部放行，`false` 全部拒绝。
    pub fn set_allow_all(&self, allow: bool) {
        self.allow.store(allow, Ordering::Release);
    }
}

impl PermissionDecider for StubDecider {
    type VoidHold = ();

    async fn decide(&self, _turn_id: TurnId, _req: &PermissionRequest) -> Decided {
        let decision = if self.allow.load(Ordering::Acquire) {
            PermissionDecision::Allow
        } else {
            PermissionDecision::Deny {
                message: STUB_DENY_MESSAGE.into(),
            }
        };
        Decided {
            decision,
            approval: None,
        }
    }

    /// 桩当场答复，不留审批记录，没有可作废的。
    async fn void_pending(
        &self,
        _turn_id: TurnId,
        _reason: VoidReason,
    ) -> Result<(Vec<DomainEvent>, ()), VoidPlanError> {
        Ok((Vec::new(), ()))
    }
}

/// 一张审批票，在一张交接表里唯一。
pub(crate) type Ticket = u64;

/// 受理那一刻的两种时钟读数；等待上限按经过时间较大的那种算（睡眠期间单调时钟可能不走）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct AcceptedAt {
    pub mono: MonotonicInstant,
    pub wall: Timestamp,
}

impl AcceptedAt {
    /// 从受理到 `now` 经过了多久：两种时钟各自经过的时间取大；墙钟往回拨时按 0 算，单调时钟兜底。
    pub fn elapsed(self, now: AcceptedAt) -> Duration {
        let mono = Duration::from_nanos(now.mono.0.saturating_sub(self.mono.0));
        let wall = Duration::from_millis(now.wall.0.saturating_sub(self.wall.0).max(0) as u64);
        mono.max(wall)
    }
}

/// 处理方在等受理时拿到的：`Some` 是撤销信号的接收端（受理了），`None` 是不受理。
type Admission = Option<oneshot::Receiver<()>>;

/// 一格里放的，正是处理方此刻在等的那个 oneshot 的发送端。
#[derive(Debug)]
enum Stage {
    /// 等受理；`after`：编排要先处理完本回合这么多条更新才受理（前序边界）
    Asked {
        after: u64,
        reply: oneshot::Sender<Admission>,
    },
    /// 已受理，等判定方；编排停止受理时经 `revoke` 撤票
    Deciding {
        accepted: AcceptedAt,
        revoke: oneshot::Sender<()>,
    },
    /// 有结论，等编排的回执
    Decided {
        approval: Option<ApprovalId>,
        ack: oneshot::Sender<bool>,
    },
}

impl Stage {
    /// 照所在的阶段回绝：不受理、撤票、回执 `false`。处理方收到后都答「回合已结束」。
    fn refuse(self) {
        match self {
            Stage::Asked { reply, .. } => {
                let _ = reply.send(None);
            }
            Stage::Deciding { revoke, .. } => {
                let _ = revoke.send(());
            }
            Stage::Decided { ack, .. } => {
                let _ = ack.send(false);
            }
        }
    }

    fn awaits_approval(&self) -> bool {
        matches!(self, Stage::Deciding { .. } | Stage::Decided { .. })
    }
}

/// 一个在途请求：从登记到有结论，恰好占一格。
#[derive(Debug)]
struct InFlight {
    /// 轮到编排处理的先后：登记时、交结论时各取一次
    seq: u64,
    stage: Stage,
}

/// 停止受理时取走的格子；放锁之后再逐格回绝。
#[derive(Debug)]
#[must_use = "取走的格子要回绝，否则处理方一直等"]
pub(crate) struct Refusals(Vec<Stage>);

impl Refusals {
    pub fn deliver(self) {
        for stage in self.0 {
            stage.refuse();
        }
    }
}

/// 回合已停止受理（或不是这个回合、票不在该在的阶段）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Stopped;

/// 登记成功：回合、票，以及等受理的接收端。
#[derive(Debug)]
pub(crate) struct Registered {
    pub turn_id: TurnId,
    pub ticket: Ticket,
    pub asked: oneshot::Receiver<Admission>,
}

/// 编排这一刻该做什么（[`Handoff::poll_work`]）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Work {
    /// 采纳这张票的结论
    Adopt(Ticket),
    /// 受理这张票
    Admit(Ticket),
    /// 排在最前的请求的前序边界还没到：先处理更新
    UpdatesFirst,
    /// 没有要处理的
    Idle,
}

/// 一个回合的交接表。只能经 [`TurnSlot`] 的锁守卫拿到 `&mut`，所以下面每个方法都在锁里跑完。
#[derive(Debug)]
pub(crate) struct Handoff {
    turn_id: TurnId,
    /// 还在受理
    accepting: bool,
    in_flight: BTreeMap<Ticket, InFlight>,
    next_ticket: Ticket,
    next_seq: u64,
    /// 编排没活干时留下的唤醒器
    runner: Option<Waker>,
    /// 取消请求：不带回执，没有人等它。只用来叫醒更新循环；要不要交出 prompt 看 `cancel_requested`
    cancel: mpsc::UnboundedSender<()>,
    /// 这个回合被要求取消了（交出 prompt 前的裁决点在锁里读它，T10a 方案 D1a）
    cancel_requested: bool,
}

impl Handoff {
    fn new(turn_id: TurnId, cancel: mpsc::UnboundedSender<()>, cancel_requested: bool) -> Self {
        Self {
            turn_id,
            accepting: true,
            in_flight: BTreeMap::new(),
            next_ticket: 1,
            next_seq: 0,
            runner: None,
            cancel,
            cancel_requested,
        }
    }

    fn seq(&mut self) -> u64 {
        self.next_seq += 1;
        self.next_seq
    }

    /// 处理方登记一个请求。返回值里的唤醒器要在放锁之后唤醒。
    pub fn register(&mut self, after: u64) -> Result<(Registered, Option<Waker>), Stopped> {
        if !self.accepting {
            return Err(Stopped);
        }
        let ticket = self.next_ticket;
        self.next_ticket += 1;
        let (reply, asked) = oneshot::channel();
        let seq = self.seq();
        self.in_flight.insert(
            ticket,
            InFlight {
                seq,
                stage: Stage::Asked { after, reply },
            },
        );
        let registered = Registered {
            turn_id: self.turn_id,
            ticket,
            asked,
        };
        Ok((registered, self.runner.take()))
    }

    /// 处理方交结论：只有这个回合、还在受理、票在「等判定方」时才收。返回回执的接收端与要唤醒的编排。
    ///
    /// 票号每张表从 1 起，不跨回合。不同回合靠 `turn_id` 分开；同一个回合续接时 `turn_id` 相同，旧处理方
    /// 摸不到新表靠的是另一条约定：续接只发生在中断之后，而中断的回合在 `run_turn` 返回前必须
    /// `retire()` 成功（`terminate()` 先停连接任务，旧处理方随之没了），清不掉就改记 `Failed`、不能续接
    /// （`runner.rs`）。放宽 retire（比如清树失败也允许续接）之前，要先让票号跨回合唯一。
    pub fn submit(
        &mut self,
        turn_id: TurnId,
        ticket: Ticket,
        approval: Option<ApprovalId>,
    ) -> Result<(oneshot::Receiver<bool>, Option<Waker>), Stopped> {
        if turn_id != self.turn_id || !self.accepting {
            return Err(Stopped);
        }
        let seq = self.seq();
        let Some(cell) = self.in_flight.get_mut(&ticket) else {
            return Err(Stopped);
        };
        if !matches!(cell.stage, Stage::Deciding { .. }) {
            return Err(Stopped);
        }
        let (ack, acked) = oneshot::channel();
        // 旧的 revoke 随之丢掉：处理方此后不再听撤销信号
        cell.stage = Stage::Decided { approval, ack };
        cell.seq = seq;
        Ok((acked, self.runner.take()))
    }

    /// 编排看下一件事：`seq` 最小的、在「等受理」或「有结论」的那一格。没活干（或要先处理更新）时记下唤醒器。
    /// 只返回票号，格子留在表里。
    pub fn poll_work(&mut self, consumed: u64, cx: &Context<'_>) -> Work {
        let head = self
            .in_flight
            .iter()
            .filter(|(_, cell)| !matches!(cell.stage, Stage::Deciding { .. }))
            .min_by_key(|(_, cell)| cell.seq);
        let work = match head {
            Some((&ticket, cell)) => match &cell.stage {
                Stage::Decided { .. } => return Work::Adopt(ticket),
                Stage::Asked { after, .. } if *after <= consumed => return Work::Admit(ticket),
                Stage::Asked { .. } => Work::UpdatesFirst,
                Stage::Deciding { .. } => unreachable!("上面已滤掉"),
            },
            None => Work::Idle,
        };
        self.runner = Some(cx.waker().clone());
        work
    }

    /// 受理：「等受理」→「等判定方」。先把 `revoke` 写进格子，再把撤销信号的接收端交给处理方。
    pub fn admit(&mut self, ticket: Ticket, accepted: AcceptedAt) {
        let Some(cell) = self.in_flight.get_mut(&ticket) else {
            return;
        };
        let (revoke, revoked) = oneshot::channel();
        match std::mem::replace(&mut cell.stage, Stage::Deciding { accepted, revoke }) {
            Stage::Asked { reply, .. } => {
                // 处理方已经不在（连接任务结束）：格子留到回合结束由 close() 清走，无害
                let _ = reply.send(Some(revoked));
            }
            other => {
                debug_assert!(false, "受理的票不在等受理：{other:?}");
                cell.stage = other;
            }
        }
    }

    /// 采纳：取走「有结论」的那一格，返回它的审批与回执的发送端（放锁后回 `true`）。不在这个阶段返回 `None`。
    pub fn adopt(&mut self, ticket: Ticket) -> Option<(Option<ApprovalId>, oneshot::Sender<bool>)> {
        if !matches!(
            self.in_flight.get(&ticket).map(|c| &c.stage),
            Some(Stage::Decided { .. })
        ) {
            return None;
        }
        match self.in_flight.remove(&ticket).map(|c| c.stage) {
            Some(Stage::Decided { approval, ack }) => Some((approval, ack)),
            _ => unreachable!("刚检查过"),
        }
    }

    /// 取走那一格，交给调用方放锁后回绝。
    pub fn refuse(&mut self, ticket: Ticket) -> Refusals {
        Refusals(
            self.in_flight
                .remove(&ticket)
                .map(|c| c.stage)
                .into_iter()
                .collect(),
        )
    }

    /// 停止受理：关标志、取走整张表。之后 `register`、`submit` 当场返回 [`Stopped`]。
    pub fn stop_accepting(&mut self) -> Refusals {
        self.accepting = false;
        Refusals(
            std::mem::take(&mut self.in_flight)
                .into_values()
                .map(|c| c.stage)
                .collect(),
        )
    }

    /// 这张票在「有结论」时，它的审批。
    pub fn decided(&self, ticket: Ticket) -> Option<Option<ApprovalId>> {
        match self.in_flight.get(&ticket).map(|c| &c.stage) {
            Some(Stage::Decided { approval, .. }) => Some(*approval),
            _ => None,
        }
    }

    /// 那张票之外，还有没有在等审批（等判定方或有结论）的票。
    pub fn others_await_approval(&self, ticket: Ticket) -> bool {
        self.in_flight
            .iter()
            .any(|(t, c)| *t != ticket && c.stage.awaits_approval())
    }

    pub fn has_deciding(&self) -> bool {
        self.in_flight
            .values()
            .any(|c| matches!(c.stage, Stage::Deciding { .. }))
    }

    /// 有没有一张「等判定方」的票从受理到 `now` 已经满了 `limit`。
    pub fn deciding_overdue(&self, limit: Duration, now: AcceptedAt) -> bool {
        self.in_flight.values().any(|c| match c.stage {
            Stage::Deciding { accepted, .. } => accepted.elapsed(now) >= limit,
            _ => false,
        })
    }

    /// 表里任意一张「有结论」的票（不看 `seq`）。
    pub fn any_decided(&self) -> Option<Ticket> {
        self.in_flight
            .iter()
            .find(|(_, c)| matches!(c.stage, Stage::Decided { .. }))
            .map(|(t, _)| *t)
    }

    #[cfg(test)]
    pub fn is_empty(&self) -> bool {
        self.in_flight.is_empty()
    }
}

/// 交接表和早到的取消，同一把锁护着。
#[derive(Debug, Default)]
pub(crate) struct SlotState {
    /// 当前回合的交接表；回合没开或已结束时为 `None`
    pub handoff: Option<Handoff>,
    /// 交接表还没建时就到了的取消：这个回合一开就算已要求取消（T10a 方案 D1a、1/F-5）
    early_cancel: Option<TurnId>,
}

/// 当前回合的交接表。会话线、权限处理方、取消句柄共用；回合结束时撤掉。
#[derive(Debug, Default)]
pub(crate) struct TurnSlot(Mutex<SlotState>);

impl TurnSlot {
    pub fn lock(&self) -> MutexGuard<'_, SlotState> {
        self.0.lock().unwrap_or_else(|p| p.into_inner())
    }

    /// 拿锁、对当前回合的表做一件事、放锁。没有回合时返回 `None`。
    fn with<R>(&self, f: impl FnOnce(&mut Handoff) -> R) -> Option<R> {
        self.lock().handoff.as_mut().map(f)
    }

    /// 开一个回合的表，返回取消请求的接收端。之前早到的、正是这个回合的取消随之带上；别的回合的丢掉。
    pub fn open(&self, turn_id: TurnId) -> mpsc::UnboundedReceiver<()> {
        let (cancel, cancels) = mpsc::unbounded();
        let mut state = self.lock();
        let early = state.early_cancel.take() == Some(turn_id);
        let old = state.handoff.replace(Handoff::new(turn_id, cancel, early));
        debug_assert!(old.is_none(), "上一个回合的表没有撤掉");
        cancels
    }

    /// 停止受理，再把回合撤掉；没用上的早到取消一并清掉。
    pub fn close(&self) {
        let refusals = {
            let mut state = self.lock();
            state.early_cancel = None;
            state.handoff.take().map(|mut h| h.stop_accepting())
        };
        if let Some(refusals) = refusals {
            refusals.deliver();
        }
    }

    pub fn register(&self, after: u64) -> Result<Registered, Stopped> {
        let (registered, waker) = self.with(|h| h.register(after)).ok_or(Stopped)??;
        if let Some(waker) = waker {
            waker.wake();
        }
        Ok(registered)
    }

    pub fn submit(
        &self,
        turn_id: TurnId,
        ticket: Ticket,
        approval: Option<ApprovalId>,
    ) -> Result<oneshot::Receiver<bool>, Stopped> {
        let (acked, waker) = self
            .with(|h| h.submit(turn_id, ticket, approval))
            .ok_or(Stopped)??;
        if let Some(waker) = waker {
            waker.wake();
        }
        Ok(acked)
    }

    pub fn poll_work(&self, consumed: u64, cx: &Context<'_>) -> Work {
        self.with(|h| h.poll_work(consumed, cx))
            .unwrap_or(Work::Idle)
    }

    pub fn admit(&self, ticket: Ticket, accepted: AcceptedAt) {
        self.with(|h| h.admit(ticket, accepted));
    }

    /// 采纳并回执 `true`。返回这一格的审批；票不在「有结论」时返回 `None`、什么都不做。
    pub fn adopt(&self, ticket: Ticket) -> Option<Option<ApprovalId>> {
        let (approval, ack) = self.with(|h| h.adopt(ticket)).flatten()?;
        let _ = ack.send(true);
        Some(approval)
    }

    pub fn refuse(&self, ticket: Ticket) {
        if let Some(refusals) = self.with(|h| h.refuse(ticket)) {
            refusals.deliver();
        }
    }

    pub fn stop_accepting(&self) {
        if let Some(refusals) = self.with(Handoff::stop_accepting) {
            refusals.deliver();
        }
    }

    /// 只读地看一眼当前回合的表。
    pub fn inspect<R>(&self, f: impl FnOnce(&Handoff) -> R) -> Option<R> {
        self.lock().handoff.as_ref().map(f)
    }

    /// 请求取消某个回合（T10a 方案 D1a）。交接表是这个回合的：置取消标记、叫醒编排。还没有交接表：记下
    /// 「这个回合一开就取消」，`open` 时带上。表是别的回合的：什么都不做（这个回合已经结束）。
    pub fn cancel(&self, turn_id: TurnId) {
        let mut state = self.lock();
        match state.handoff.as_mut() {
            Some(handoff) if handoff.turn_id == turn_id => {
                handoff.cancel_requested = true;
                let _ = handoff.cancel.unbounded_send(());
            }
            Some(_) => {}
            None => state.early_cancel = Some(turn_id),
        }
    }

    /// 交出 prompt 之前的裁决点（T10a 方案 D1a）：锁里看取消标记。没要求取消 → `true`，可以交出；要求了 →
    /// `false`，不交出。取消和裁决在同一把锁里，只有两种先后：取消先到，这一趟没有 prompt；裁决先过，
    /// 之后到的取消照在跑的回合处理（更新循环里的 `on_cancel`）。
    pub fn pass_prompt(&self) -> bool {
        !self.cancel_requested()
    }

    /// 这个回合被要求取消了没有。没有交接表时为 `false`。
    pub fn cancel_requested(&self) -> bool {
        self.inspect(|h| h.cancel_requested).unwrap_or(false)
    }
}

/// 交给适配器的权限处理方。任何一步对不上号（回合已结束、已换、已停止受理）都答拒绝：失败即关闭。
/// 回合停下时它名下没有结论的请求都返回拒绝（`PermissionHandler` 的约定）：编排停止受理时逐格回绝。
#[derive(Debug)]
pub struct PermissionGate<D> {
    pub(crate) slot: Arc<TurnSlot>,
    pub(crate) decider: Arc<D>,
}

/// 回合已不在、已停止、或已不接受审批结果时的拒绝理由。
pub const TURN_GONE_MESSAGE: &str = "回合已结束，权限请求作废";

fn turn_gone() -> PermissionDecision {
    PermissionDecision::Deny {
        message: TURN_GONE_MESSAGE.into(),
    }
}

impl<D: PermissionDecider> PermissionGate<D> {
    /// 登记之后的一段：等受理 → 问判定方（同时听撤销信号）→ 交结论。分段是为了让每次拿锁之后的那一段
    /// 都有名字：屏障测试自己拿着锁调 `Handoff::register` / `submit`，放锁后接着跑这里，走的是同一段代码。
    pub(crate) async fn after_register(
        &self,
        registered: Registered,
        req: &PermissionRequest,
    ) -> PermissionDecision {
        let Registered {
            turn_id,
            ticket,
            asked,
        } = registered;
        let Ok(Some(revoked)) = asked.await else {
            return turn_gone();
        };
        let Some(decided) = first_unless_revoked(self.decider.decide(turn_id, req), revoked).await
        else {
            // 编排先撤了票：回合停了，这个请求没有结论
            return turn_gone();
        };
        // 从这里起不再看撤销信号。结论交给编排，只认它的回执
        let Ok(acked) = self.slot.submit(turn_id, ticket, decided.approval) else {
            return turn_gone();
        };
        after_submit(decided.decision, acked).await
    }
}

/// 交结论之后的一段：回执 `true` 就是交接点，之后没有任何 `await`，没有别的信号能改返回值。
pub(crate) async fn after_submit(
    decision: PermissionDecision,
    acked: oneshot::Receiver<bool>,
) -> PermissionDecision {
    match acked.await {
        Ok(true) => decision,
        _ => turn_gone(),
    }
}

/// 等 `fut`；撤销信号先到（或撤销的发送端没了）就丢掉 `fut`、返回 `None`。两者同时就绪时 `fut` 优先。
async fn first_unless_revoked<F: Future>(
    fut: F,
    mut revoked: oneshot::Receiver<()>,
) -> Option<F::Output> {
    let mut fut = pin!(fut);
    poll_fn(|cx| {
        if let Poll::Ready(out) = fut.as_mut().poll(cx) {
            return Poll::Ready(Some(out));
        }
        if std::pin::Pin::new(&mut revoked).poll(cx).is_ready() {
            return Poll::Ready(None);
        }
        Poll::Pending
    })
    .await
}

impl<D: PermissionDecider> PermissionHandler for PermissionGate<D> {
    async fn on_permission(&self, req: PermissionRequest) -> PermissionDecision {
        let Ok(registered) = self.slot.register(req.updates_before) else {
            return turn_gone();
        };
        self.after_register(registered, &req).await
    }
}

#[cfg(test)]
#[path = "permission_tests.rs"]
mod tests;
