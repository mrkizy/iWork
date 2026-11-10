//! 消息。流式正文检查点在非投影表 `message_checkpoints`，消息结束时删掉（设计 3.5）。

use iwork_domain::event::{MessageAppended, MessageCompleted};
use iwork_domain::message::Author;
use iwork_ports::store::{EventCodec, StorageError};
use rusqlite::params;

use super::{Cx, conversations};
use crate::snapshot::{decode_message, encode_message};
use crate::sql::ColumnText;

pub(crate) fn author_kind(author: &Author) -> &'static str {
    match author {
        Author::User => "user",
        Author::Assistant { .. } => "assistant",
        Author::System => "system",
    }
}

pub(super) fn appended<C: EventCodec + ?Sized>(
    cx: &Cx<'_, C>,
    e: &MessageAppended,
) -> Result<(), StorageError> {
    let m = &e.message;
    cx.exec(
        "INSERT INTO messages(id, conversation_id, author_kind, status, turn_id, appended_seq, \
         created_at, snapshot, updated_seq) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?6)",
        params![
            m.id.to_string(),
            m.conversation_id.to_string(),
            author_kind(&m.author),
            m.status.to_text(),
            m.turn_id.map(|t| t.to_string()),
            cx.seq(),
            m.created_at.0,
            encode_message(cx.codec, m)?
        ],
    )?;
    // 对话有了新消息：列表按 updated_at 排序
    conversations::modify(cx, &m.conversation_id.to_string(), true, |_| Ok(()))
}

pub(super) fn completed<C: EventCodec + ?Sized>(
    cx: &Cx<'_, C>,
    e: &MessageCompleted,
) -> Result<(), StorageError> {
    let id = e.message_id.to_string();
    let mut m = decode_message(cx.codec, &cx.snapshot("messages", "id", &id)?)?;
    m.body = e.body.clone();
    m.status = e.status;
    cx.exec_hit(
        "UPDATE messages SET status = ?1, snapshot = ?2, updated_seq = ?3 WHERE id = ?4",
        params![
            m.status.to_text(),
            encode_message(cx.codec, &m)?,
            cx.seq(),
            id
        ],
        "messages",
        &id,
    )?;
    cx.exec(
        "DELETE FROM message_checkpoints WHERE message_id = ?1",
        [&id],
    )?;
    Ok(())
}
