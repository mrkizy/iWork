//! 权限请求的接缝：运行时问「能不能做」，由谁来答。
//!
//! - [`PermissionDecider`] 是判定方。T07 的审批引擎实现它；在那之前用 [`StubDecider`]（全部放行 /
//!   全部拒绝，可切换）。
//! - [`PermissionGate`] 是交给适配器的 `PermissionHandler`：它先告诉回合编排「开始等审批」，
//!   再问判定方，最后告诉编排「审批有结果了」，然后才把结果交回运行时。这样回合状态的
//!   `Running → WaitingApproval → Running` 与运行时的实际顺序一致，而写状态的只有编排一处。

use std::future::Future;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use futures_channel::{mpsc, oneshot};
use iwork_domain::approval::VoidReason;
use iwork_domain::event::DomainEvent;
use iwork_domain::id::TurnId;
use iwork_ports::agent::{PermissionDecision, PermissionHandler, PermissionRequest};

/// 权限判定方。
pub trait PermissionDecider: Send + Sync + 'static {
    /// 判定一次请求。可以一直不返回（等用户），调用方不设超时。
    fn decide(
        &self,
        turn_id: TurnId,
        req: &PermissionRequest,
    ) -> impl Future<Output = PermissionDecision> + Send;

    /// 回合因 `reason` 结束等待：作废它挂着的审批，返回要与回合状态变化写进同一次提交的事件
    /// （`ApprovalVoided`）。没有挂着的审批时返回空。
    fn void_pending(
        &self,
        turn_id: TurnId,
        reason: VoidReason,
    ) -> impl Future<Output = Vec<DomainEvent>> + Send;
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
    async fn decide(&self, _turn_id: TurnId, _req: &PermissionRequest) -> PermissionDecision {
        if self.allow.load(Ordering::Acquire) {
            PermissionDecision::Allow
        } else {
            PermissionDecision::Deny {
                message: STUB_DENY_MESSAGE.into(),
            }
        }
    }

    /// 桩当场答复，不留审批记录，没有可作废的。
    async fn void_pending(&self, _turn_id: TurnId, _reason: VoidReason) -> Vec<DomainEvent> {
        Vec::new()
    }
}

/// 编排收到的输入。权限两步都带回执：编排把状态变化写进存储之后才回，
/// 运行时因此不会在状态落盘前继续往下走。
#[derive(Debug)]
pub(crate) enum Signal {
    /// 回执是当前回合的 id；`None` 表示现在没有可以等审批的回合
    PermissionAsked {
        reply: oneshot::Sender<Option<TurnId>>,
    },
    PermissionDecided {
        ack: oneshot::Sender<()>,
    },
    /// 只取消这一回合；到达时回合已换了就忽略
    Cancel(TurnId),
}

/// 交给适配器的权限处理方。任何一步联系不上编排（回合已结束）都答拒绝：失败即关闭。
#[derive(Debug)]
pub struct PermissionGate<D> {
    pub(crate) mailbox: mpsc::UnboundedSender<Signal>,
    pub(crate) decider: Arc<D>,
}

/// 回合已不在时的拒绝理由。
pub const TURN_GONE_MESSAGE: &str = "回合已结束，权限请求作废";

fn turn_gone() -> PermissionDecision {
    PermissionDecision::Deny {
        message: TURN_GONE_MESSAGE.into(),
    }
}

impl<D: PermissionDecider> PermissionHandler for PermissionGate<D> {
    async fn on_permission(&self, req: PermissionRequest) -> PermissionDecision {
        let (reply, asked) = oneshot::channel();
        if self
            .mailbox
            .unbounded_send(Signal::PermissionAsked { reply })
            .is_err()
        {
            return turn_gone();
        }
        let Ok(Some(turn_id)) = asked.await else {
            return turn_gone();
        };
        let decision = self.decider.decide(turn_id, &req).await;
        let (ack, acked) = oneshot::channel();
        if self
            .mailbox
            .unbounded_send(Signal::PermissionDecided { ack })
            .is_err()
        {
            return turn_gone();
        }
        match acked.await {
            Ok(()) => decision,
            Err(_) => turn_gone(),
        }
    }
}
