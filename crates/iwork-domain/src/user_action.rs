//! 亲手执行卡片（技术设计 7.2a；方案 4.3）。
//!
//! 底线拦的是 Agent 不是用户：需要动底线范围内的内容时生成卡片，由用户点「执行」。
//! 结果不确定时（等待失败、后台服务中断）卡片进 `Unknown`：绝不自动重跑、绝不自动判为结束，
//! 唯一出口是用户确认（`UserConfirmedEnded`）。
//!
//! 状态里的退出码、错误文字不可穷举，所以转换表按「种类」写：[`USER_ACTION_TRANSITIONS`]
//! 只管合不合法，载荷由 [`next`] 从触发里取。

use crate::action::CanonicalPath;
use crate::approval::VoidReason;
use crate::common::{ArtifactRef, Sha256};
use crate::id::{ConversationId, TurnId, UserActionId};
use crate::machine::{IllegalTransition, Transition, finite_enum, lookup};

/// 进程怎么结束的。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ExitStatus {
    Code(i32),
    /// 被信号结束（Windows 上没有，只会是 `Code`）
    Signal(i32),
}

/// 崩溃恢复时卡片转 `Unknown` 带的错误文字。
pub const PROCESS_LOST_ERROR: &str = "后台服务中断";

/// 卡片状态。终态：`Done`、`Ended`、`Failed`、`Dismissed`、`Voided`。
/// `Unknown` 不是终态：结果未知、iWork 已不再跟踪，用户确认前不算结束。
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum UserActionState {
    Pending,
    /// 哈希已校验、进程还没起
    Starting,
    /// 进程起来了，后台服务持有执行句柄（只在内存里，不落盘）
    Executing,
    Stopping,
    Done {
        exit: ExitStatus,
    },
    /// 明确知道没起来（`spawn` 返回错误）。不自动重试
    Failed {
        error: String,
    },
    Unknown {
        error: String,
    },
    /// 用户核对后确认结束
    Ended,
    Dismissed,
    Voided {
        reason: VoidReason,
    },
}

finite_enum! {
    /// 卡片状态的种类（去掉载荷），转换表按它写。
    #[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
    pub enum UserActionStateKind {
        Pending,
        Starting,
        Executing,
        Stopping,
        Done,
        Failed,
        Unknown,
        Ended,
        Dismissed,
        Voided,
    }
}

/// 卡片触发（带载荷）。
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum UserActionTrigger {
    /// `ExecuteUserAction`，`proposal_hash` 已由应用服务校验通过
    Execute,
    Dismiss,
    Void {
        reason: VoidReason,
    },
    Spawned,
    SpawnFailed {
        error: String,
    },
    Finished {
        exit: ExitStatus,
    },
    /// 用户点停止，或删除对话 / 删除助手的收尾流程发起
    Stop,
    /// 句柄 `wait()` 报错：host 先尽力停止，再进 `Unknown`
    WaitFailed {
        error: String,
    },
    /// 崩溃恢复：后台服务重启后不再持有这个进程
    ProcessLost,
    /// `ConfirmUserActionEnded`：用户自己核对过了
    UserConfirmedEnded,
}

finite_enum! {
    /// 卡片触发的种类（去掉载荷）。
    #[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
    pub enum UserActionTriggerKind {
        Execute,
        Dismiss,
        Void,
        Spawned,
        SpawnFailed,
        Finished,
        Stop,
        WaitFailed,
        ProcessLost,
        UserConfirmedEnded,
    }
}

use UserActionStateKind as K;
use UserActionTriggerKind as TK;

/// 卡片转换表（方案 4.3）。没有任何一行回到 `Starting` 以外的起点：卡片只会被执行一次。
pub const USER_ACTION_TRANSITIONS: &[Transition<UserActionStateKind, UserActionTriggerKind>] = &[
    (K::Pending, TK::Execute, K::Starting),
    (K::Pending, TK::Dismiss, K::Dismissed),
    (K::Pending, TK::Void, K::Voided),
    (K::Starting, TK::Spawned, K::Executing),
    (K::Starting, TK::SpawnFailed, K::Failed),
    (K::Executing, TK::Finished, K::Done),
    (K::Executing, TK::Stop, K::Stopping),
    (K::Stopping, TK::Finished, K::Done),
    (K::Executing, TK::WaitFailed, K::Unknown),
    (K::Stopping, TK::WaitFailed, K::Unknown),
    // 崩溃恢复（做法 A）：Starting 的进程可能已起来并产生副作用，三个状态一律结果未知
    (K::Starting, TK::ProcessLost, K::Unknown),
    (K::Executing, TK::ProcessLost, K::Unknown),
    (K::Stopping, TK::ProcessLost, K::Unknown),
    (K::Unknown, TK::UserConfirmedEnded, K::Ended),
];

