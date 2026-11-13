//! 对话命令的受理计划（T10a 方案 2.6）：纯函数，输入读到的状态，输出要提交的一笔事件或一个点名的拒绝。

use iwork_domain::common::{ModelSelection, Timestamp};
use iwork_domain::conversation::{Conversation, ConversationKind, ConversationState};
use iwork_domain::event::{
    ConversationCreated, ConversationDeleting, ConversationModelChanged, ConversationRead,
    DomainEvent,
};
use iwork_domain::id::{ConversationId, Seq};
use iwork_domain::project::Project;

use crate::admission::{CleanupKind, DELETING_ALLOWED};
use crate::refusal::Refusal;

/// `DeleteConversation`（X1）：`Active → Deleting`；已在删返回空的一笔（重发幂等，`DELETING_ALLOWED`）。
/// 作废待审批与待执行卡片由引擎在同一笔里附带（`Scope::ConversationDeleting`）。
pub fn plan_delete_conversation(conversation: &Conversation) -> Result<Vec<DomainEvent>, Refusal> {
    debug_assert!(
        DELETING_ALLOWED
            .iter()
            .any(|(_, k)| *k == CleanupKind::DeleteConversation),
        "删除在删除期间受理，登记表里要有它"
    );
    let mut c = conversation.clone();
    match c.state {
        ConversationState::Deleting => Ok(Vec::new()),
        ConversationState::Deleted => {
            Err(iwork_domain::DomainError::ConversationDeleted { id: c.id }.into())
        }
        ConversationState::Active => {
            c.request_delete()?;
            Ok(vec![
                ConversationDeleting {
                    conversation_id: c.id,
                }
                .into(),
            ])
        }
    }
}

/// `CreateConversation`：在项目里新开一个项目对话。
pub fn plan_create_conversation(
    project: Option<&Project>,
    project_id: iwork_domain::id::ProjectId,
    id: ConversationId,
    now: Timestamp,
) -> Result<Vec<DomainEvent>, Refusal> {
    let project = project.ok_or_else(|| Refusal::NotFound {
        entity: "project",
        id: project_id.to_string(),
    })?;
    Ok(vec![
        ConversationCreated {
            conversation: Conversation {
                id,
                kind: ConversationKind::Project {
                    project_id: project.id,
                },
                state: ConversationState::Active,
                last_read_seq: Seq(0),
                model_override: None,
                approval_mode: None,
                updated_at: now,
            },
        }
        .into(),
    ])
}

/// `MarkRead`：`up_to_seq = 0` 表示读到此刻；超过 head 的截到 head。不往回拨，没前进就是空的一笔。
pub fn plan_mark_read(
    conversation: &Conversation,
    up_to_seq: u64,
    head: Seq,
) -> Result<Vec<DomainEvent>, Refusal> {
    conversation.admit_command()?;
    let up_to = if up_to_seq == 0 {
        head
    } else {
        Seq(up_to_seq.min(head.0))
    };
    if up_to <= conversation.last_read_seq {
        return Ok(Vec::new());
    }
    Ok(vec![
        ConversationRead {
            conversation_id: conversation.id,
            last_read_seq: up_to,
        }
        .into(),
    ])
}

/// `SetConversationModel`：下一轮生效（技术设计 9.6 第 3 条）。
pub fn plan_set_conversation_model(
    conversation: &Conversation,
    model: ModelSelection,
) -> Result<Vec<DomainEvent>, Refusal> {
    conversation.admit_command()?;
    if model.model_id.0.trim().is_empty() {
        return Err(Refusal::InvalidArgument {
            field: "SetConversationModel.model.model_id",
            reason: "模型是空的".into(),
        });
    }
    Ok(vec![
        ConversationModelChanged {
            conversation_id: conversation.id,
            model,
        }
        .into(),
    ])
}
