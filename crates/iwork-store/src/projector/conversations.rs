//! 对话：创建、设置、删除与级联。审批档位三层的落点见方案 2.11。

use iwork_domain::approval_mode::{ApprovalMode, ProjectDefaultMode};
use iwork_domain::conversation::{Conversation, ConversationKind, ConversationState};
use iwork_domain::event::{
    ApprovalModeChanged, ApprovalModeTarget, ConversationCreated, ConversationDeleting,
    ConversationModelChanged, DmProjectSwitched,
};
use iwork_domain::id::ConversationId;
use iwork_ports::store::{EventCodec, StorageError};
use rusqlite::params;

use super::{Cx, projects};
use crate::snapshot::{decode_conversation, encode_conversation};
use crate::sql::ColumnText;

/// `(kind, project_id, assistant_id, context_project_id)` 四列。
fn kind_columns(
    kind: &ConversationKind,
) -> (&'static str, Option<String>, Option<String>, Option<String>) {
    match kind {
        ConversationKind::Project { project_id } => {
            ("project", Some(project_id.to_string()), None, None)
        }
        ConversationKind::AssistantDm {
            assistant_id,
            context_project,
        } => (
            "assistant_dm",
            None,
            Some(assistant_id.to_string()),
            context_project.map(|p| p.to_string()),
        ),
    }
}

pub(super) fn created<C: EventCodec + ?Sized>(
    cx: &Cx<'_, C>,
    e: &ConversationCreated,
) -> Result<(), StorageError> {
    let c = &e.conversation;
    let (kind, project_id, assistant_id, context_project_id) = kind_columns(&c.kind);
    cx.exec(
        "INSERT INTO conversations(id, kind, project_id, assistant_id, context_project_id, state, \
         last_read_seq, unread, approval_mode, updated_at, snapshot, updated_seq) \
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, 0, ?8, ?9, ?10, ?11)",
        params![
            c.id.to_string(),
            kind,
            project_id,
            assistant_id,
            context_project_id,
            c.state.to_text(),
            cx.seq_arg("last_read_seq", c.last_read_seq)?,
            c.approval_mode.map(ApprovalMode::to_text),
            c.updated_at.0,
            encode_conversation(cx.codec, c)?,
            cx.seq()
        ],
    )?;
    Ok(())
}

/// 读快照 → 改 → 写回快照推出的全部列（`unread` 归 `unread.rs`，不碰）。
///
/// `touch`：这次改动是否算「对话有新动静」，是则 `updated_at` 取信封时间（对话列表按它排序）。
/// 已读位置的变化不算。
pub(super) fn modify<C: EventCodec + ?Sized>(
    cx: &Cx<'_, C>,
    id: &str,
    touch: bool,
    change: impl FnOnce(&mut Conversation) -> Result<(), StorageError>,
) -> Result<(), StorageError> {
    let mut c = decode_conversation(cx.codec, &cx.snapshot("conversations", "id", id)?)?;
    change(&mut c)?;
    if touch {
        c.updated_at = cx.env.at;
    }
    let (kind, project_id, assistant_id, context_project_id) = kind_columns(&c.kind);
    cx.exec_hit(
        "UPDATE conversations SET kind = ?1, project_id = ?2, assistant_id = ?3, \
         context_project_id = ?4, state = ?5, last_read_seq = ?6, approval_mode = ?7, \
         updated_at = ?8, snapshot = ?9, updated_seq = ?10 WHERE id = ?11",
        params![
            kind,
            project_id,
            assistant_id,
            context_project_id,
            c.state.to_text(),
            cx.seq_arg("last_read_seq", c.last_read_seq)?,
            c.approval_mode.map(ApprovalMode::to_text),
            c.updated_at.0,
            encode_conversation(cx.codec, &c)?,
            cx.seq(),
            id
        ],
        "conversations",
        id,
    )
}

pub(super) fn deleting<C: EventCodec + ?Sized>(
    cx: &Cx<'_, C>,
    e: &ConversationDeleting,
) -> Result<(), StorageError> {
    modify(cx, &e.conversation_id.to_string(), true, |c| {
        c.state = ConversationState::Deleting;
        Ok(())
    })
}

pub(super) fn deleted<C: EventCodec + ?Sized>(
    cx: &Cx<'_, C>,
    id: ConversationId,
) -> Result<(), StorageError> {
    let id = id.to_string();
    super::owner::require_cascade(cx, std::slice::from_ref(&id))?;
    delete_cascade(cx, &id)
}

