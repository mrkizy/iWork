//! 实体 ↔ 快照字节（方案 2.6、ADR-0008）。
//!
//! 快照借实体「创建事件」的编码：把实体包进创建事件交给 `EventCodec::encode`，解码时按同一个
//! `kind` 调 `decode` 再取出实体。store 不依赖 proto，也不另发明一套实体序列化。
//!
//! 创建事件装不下的后续状态另立列（核对表见方案 2.6）：目前只有审批的主状态（批准人、作废原因），
//! 快照里固定写 `Pending` 基底，真值在 `approvals` 的 `state / decider / void_reason` 三列，
//! 读出时由 [`approval_state_from_columns`] 叠加回来。

use iwork_domain::approval::{Approval, ApprovalState, Decider, VoidReason};
use iwork_domain::assistant::{Assistant, AssistantConfig};
use iwork_domain::attachment::Attachment;
use iwork_domain::conversation::Conversation;
use iwork_domain::event::{
    ApprovalRequested, AssistantCreated, AssistantUpdated, AttachmentStored, ConversationCreated,
    DomainEvent, MessageAppended, ProjectAdded, RuleRemembered, RuntimeUpdated, TurnQueued,
    UserActionProposed,
};
use iwork_domain::id::AssistantId;
use iwork_domain::message::Message;
use iwork_domain::project::Project;
use iwork_domain::rule::RuleEntry;
use iwork_domain::runtime::Runtime;
use iwork_domain::turn::Turn;
use iwork_domain::user_action::UserActionCard;
use iwork_ports::store::{EventCodec, StorageError};

use crate::sql::{ColumnText, bad_column};

/// 解出的事件不是借用的那一种：只可能是行被写坏了。
fn wrong_kind(expected: &str, got: &DomainEvent) -> StorageError {
    StorageError::Io {
        message: format!("快照应为 {expected}，解出的是 {}", got.kind()),
    }
}

