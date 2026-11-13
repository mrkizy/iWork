//! 受理命令时点名的拒绝，以及它们对外的错误码（T10a 方案 2.3、2.6）。
//!
//! 各命令的计划函数（`turn_command`、`conversation_command`、`settings_command`）只返回 [`Refusal`]，
//! 错误码只在 [`Refusal::to_error`] 这一处定。

use std::collections::BTreeMap;

use iwork_domain::DomainError;
use iwork_domain::id::TurnId;
use iwork_domain::turn::TurnState;
use iwork_proto::pb;

use crate::convert::ConvertError;

/// 一条命令不受理的原因。没有一种是「存储忙」：那是提交的结果，由外壳映射。
#[derive(Debug, Clone, PartialEq, thiserror::Error)]
pub enum Refusal {
    /// 领域规则不放行（对话在删、已删、私聊不能删……）
    #[error(transparent)]
    Domain(#[from] DomainError),
    /// 命令里的 proto 字段解不出来
    #[error(transparent)]
    Convert(#[from] ConvertError),
    #[error("找不到 {entity} {id}")]
    NotFound { entity: &'static str, id: String },
    /// 已中断的回合不能取消，要用「放弃」
    #[error("回合 {turn} 已中断，用「放弃」结束它")]
    TurnNotCancellable { turn: TurnId },
    /// 只有已中断的回合能继续或放弃
    #[error("回合 {turn} 是 {state:?}，不在已中断")]
    TurnNotResumable { turn: TurnId, state: TurnState },
    /// 参数本身不合法（空消息、相对路径……）
    #[error("{field} 不合法：{reason}")]
    InvalidArgument { field: &'static str, reason: String },
    /// 项目默认档位只能是手动或自动
    #[error("项目默认档位不能是全开")]
    ApprovalModeNotAllowed,
    /// 内置条目、安全清单条目不能撤销
    #[error("规则 {rule} 不能撤销")]
    RuleNotRevocable { rule: String },
    /// 这部分归别的卡（附件、助手管理……），还没接上
    #[error("未接入：{what}")]
    Unwired { what: &'static str },
    /// 后台在退出：不再收会起新活的命令（A6）
    #[error("后台服务正在退出，请稍后重试")]
    Closing,
}

impl Refusal {
    pub fn code(&self) -> pb::ErrorCode {
        use pb::ErrorCode as C;
        match self {
            Self::Domain(e) => match e {
                DomainError::ConversationDeleting { .. } => C::ConversationDeleting,
                DomainError::ConversationDeleted { .. } => C::NotFound,
                DomainError::AssistantDmNotDeletable { .. }
                | DomainError::NotAssistantDm { .. }
                | DomainError::CardNotInConversation { .. }
                | DomainError::UserActionNotStoppable { .. } => C::PreconditionFailed,
                DomainError::InvalidConfig { .. } | DomainError::IdParse(_) => C::InvalidArgument,
                DomainError::ProjectIdMismatch { .. } => C::ProjectIdMismatch,
                DomainError::IllegalTransition { .. } | DomainError::Approval(_) => C::Internal,
            },
            Self::Convert(_) => C::InvalidArgument,
            Self::NotFound { .. } => C::NotFound,
            Self::TurnNotCancellable { .. } => C::TurnNotCancellable,
            Self::TurnNotResumable { .. } => C::TurnNotResumable,
            Self::InvalidArgument { .. } => C::InvalidArgument,
            Self::ApprovalModeNotAllowed => C::ApprovalModeNotAllowed,
            Self::RuleNotRevocable { .. } => C::RuleNotRevocable,
            Self::Unwired { .. } | Self::Closing => C::Unavailable,
        }
    }

    /// 重发有没有可能成功：只有「后台在退出」这一种（重开后再发）。
    pub fn retryable(&self) -> bool {
        matches!(self, Self::Closing)
    }

    pub fn to_error(&self) -> pb::Error {
        let details = match self {
            Self::Convert(e) => BTreeMap::from([
                (crate::details::KIND.to_owned(), e.kind().to_owned()),
                (crate::details::FIELD.to_owned(), e.field().to_owned()),
            ]),
            _ => BTreeMap::new(),
        };
        pb::Error {
            code: self.code().into(),
            message: self.to_string(),
            retryable: self.retryable(),
            details,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use iwork_domain::id::{ConversationId, RawUlid};

    #[test]
    fn deleting_and_deleted_map_to_distinct_codes() {
        let id = ConversationId::from_raw(RawUlid::from_u128(1));
        assert_eq!(
            Refusal::from(DomainError::ConversationDeleting { id }).code(),
            pb::ErrorCode::ConversationDeleting
        );
        assert_eq!(
            Refusal::from(DomainError::ConversationDeleted { id }).code(),
            pb::ErrorCode::NotFound
        );
        assert!(Refusal::Closing.to_error().retryable);
        assert!(!Refusal::Unwired { what: "附件" }.to_error().retryable);
    }
}
