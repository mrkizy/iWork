//! 回合：一次 prompt 到结束（技术设计 6.4）。

use crate::approval::VoidReason;
use crate::common::{ModelSelection, Timestamp};
use crate::id::{AssistantId, ConversationId, RuntimeId, TurnId};
use crate::machine::{IllegalTransition, Transition, finite_enum, lookup};

finite_enum! {
    /// 回合为什么中断。中断不是终态：用户可以继续或放弃。
    #[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
    pub enum InterruptReason {
        /// 后台服务重启时进程已不在（崩溃恢复，技术设计 4）
        Crash,
        /// 等审批太久，为防 hook 自然超时（超时即放行）主动中断了回合（技术设计 6.3）。
        /// 这种中断**不**作废审批，审批继续等用户。
        ApprovalWaitLimit,
    }
}

finite_enum! {
    /// 回合状态。终态：`Completed`、`Cancelled`、`Failed`。
    ///
    /// 只放决定「下一步能做什么」的信息；用量、失败原因等随事件（`TurnFinished`、
    /// `TurnStateChanged.reason`）记录，不进状态，所以状态与触发都能穷举、转换表能逐对检查。
    #[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
    pub enum TurnState {
        Queued,
        Running,
        WaitingApproval,
        Cancelling,
        Interrupted { reason: InterruptReason },
        Completed,
        Cancelled,
        Failed,
    }
}

finite_enum! {
    /// 回合的触发：名字取自引起它的领域事件或命令。
    #[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
    pub enum TurnTrigger {
        /// 会话开好、prompt 已发
        Started,
        /// `CancelTurn` / `CancelAll` / 删除对话的收尾
        Cancel,
        /// 运行时缺失、模型不可用、运行中出错，或后台服务还在时运行时进程意外退出
        Failed,
        PermissionAsked,
        /// 放行或拒绝都回到运行
        PermissionDecided,
        /// 取消中的进程结束了（中断成功或 3 秒后结束进程树）
        ProcessEnded,
        Completed,
        /// 崩溃恢复：后台服务重启时进程已不在
        ProcessLost,
        WaitLimitReached,
        /// `ResumeTurn`，或中断后审批被批准、留下一次性放行时自动续接（引擎的 `turns_to_resume`）
        Resume,
        /// `AbandonTurn`；它的待审批随之作废
        Abandon,
    }
}

use InterruptReason::{ApprovalWaitLimit, Crash};
use TurnState as S;
use TurnTrigger as T;

/// 回合转换表（方案 4.3）。改转换只改这里。
pub const TURN_TRANSITIONS: &[Transition<TurnState, TurnTrigger>] = &[
    (S::Queued, T::Started, S::Running),
    (S::Queued, T::Cancel, S::Cancelled),
    (S::Queued, T::Failed, S::Failed),
    (S::Running, T::PermissionAsked, S::WaitingApproval),
    (S::WaitingApproval, T::PermissionDecided, S::Running),
    (S::Running, T::Cancel, S::Cancelling),
    (S::WaitingApproval, T::Cancel, S::Cancelling),
    (S::Cancelling, T::ProcessEnded, S::Cancelled),
    (S::Running, T::Completed, S::Completed),
    (S::Running, T::Failed, S::Failed),
    // 等审批时运行时出错或进程意外退出（后台服务还在）：与运行中出错同样失败，不经过假的「审批有结果」
    (S::WaitingApproval, T::Failed, S::Failed),
    (S::Running, T::ProcessLost, S::Interrupted { reason: Crash }),
    (
        S::WaitingApproval,
        T::ProcessLost,
        S::Interrupted { reason: Crash },
    ),
    (
        S::WaitingApproval,
        T::WaitLimitReached,
        S::Interrupted {
            reason: ApprovalWaitLimit,
        },
    ),
    // 正在取消时后台服务重启、进程已不在：用户本来就要取消，直接算取消，不算中断
    (S::Cancelling, T::ProcessLost, S::Cancelled),
    (S::Interrupted { reason: Crash }, T::Resume, S::Running),
    (
        S::Interrupted {
            reason: ApprovalWaitLimit,
        },
        T::Resume,
        S::Running,
    ),
    (S::Interrupted { reason: Crash }, T::Abandon, S::Cancelled),
    (
        S::Interrupted {
            reason: ApprovalWaitLimit,
        },
        T::Abandon,
        S::Cancelled,
    ),
];

/// 回合状态机名，出现在 [`IllegalTransition`] 里。
pub const TURN_MACHINE: &str = "turn";

/// 回合的表外转换错误。
pub type TurnIllegal = IllegalTransition<TurnState, TurnTrigger>;

/// 按 [`TURN_TRANSITIONS`] 求下一状态；表外返回点名的 [`IllegalTransition`]。
pub fn next(state: TurnState, trigger: TurnTrigger) -> Result<TurnState, TurnIllegal> {
    lookup(TURN_MACHINE, TURN_TRANSITIONS, state, trigger)
}

impl TurnTrigger {
    /// 这个触发让回合挂着的审批作废时，作废的原因（需求第 12 章）。
    /// 等待上限引起的中断（`WaitLimitReached`）不作废审批，审批继续等用户。
    pub fn void_reason(self) -> Option<VoidReason> {
        match self {
            T::Cancel => Some(VoidReason::TurnCancelled),
            T::Failed => Some(VoidReason::TurnFailed),
            T::Abandon => Some(VoidReason::TurnAbandonedAfterInterrupt),
            T::Started
            | T::PermissionAsked
            | T::PermissionDecided
            | T::ProcessEnded
            | T::Completed
            | T::ProcessLost
            | T::WaitLimitReached
            | T::Resume => None,
        }
    }
}

impl TurnState {
    pub fn is_terminal(self) -> bool {
        matches!(self, S::Completed | S::Cancelled | S::Failed)
    }

    /// 这个状态下回合是否持有运行时进程。崩溃恢复只对这些状态的回合发 `ProcessLost`：
    /// `Queued` 还没起进程，重启后照常排队；`Interrupted` 已经不在跑。
    /// 对其他状态发 `ProcessLost` 是调用方漏了过滤，返回 [`IllegalTransition`]。
    pub fn holds_process(self) -> bool {
        matches!(self, S::Running | S::WaitingApproval | S::Cancelling)
    }
}

/// 回合由谁发起（技术设计 7.2b）。系统回合的工具调用不经 AI 审查、不询问用户。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum TurnKind {
    Assistant,
    SystemAiReview,
    SystemAgentsMdDraft,
}

/// 回合实体。状态只经 [`Turn::apply`] 改变。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Turn {
    pub id: TurnId,
    pub conversation_id: ConversationId,
    pub assistant_id: AssistantId,
    /// 回合开始时助手配置的版本，配置后续再改也不影响这一回合
    pub assistant_version: u32,
    pub runtime_id: RuntimeId,
    pub model: ModelSelection,
    /// 运行时自己的会话 ID，续接（resume）用；不透明
    pub runtime_session_id: Option<String>,
    pub kind: TurnKind,
    pub state: TurnState,
    pub started_at: Timestamp,
    pub ended_at: Option<Timestamp>,
}

impl Turn {
    /// 应用一个触发；进入终态时记下 `ended_at`。表外转换不改任何字段。
    pub fn apply(&mut self, trigger: TurnTrigger, at: Timestamp) -> Result<TurnState, TurnIllegal> {
        let to = next(self.state, trigger)?;
        self.state = to;
        if to.is_terminal() {
            self.ended_at = Some(at);
        }
        Ok(to)
    }
}