/// 登记表：每种实体借用哪个创建事件、它的种类名。加一种实体 = 这里一行；
/// 种类名写错由单测 `snapshot_kinds_match_events` 抓出来。
macro_rules! snapshot {
    ($(#[$decode_meta:meta])* $encode:ident, $decode:ident, $entity:ty, $event:ident { $field:ident }, $kind:literal) => {
        pub(crate) fn $encode<C: EventCodec + ?Sized>(
            codec: &C,
            value: &$entity,
        ) -> Result<Vec<u8>, StorageError> {
            let event = DomainEvent::from($event {
                $field: value.clone(),
            });
            Ok(codec.encode(&event)?.payload)
        }

        $(#[$decode_meta])*
        pub(crate) fn $decode<C: EventCodec + ?Sized>(
            codec: &C,
            bytes: &[u8],
        ) -> Result<$entity, StorageError> {
            match codec.decode($kind, bytes)? {
                DomainEvent::$event(e) => Ok(e.$field),
                other => Err(wrong_kind($kind, &other)),
            }
        }
    };
}

snapshot!(
    encode_project,
    decode_project,
    Project,
    ProjectAdded { project },
    "project_added"
);
snapshot!(
    encode_assistant,
    decode_assistant,
    Assistant,
    AssistantCreated { assistant },
    "assistant_created"
);
snapshot!(
    encode_conversation,
    decode_conversation,
    Conversation,
    ConversationCreated { conversation },
    "conversation_created"
);
snapshot!(
    encode_message,
    decode_message,
    Message,
    MessageAppended { message },
    "message_appended"
);
snapshot!(
    encode_turn,
    decode_turn,
    Turn,
    TurnQueued { turn },
    "turn_queued"
);
snapshot!(
    encode_approval_raw,
    decode_approval_base,
    Approval,
    ApprovalRequested { approval },
    "approval_requested"
);
snapshot!(
    encode_card,
    decode_card,
    UserActionCard,
    UserActionProposed { card },
    "user_action_proposed"
);
snapshot!(
    encode_rule,
    decode_rule,
    RuleEntry,
    RuleRemembered { rule },
    "rule_remembered"
);
snapshot!(
    // 附件没有读查询（T10 需要时再加）；解码只在快照往返测试里用
    #[cfg_attr(not(test), expect(dead_code))]
    encode_attachment,
    decode_attachment,
    Attachment,
    AttachmentStored { attachment },
    "attachment_stored"
);
snapshot!(
    encode_runtime,
    decode_runtime,
    Runtime,
    RuntimeUpdated { runtime },
    "runtime_updated"
);

/// 助手的一个版本借 `AssistantUpdated{assistant_id, version, config}`；`created_at` 在列里。
pub(crate) fn encode_assistant_version<C: EventCodec + ?Sized>(
    codec: &C,
    assistant_id: AssistantId,
    version: u32,
    config: &AssistantConfig,
) -> Result<Vec<u8>, StorageError> {
    let event = DomainEvent::from(AssistantUpdated {
        assistant_id,
        version,
        config: config.clone(),
    });
    Ok(codec.encode(&event)?.payload)
}

#[cfg(test)]
pub(crate) fn decode_assistant_version<C: EventCodec + ?Sized>(
    codec: &C,
    bytes: &[u8],
) -> Result<AssistantUpdated, StorageError> {
    match codec.decode("assistant_updated", bytes)? {
        DomainEvent::AssistantUpdated(e) => Ok(e),
        other => Err(wrong_kind("assistant_updated", &other)),
    }
}

/// 审批快照：主状态固定写 `Pending` 基底（proto `Approval` 装不下批准人与作废原因）。
pub(crate) fn encode_approval<C: EventCodec + ?Sized>(
    codec: &C,
    approval: &Approval,
) -> Result<Vec<u8>, StorageError> {
    let mut base = approval.clone();
    base.state = ApprovalState::Pending;
    encode_approval_raw(codec, &base)
}

/// `approvals.state` 列的取值。
pub(crate) fn approval_state_text(state: ApprovalState) -> &'static str {
    match state {
        ApprovalState::Pending => "pending",
        ApprovalState::Approved { .. } => "approved",
        ApprovalState::Denied => "denied",
        ApprovalState::Voided { .. } => "voided",
    }
}

/// `approvals` 的 `state / decider / void_reason` 三列 → 主状态。组合不合法时点名行。
pub(crate) fn approval_state_from_columns(
    id: &str,
    state: &str,
    decider: Option<&str>,
    void_reason: Option<&str>,
) -> Result<ApprovalState, StorageError> {
    let bad = || {
        bad_column(
            "approvals",
            "state/decider/void_reason",
            format!("{id}: {state} {decider:?} {void_reason:?}"),
        )
    };
    match (state, decider, void_reason) {
        ("pending", None, None) => Ok(ApprovalState::Pending),
        ("denied", None, None) => Ok(ApprovalState::Denied),
        ("approved", Some(d), None) => Ok(ApprovalState::Approved {
            decider: Decider::from_text(d).ok_or_else(bad)?,
        }),
        ("voided", None, Some(r)) => Ok(ApprovalState::Voided {
            reason: VoidReason::from_text(r).ok_or_else(bad)?,
        }),
        _ => Err(bad()),
    }
}

/// 主状态 → `decider`、`void_reason` 两列。
pub(crate) fn approval_state_columns(
    state: ApprovalState,
) -> (Option<&'static str>, Option<&'static str>) {
    match state {
        ApprovalState::Approved { decider } => (Some(decider.to_text()), None),
        ApprovalState::Voided { reason } => (None, Some(reason.to_text())),
        ApprovalState::Pending | ApprovalState::Denied => (None, None),
    }
}

/// 读出一行审批：快照基底 + 三列主状态。
pub(crate) fn decode_approval<C: EventCodec + ?Sized>(
    codec: &C,
    bytes: &[u8],
    state: ApprovalState,
) -> Result<Approval, StorageError> {
    let mut approval = decode_approval_base(codec, bytes)?;
    approval.state = state;
    Ok(approval)
}
