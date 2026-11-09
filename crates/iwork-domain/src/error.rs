//! 领域错误。每个变体带点名字段（哪个 ID、哪个状态、哪个触发），读日志的人一眼知道哪里坏了。
//!
//! 对外错误码的映射在 `iwork-app`（错误码定义在 Protobuf 里，domain 不依赖它）。

use core::fmt::Debug;

use crate::approval::ApprovalApplyError;
use crate::id::{ConversationId, IdParseError, ProjectId, UserActionId};
use crate::machine::IllegalTransition;
use crate::user_action::UserActionStateKind;

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum DomainError {
    /// 状态机表外转换：程序缺陷。状态与触发转成文字，便于跨状态机统一上报。
    #[error("{machine} 状态机：状态 {state} 不接受触发 {trigger}")]
    IllegalTransition {
        machine: &'static str,
        state: String,
        trigger: String,
    },
    #[error(transparent)]
    IdParse(#[from] IdParseError),
    #[error(transparent)]
    Approval(#[from] ApprovalApplyError),
    /// 配置值不合法（例如超时为 0）
    #[error("配置 {field} 不合法：{reason}")]
    InvalidConfig { field: &'static str, reason: String },
    /// 对话正在删除，只受理收尾命令（映射 `CONVERSATION_DELETING`）
    #[error("对话 {id} 正在删除，只受理收尾命令")]
    ConversationDeleting { id: ConversationId },
    /// 对话已删除（映射 `NOT_FOUND`）
    #[error("对话 {id} 已删除")]
    ConversationDeleted { id: ConversationId },
    /// 助手私聊 M1 不提供删除（技术设计 9.0）
    #[error("对话 {id} 是助手私聊，不能删除")]
    AssistantDmNotDeletable { id: ConversationId },
    /// 只有助手私聊能切换所选项目
    #[error("对话 {id} 不是助手私聊，不能切换项目")]
    NotAssistantDm { id: ConversationId },
    /// 收尾命令指向的卡片不属于这个对话
    #[error("卡片 {card} 不属于对话 {conversation}")]
    CardNotInConversation {
        card: UserActionId,
        conversation: ConversationId,
    },
    /// 停止只对执行中、且本次后台服务仍持有句柄的卡片有效
    #[error("卡片 {card} 不能停止：状态 {state:?}，持有句柄 {handle_held}")]
    UserActionNotStoppable {
        card: UserActionId,
        state: UserActionStateKind,
        handle_held: bool,
    },
    /// 重新定位时新位置的项目 ID 与原项目不一致（映射 `PROJECT_ID_MISMATCH`）
    #[error("新位置的项目 ID {found} 与项目 {expected} 不一致")]
    ProjectIdMismatch {
        expected: ProjectId,
        found: ProjectId,
    },
}

impl<S: Debug, T: Debug> From<IllegalTransition<S, T>> for DomainError {
    fn from(e: IllegalTransition<S, T>) -> Self {
        Self::IllegalTransition {
            machine: e.machine,
            state: format!("{:?}", e.state),
            trigger: format!("{:?}", e.trigger),
        }
    }
}