/// 删对话及挂在它下面的一切（方案 2.6「级联」）。调用方先过 `owner::require_cascade`（名下回合都到终态）：消息、回合、用量、工具调用、审批、亲手执行卡片、
/// 附件、成员档位、流式检查点，以及这些行贡献的制品引用。规则由 `RulesDeleted` 单独删。
pub(super) fn delete_cascade<C: EventCodec + ?Sized>(
    cx: &Cx<'_, C>,
    id: &str,
) -> Result<(), StorageError> {
    // 先删引用（要靠还没删的行找到 ref_id）
    for sql in [
        "DELETE FROM artifact_refs WHERE ref_kind = 'message' AND ref_id IN \
         (SELECT id FROM messages WHERE conversation_id = ?1)",
        "DELETE FROM artifact_refs WHERE ref_kind = 'tool_call' AND ref_id IN \
         (SELECT tc.turn_id || '/' || tc.tool_call_id FROM tool_calls tc \
          JOIN turns t ON t.id = tc.turn_id WHERE t.conversation_id = ?1)",
        "DELETE FROM artifact_refs WHERE ref_kind = 'user_action' AND ref_id IN \
         (SELECT id FROM user_actions WHERE conversation_id = ?1)",
        "DELETE FROM artifact_refs WHERE ref_kind = 'attachment' AND ref_id IN \
         (SELECT id FROM attachments WHERE conversation_id = ?1)",
        "DELETE FROM message_checkpoints WHERE message_id IN \
         (SELECT id FROM messages WHERE conversation_id = ?1)",
        "DELETE FROM tool_calls WHERE conversation_id = ?1",
        "DELETE FROM turn_usage WHERE conversation_id = ?1",
        "DELETE FROM turns WHERE conversation_id = ?1",
        "DELETE FROM messages WHERE conversation_id = ?1",
        "DELETE FROM approvals WHERE conversation_id = ?1",
        "DELETE FROM user_actions WHERE conversation_id = ?1",
        "DELETE FROM attachments WHERE conversation_id = ?1",
        "DELETE FROM conversation_member_modes WHERE conversation_id = ?1",
    ] {
        cx.exec(sql, [id])?;
    }
    cx.exec_hit(
        "DELETE FROM conversations WHERE id = ?1",
        [id],
        "conversations",
        id,
    )
}

pub(super) fn model_changed<C: EventCodec + ?Sized>(
    cx: &Cx<'_, C>,
    e: &ConversationModelChanged,
) -> Result<(), StorageError> {
    modify(cx, &e.conversation_id.to_string(), true, |c| {
        c.model_override = Some(e.model.clone());
        Ok(())
    })
}

/// 档位三层（卡「范围 2」、方案 2.11）：对话级写对话行；成员单独设置写 `conversation_member_modes`；
/// 项目默认写项目设置。
pub(super) fn approval_mode_changed<C: EventCodec + ?Sized>(
    cx: &Cx<'_, C>,
    e: &ApprovalModeChanged,
) -> Result<(), StorageError> {
    match e.target {
        ApprovalModeTarget::Conversation(id) => modify(cx, &id.to_string(), true, |c| {
            c.approval_mode = e.mode;
            Ok(())
        }),
        ApprovalModeTarget::Member {
            conversation_id,
            assistant_id,
        } => {
            let (conv, asst) = (conversation_id.to_string(), assistant_id.to_string());
            match e.mode {
                Some(mode) => cx.exec(
                    "INSERT INTO conversation_member_modes(conversation_id, assistant_id, mode, updated_seq) \
                     VALUES (?1, ?2, ?3, ?4) ON CONFLICT(conversation_id, assistant_id) \
                     DO UPDATE SET mode = excluded.mode, updated_seq = excluded.updated_seq",
                    params![conv, asst, mode.to_text(), cx.seq()],
                ),
                // 恢复跟随：没有单独设置的行就是跟随，删一条本来就没有的不算错
                None => cx.exec(
                    "DELETE FROM conversation_member_modes WHERE conversation_id = ?1 AND assistant_id = ?2",
                    params![conv, asst],
                ),
            }
            .map(|_| ())
        }
        ApprovalModeTarget::Project(id) => {
            // 项目默认只能手动或自动（需求 FR11）；app 层应在提交前拒绝，投影器是最后一道
            let mode = match e.mode {
                Some(ApprovalMode::Manual) => ProjectDefaultMode::Manual,
                Some(ApprovalMode::Auto) => ProjectDefaultMode::Auto,
                other => {
                    return Err(cx.invalid(format!(
                        "项目 {id} 的默认档位只能是手动或自动，事件给的是 {other:?}"
                    )));
                }
            };
            projects::set_default_mode(cx, id, mode)
        }
    }
}

/// 私聊切换所选项目：自己的档位覆盖一并清掉（T01 Q7 已定全清）。不是私聊 → `Projection`。
pub(super) fn dm_project_switched<C: EventCodec + ?Sized>(
    cx: &Cx<'_, C>,
    e: &DmProjectSwitched,
) -> Result<(), StorageError> {
    let id = e.conversation_id.to_string();
    modify(cx, &id, true, |c| match &mut c.kind {
        ConversationKind::AssistantDm {
            context_project, ..
        } => {
            *context_project = e.project_id;
            c.approval_mode = None;
            Ok(())
        }
        ConversationKind::Project { .. } => {
            Err(cx.invalid(format!("对话 {id} 不是助手私聊，不能切换项目")))
        }
    })
}
