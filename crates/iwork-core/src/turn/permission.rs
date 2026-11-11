//! 权限请求的接缝：运行时问「能不能做」，由谁来答。
//!
//! - [`PermissionDecider`] 是判定方。T07 的审批引擎实现它；在那之前用 [`StubDecider`]（全部放行 /
//!   全部拒绝，可切换）。
//! - [`PermissionGate`] 是交给适配器的 `PermissionHandler`：它先告诉回合编排「开始等审批」并领一张
//!   票，再问判定方，最后凭票告诉编排「审批有结果了」，编排认可后才把结果交回运行时。
//!   写回合状态的只有编排一处。
//!
//! **审批结果绑定回合**：每个回合有自己的信箱（[`TurnSlot`]），回合结束即撤掉。领票时记下回合 id，
//! 交结果时回合已换了、信箱已撤、票不在待处理集合里、或回合已不在等审批，一律按拒绝答复运行时，
//! 不改任何计数。迟到的放行因此不会落到别的回合头上。

use std::future::Future;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use futures_channel::{mpsc, oneshot};
use iwork_domain::approval::VoidReason;
use iwork_domain::event::DomainEvent;
use iwork_domain::id::{ApprovalId, TurnId};
use iwork_ports::agent::{PermissionDecision, PermissionHandler, PermissionRequest};
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

    /// 判定一次请求。可以一直不返回（等用户），调用方不设超时。
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

/// 审批桩：全部放行或全部拒绝，运行中可切换。T07 完成后由审批引擎替换。
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

/// 一张审批票。进程内全局递增、永不重复：迟到的结果即使混进了别的回合的信箱，也对不上那个
/// 回合的任何一张票。
pub(crate) type Ticket = u64;

static NEXT_TICKET: AtomicU64 = AtomicU64::new(1);

pub(crate) fn new_ticket() -> Ticket {
    NEXT_TICKET.fetch_add(1, Ordering::Relaxed)
}

/// 回合编排从权限处理方收到的信号。都带回执：编排把状态变化写进存储之后才回，
/// 运行时因此不会在状态落盘前继续。
#[derive(Debug)]
pub(crate) enum Signal {
    /// 回执是票；`None` 表示现在不受理审批（如正在取消）。`after`：编排要先处理完本回合
    /// 这么多条更新才受理（前序边界，见 `PermissionRequest::updates_before`）
    PermissionAsked {
        reply: oneshot::Sender<Option<Ticket>>,
        after: u64,
    },
    /// 回执 `true` 表示结果被采纳。`approval`：结果出自哪条审批；有值时编排采纳时提交 `ApprovalDelivered`
    PermissionDecided {
        ticket: Ticket,
        approval: Option<ApprovalId>,
        ack: oneshot::Sender<bool>,
    },
}

/// 当前回合的信箱。会话线、权限处理方、取消句柄共用；回合结束时清空。
#[derive(Debug, Default)]
pub(crate) struct TurnSlot(Mutex<Option<TurnInbox>>);

#[derive(Debug, Clone)]
pub(crate) struct TurnInbox {
    pub turn_id: TurnId,
    pub signals: mpsc::UnboundedSender<Signal>,
    pub cancel: mpsc::UnboundedSender<()>,
}

impl TurnSlot {
    fn lock(&self) -> std::sync::MutexGuard<'_, Option<TurnInbox>> {
        self.0.lock().unwrap_or_else(|p| p.into_inner())
    }

    pub fn open(&self, inbox: TurnInbox) {
        *self.lock() = Some(inbox);
    }

    pub fn close(&self) {
        *self.lock() = None;
    }

    /// 当前回合的信箱；`turn_id` 给了就必须是这个回合。
    pub fn current(&self, turn_id: Option<TurnId>) -> Option<TurnInbox> {
        self.lock()
            .as_ref()
            .filter(|inbox| turn_id.is_none_or(|id| inbox.turn_id == id))
            .cloned()
    }
}

/// 交给适配器的权限处理方。任何一步对不上号（回合已结束、已换、正在取消）都答拒绝：失败即关闭。
#[derive(Debug)]
pub struct PermissionGate<D> {
    pub(crate) slot: Arc<TurnSlot>,
    pub(crate) decider: Arc<D>,
}

/// 回合已不在、或已不接受审批结果时的拒绝理由。
pub const TURN_GONE_MESSAGE: &str = "回合已结束，权限请求作废";

fn turn_gone() -> PermissionDecision {
    PermissionDecision::Deny {
        message: TURN_GONE_MESSAGE.into(),
    }
}

impl<D: PermissionDecider> PermissionHandler for PermissionGate<D> {
    async fn on_permission(&self, req: PermissionRequest) -> PermissionDecision {
        let Some(inbox) = self.slot.current(None) else {
            return turn_gone();
        };
        let (reply, asked) = oneshot::channel();
        if inbox
            .signals
            .unbounded_send(Signal::PermissionAsked {
                reply,
                after: req.updates_before,
            })
            .is_err()
        {
            return turn_gone();
        }
        let Ok(Some(ticket)) = asked.await else {
            return turn_gone();
        };
        let Decided { decision, approval } = self.decider.decide(inbox.turn_id, &req).await;
        // 判定期间回合可能已经结束、换成别的回合：只把结果交给领票时的那个回合
        let Some(inbox) = self.slot.current(Some(inbox.turn_id)) else {
            return turn_gone();
        };
        let (ack, acked) = oneshot::channel();
        if inbox
            .signals
            .unbounded_send(Signal::PermissionDecided {
                ticket,
                approval,
                ack,
            })
            .is_err()
        {
            return turn_gone();
        }
        match acked.await {
            Ok(true) => decision,
            _ => turn_gone(),
        }
    }
}
