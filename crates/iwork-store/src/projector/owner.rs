//! 提交时校验（方案 2.6「状态前提」「归属存续」；T10a 方案 2.2 的三处）：投影器只负责取事实，判断在
//! `iwork-domain`。
//!
//! 归属事实按主键读三行：回合（`turns`，`TurnFinished` 不删、随对话或助手的私聊级联删除）、回合所在的
//! 对话（`conversations`，状态列 `deleting` 是删除中）、回合快照里的助手（`assistants`）。

use iwork_domain::approval::{
    ConversationPresence, OwnerFacts, PreconditionFailed, Presence, owner_alive,
};
use iwork_domain::conversation::check_cascade;
use iwork_domain::event::DomainEvent;
use iwork_domain::id::{ConversationId, TurnId};
use iwork_domain::turn::TurnState;
use iwork_ports::store::{EventCodec, StorageError};
use rusqlite::OptionalExtension;

use super::Cx;
use crate::snapshot::decode_turn;
use crate::sql::{enum_col, sql_err};

/// 一个回合的归属事实。`turn` 没有记录时后两样填 `Gone`（`owner_alive` 不看它们）。
pub(crate) fn owner_facts<C: EventCodec + ?Sized>(
    cx: &Cx<'_, C>,
    turn: TurnId,
) -> Result<OwnerFacts, StorageError> {
    owner_facts_on(cx.conn, cx.codec, turn)
}

/// 同上，只要连接与 codec：写线程在事务里用，测试在只读连接上用同一份读法。
pub(crate) fn owner_facts_on<C: EventCodec + ?Sized>(
    conn: &rusqlite::Connection,
    codec: &C,
    turn: TurnId,
) -> Result<OwnerFacts, StorageError> {
    let row: Option<(String, Vec<u8>)> = conn
        .prepare_cached("SELECT conversation_id, snapshot FROM turns WHERE id = ?1")
        .and_then(|mut s| {
            s.query_row([turn.to_string()], |r| Ok((r.get(0)?, r.get(1)?)))
                .optional()
        })
        .map_err(sql_err)?;
    let Some((conversation_id, snapshot)) = row else {
        return Ok(OwnerFacts {
            turn: Presence::Gone,
            conversation: ConversationPresence::Gone,
            assistant: Presence::Gone,
        });
    };
    let assistant_id = decode_turn(codec, &snapshot)?.assistant_id;
    facts_of_owners(conn, &conversation_id, &assistant_id.to_string())
}

/// 回合有记录时（或排新回合时回合由事件带来），它所在的对话与它的助手在不在。
fn facts_of_owners(
    conn: &rusqlite::Connection,
    conversation_id: &str,
    assistant_id: &str,
) -> Result<OwnerFacts, StorageError> {
    let state: Option<String> = conn
        .prepare_cached("SELECT state FROM conversations WHERE id = ?1")
        .and_then(|mut s| s.query_row([conversation_id], |r| r.get(0)).optional())
        .map_err(sql_err)?;
    let conversation = match state.as_deref() {
        None => ConversationPresence::Gone,
        Some("deleting") => ConversationPresence::Deleting,
        Some(_) => ConversationPresence::Present,
    };
    let assistant: Option<i64> = conn
        .prepare_cached("SELECT 1 FROM assistants WHERE id = ?1")
        .and_then(|mut s| s.query_row([assistant_id], |r| r.get(0)).optional())
        .map_err(sql_err)?;
    Ok(OwnerFacts {
        turn: Presence::Present,
        conversation,
        assistant: if assistant.is_some() {
            Presence::Present
        } else {
            Presence::Gone
        },
    })
}

/// 要求这条事件的归属还在（domain 的 `requires_owner` 说要查时）。不通过 → `Projection`，点名缺的那样。
pub(crate) fn require_owner<C: EventCodec + ?Sized>(
    cx: &Cx<'_, C>,
    event: &DomainEvent,
    what: &str,
    turn: TurnId,
) -> Result<(), StorageError> {
    if !iwork_domain::approval::requires_owner(event) {
        return Ok(());
    }
    let facts = match event {
        // 新回合还没有行：事件带着它，「回合有记录」按在算
        DomainEvent::TurnQueued(e) => facts_of_owners(
            cx.conn,
            &e.turn.conversation_id.to_string(),
            &e.turn.assistant_id.to_string(),
        )?,
        _ => owner_facts(cx, turn)?,
    };
    owner_alive(&facts).map_err(|gone| {
        cx.invalid(format!(
            "{what} 的归属不在了（回合 {turn}）：{gone}，{} 被拒",
            event.kind()
        ))
    })
}

/// 状态前提不通过 → `Projection`，点名实体。
pub(crate) fn precondition<C: EventCodec + ?Sized>(
    cx: &Cx<'_, C>,
    what: &str,
    result: Result<(), PreconditionFailed>,
) -> Result<(), StorageError> {
    result.map_err(|e| cx.invalid(format!("{what}：{e}")))
}

/// 级联删除前名下回合都到终态（X5）。`conversation_ids` 是这笔要级联删掉的对话。
pub(crate) fn require_cascade<C: EventCodec + ?Sized>(
    cx: &Cx<'_, C>,
    conversation_ids: &[String],
) -> Result<(), StorageError> {
    for c in conversation_ids {
        let conversation: ConversationId = c
            .parse()
            .map_err(|e| cx.invalid(format!("conversations 里的对话编号 {c} 解析不了：{e}")))?;
        let turns: Vec<(TurnId, TurnState)> = cx
            .conn
            .prepare_cached("SELECT id, state FROM turns WHERE conversation_id = ?1")
            .and_then(|mut s| {
                s.query_map([c], |r| {
                    Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?))
                })?
                .collect::<Result<Vec<_>, _>>()
            })
            .map_err(sql_err)?
            .into_iter()
            .map(|(id, state)| -> Result<_, StorageError> {
                Ok((
                    id.parse().map_err(|e| {
                        cx.invalid(format!("turns 里的回合编号 {id} 解析不了：{e}"))
                    })?,
                    enum_col("turns", "state", &state)?,
                ))
            })
            .collect::<Result<_, _>>()?;
        precondition(cx, "级联删除", check_cascade(conversation, &turns))?;
    }
    Ok(())
}
