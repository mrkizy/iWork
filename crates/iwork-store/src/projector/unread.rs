//! 未读计数（设计 9.1；方案 2.7、Q4 (a)）。
//!
//! 只有两类事件计数：发给用户的消息（作者不是用户），与待处理卡片——审批请求、亲手执行卡片、
//! 回合中断后的「继续」卡片。AGENTS.md 草稿属于项目不属于对话，不计入。
//! [`counted_in`] 就是这张登记表；`ConversationRead` 时按同样的四个来源重算。

use iwork_domain::conversation::check_read_committed;
use iwork_domain::event::{ConversationRead, DomainEvent};
use iwork_domain::message::Author;
use iwork_domain::turn::TurnState;
use iwork_ports::store::{EventCodec, StorageError};
use rusqlite::{OptionalExtension, params};

use super::owner::precondition;
use super::{Cx, conversations};
use crate::sql::sql_err;

/// 这条事件是否计入某个对话的未读；计入时返回对话 ID 的文本。
fn counted_in<C: EventCodec + ?Sized>(
    cx: &Cx<'_, C>,
    event: &DomainEvent,
) -> Result<Option<String>, StorageError> {
    Ok(match event {
        DomainEvent::MessageAppended(e) if e.message.author != Author::User => {
            Some(e.message.conversation_id.to_string())
        }
        DomainEvent::ApprovalRequested(e) => Some(e.approval.conversation_id.to_string()),
        DomainEvent::UserActionProposed(e) => Some(e.card.conversation_id.to_string()),
        DomainEvent::TurnStateChanged(e) if matches!(e.to, TurnState::Interrupted { .. }) => cx
            .conn
            .query_row(
                "SELECT conversation_id FROM turns WHERE id = ?1",
                [e.turn_id.to_string()],
                |r| r.get(0),
            )
            .optional()
            .map_err(sql_err)?,
        _ => None,
    })
}

/// 计入的事件且 `seq > last_read_seq` 时未读加一。
pub(super) fn bump<C: EventCodec + ?Sized>(
    cx: &Cx<'_, C>,
    event: &DomainEvent,
) -> Result<(), StorageError> {
    if let Some(conversation_id) = counted_in(cx, event)? {
        cx.exec(
            "UPDATE conversations SET unread = unread + 1, updated_seq = ?1 \
             WHERE id = ?2 AND last_read_seq < ?1",
            params![cx.seq(), conversation_id],
        )?;
    }
    Ok(())
}

/// 已读到 `last_read_seq`：先过状态前提（只往前走，T10a 1/F-2），写已读位置，按四个来源重算未读。
pub(super) fn conversation_read<C: EventCodec + ?Sized>(
    cx: &Cx<'_, C>,
    e: &ConversationRead,
) -> Result<(), StorageError> {
    let id = e.conversation_id.to_string();
    conversations::modify(cx, &id, false, |c| {
        precondition(
            cx,
            &format!("对话 {id}"),
            check_read_committed(c.last_read_seq, e),
        )?;
        c.last_read_seq = e.last_read_seq;
        Ok(())
    })?;
    cx.exec(
        "UPDATE conversations SET unread = \
           (SELECT COUNT(*) FROM messages WHERE conversation_id = ?1 AND appended_seq > ?2 AND author_kind <> 'user') \
         + (SELECT COUNT(*) FROM approvals WHERE conversation_id = ?1 AND requested_seq > ?2) \
         + (SELECT COUNT(*) FROM user_actions WHERE conversation_id = ?1 AND proposed_seq > ?2) \
         + (SELECT COUNT(*) FROM turns WHERE conversation_id = ?1 AND interrupted_seq > ?2), \
         updated_seq = ?3 WHERE id = ?1",
        params![id, cx.seq_arg("last_read_seq", e.last_read_seq)?, cx.seq()],
    )?;
    Ok(())
}