pub const USER_ACTION_MACHINE: &str = "user_action";

/// 卡片的表外转换错误（按种类点名）。
pub type UserActionIllegal = IllegalTransition<UserActionStateKind, UserActionTriggerKind>;

/// 按种类查表，不涉及载荷。
pub fn next_kind(
    state: UserActionStateKind,
    trigger: UserActionTriggerKind,
) -> Result<UserActionStateKind, UserActionIllegal> {
    lookup(USER_ACTION_MACHINE, USER_ACTION_TRANSITIONS, state, trigger)
}

/// 求下一状态：先按种类查 [`USER_ACTION_TRANSITIONS`] 判合法，再从触发里取载荷。
pub fn next(
    state: &UserActionState,
    trigger: UserActionTrigger,
) -> Result<UserActionState, UserActionIllegal> {
    let to_kind = next_kind(state.kind(), trigger.kind())?;
    let to = match trigger {
        UserActionTrigger::Execute => UserActionState::Starting,
        UserActionTrigger::Dismiss => UserActionState::Dismissed,
        UserActionTrigger::Void { reason } => UserActionState::Voided { reason },
        UserActionTrigger::Spawned => UserActionState::Executing,
        UserActionTrigger::SpawnFailed { error } => UserActionState::Failed { error },
        UserActionTrigger::Finished { exit } => UserActionState::Done { exit },
        UserActionTrigger::Stop => UserActionState::Stopping,
        UserActionTrigger::WaitFailed { error } => UserActionState::Unknown { error },
        UserActionTrigger::ProcessLost => UserActionState::Unknown {
            error: PROCESS_LOST_ERROR.to_owned(),
        },
        UserActionTrigger::UserConfirmedEnded => UserActionState::Ended,
    };
    debug_assert_eq!(
        to.kind(),
        to_kind,
        "触发的载荷与 USER_ACTION_TRANSITIONS 的目标种类不一致"
    );
    Ok(to)
}

impl UserActionState {
    pub fn kind(&self) -> UserActionStateKind {
        match self {
            Self::Pending => K::Pending,
            Self::Starting => K::Starting,
            Self::Executing => K::Executing,
            Self::Stopping => K::Stopping,
            Self::Done { .. } => K::Done,
            Self::Failed { .. } => K::Failed,
            Self::Unknown { .. } => K::Unknown,
            Self::Ended => K::Ended,
            Self::Dismissed => K::Dismissed,
            Self::Voided { .. } => K::Voided,
        }
    }

    pub fn is_terminal(&self) -> bool {
        self.kind().is_terminal()
    }
}

impl UserActionStateKind {
    pub fn is_terminal(self) -> bool {
        matches!(
            self,
            K::Done | K::Ended | K::Failed | K::Dismissed | K::Voided
        )
    }
}

impl UserActionTrigger {
    pub fn kind(&self) -> UserActionTriggerKind {
        match self {
            Self::Execute => TK::Execute,
            Self::Dismiss => TK::Dismiss,
            Self::Void { .. } => TK::Void,
            Self::Spawned => TK::Spawned,
            Self::SpawnFailed { .. } => TK::SpawnFailed,
            Self::Finished { .. } => TK::Finished,
            Self::Stop => TK::Stop,
            Self::WaitFailed { .. } => TK::WaitFailed,
            Self::ProcessLost => TK::ProcessLost,
            Self::UserConfirmedEnded => TK::UserConfirmedEnded,
        }
    }
}

/// 卡片的提议内容：精确命令或补丁。
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum Proposal {
    Command {
        command: String,
        cwd: CanonicalPath,
    },
    Patch {
        path: CanonicalPath,
        old: String,
        new: String,
    },
}

/// 亲手执行卡片实体。不记进程身份：崩溃后 iWork 不核对、不停止（方案第 8 节已知限制）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UserActionCard {
    pub id: UserActionId,
    pub turn_id: TurnId,
    pub conversation_id: ConversationId,
    pub proposal: Proposal,
    pub reason: String,
    /// 提议内容的哈希；`ExecuteUserAction` 带来的哈希不符时拒绝执行
    pub proposal_hash: Sha256,
    pub state: UserActionState,
    /// 已收到的输出。只给用户看（技术设计 7.2a 第 5 条），Agent 只得知是否执行与退出码
    pub output_artifact: Option<ArtifactRef>,
}

impl UserActionCard {
    /// 应用一个触发。表外转换不改任何字段。
    pub fn apply(&mut self, trigger: UserActionTrigger) -> Result<(), UserActionIllegal> {
        self.state = next(&self.state, trigger)?;
        Ok(())
    }
}
