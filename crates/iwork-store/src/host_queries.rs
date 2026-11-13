//! 回合宿主与受理要读的几个查询（T10a 方案 2.2）。每个查询在调用方给的一个读事务里跑完。
//!
//! 两条绑定查询读的是 `events` 表里这个对话各回合的 `turn_session_bound` 事件（按 `events_by_stream`
//! 索引取），不读投影：投影上只有每个回合最后一个值，续接失败被退回的编号只在事件里。

use iwork_domain::approval_mode::ApprovalMode;
use iwork_domain::conversation::ConversationState;
use iwork_domain::event::{DomainEvent, StreamKind, TurnSessionBound};
use iwork_domain::id::{ConversationId, Seq};
use iwork_domain::turn::TurnState;
use iwork_domain::user_action::UserActionStateKind;
use iwork_ports::read_model::{ConversationWork, OpenTurn, QuitCounts, SnapshotInputs, TurnWork};
use iwork_ports::store::{EventCodec, StorageError};
use rusqlite::{Connection, OptionalExtension, params, params_from_iter};

use crate::projector::owner::owner_facts_on;
use crate::read_model::{blobs, kinds_where, rows};
use crate::snapshot::{
    decode_assistant, decode_conversation, decode_project, decode_rule, decode_turn,
};
use crate::sql::{ColumnText, enum_col, id_col, seq_col, sql_err};

/// 列里可空的 seq。
fn opt_seq(column: &str, v: Option<i64>) -> Result<Option<Seq>, StorageError> {
    v.map(|v| seq_col("turns", column, v)).transpose()
}

fn head(conn: &Connection) -> Result<Seq, StorageError> {
    let head: i64 = conn
        .query_row("SELECT COALESCE(MAX(seq), 0) FROM events", [], |r| r.get(0))
        .map_err(sql_err)?;
    seq_col("events", "seq", head)
}

pub(crate) fn turn_work<C: EventCodec + ?Sized>(
    conn: &Connection,
    codec: &C,
) -> Result<TurnWork, StorageError> {
    let head = head(conn)?;
    let (open_in, open_states) = kinds_where(|s: TurnState| !s.is_terminal());
    type Row = (
        String,
        Vec<u8>,
        Option<i64>,
        Option<i64>,
        Option<i64>,
        Option<i64>,
    );
    let open: Vec<Row> = rows(
        conn,
        &format!(
            "SELECT conversation_id, snapshot, run_started_seq, interrupted_seq, cancel_intent_seq, \
             resume_intent_seq FROM turns WHERE state IN ({open_in}) \
             ORDER BY conversation_id, created_seq"
        ),
        params_from_iter(&open_states),
        |r| {
            Ok((
                r.get(0)?,
                r.get(1)?,
                r.get(2)?,
                r.get(3)?,
                r.get(4)?,
                r.get(5)?,
            ))
        },
    )?;
    let deleting: Vec<String> = rows(
        conn,
        "SELECT id FROM conversations WHERE state = ?1",
        [ConversationState::Deleting.to_text()],
        |r| r.get(0),
    )?;
    // 对话 id → 它的没到终态的回合（已按 created_seq 排好）
    let mut by_conversation: std::collections::BTreeMap<String, Vec<OpenTurn>> =
        deleting.into_iter().map(|c| (c, Vec::new())).collect();
    for (conversation, snapshot, run, interrupted, cancel, resume) in open {
        let turn = decode_turn(codec, &snapshot)?;
        let owner = owner_facts_on(conn, codec, turn.id)?;
        by_conversation
            .entry(conversation)
            .or_default()
            .push(OpenTurn {
                turn,
                owner,
                run_started_seq: opt_seq("run_started_seq", run)?,
                interrupted_seq: opt_seq("interrupted_seq", interrupted)?,
                cancel_intent_seq: opt_seq("cancel_intent_seq", cancel)?,
                resume_intent_seq: opt_seq("resume_intent_seq", resume)?,
            });
    }
    let (card_in, card_kinds) = kinds_where(|k: UserActionStateKind| !k.is_terminal());
    let mut conversations = Vec::with_capacity(by_conversation.len());
    for (id, open_turns) in by_conversation {
        let snapshot: Option<Vec<u8>> = conn
            .prepare_cached("SELECT snapshot FROM conversations WHERE id = ?1")
            .and_then(|mut s| s.query_row([&id], |r| r.get(0)).optional())
            .map_err(sql_err)?;
        let Some(snapshot) = snapshot else {
            // 回合在、对话没了：级联删除不会留下这种行（X5），是投影坏了
            return Err(crate::sql::bad_column(
                "turns",
                "conversation_id",
                format!("回合指向的对话 {id} 不在"),
            ));
        };
        let mut args: Vec<&str> = vec![&id];
        args.extend(card_kinds.iter().copied());
        let cards_open: bool = conn
            .prepare_cached(&format!(
                "SELECT EXISTS (SELECT 1 FROM user_actions WHERE conversation_id = ? \
                 AND state_kind IN ({card_in}))"
            ))
            .and_then(|mut s| s.query_row(params_from_iter(&args), |r| r.get(0)))
            .map_err(sql_err)?;
        conversations.push(ConversationWork {
            conversation: decode_conversation(codec, &snapshot)?,
            open_turns,
            cards_open,
        });
    }
    Ok(TurnWork {
        head,
        conversations,
    })
}

/// 这个对话各回合的 `turn_session_bound` 事件，按 seq 升序。
fn session_bindings<C: EventCodec + ?Sized>(
    conn: &Connection,
    codec: &C,
    conversation: ConversationId,
) -> Result<Vec<TurnSessionBound>, StorageError> {
    let found: Vec<(String, Vec<u8>)> = rows(
        conn,
        "SELECT e.type, e.payload FROM turns t JOIN events e \
         ON e.stream_kind = ?1 AND e.stream_id = t.id AND e.type = 'turn_session_bound' \
         WHERE t.conversation_id = ?2 ORDER BY e.seq",
        params![StreamKind::Turn.to_text(), conversation.to_string()],
        |r| Ok((r.get(0)?, r.get(1)?)),
    )?;
    found
        .into_iter()
        .map(|(kind, payload)| match codec.decode(&kind, &payload)? {
            DomainEvent::TurnSessionBound(b) => Ok(b),
            other => Err(crate::sql::bad_column(
                "events",
                "type",
                format!("turn_session_bound 解出来是 {}", other.kind()),
            )),
        })
        .collect()
}

pub(crate) fn last_session_binding<C: EventCodec + ?Sized>(
    conn: &Connection,
    codec: &C,
    conversation: ConversationId,
) -> Result<Option<String>, StorageError> {
    Ok(session_bindings(conn, codec, conversation)?
        .pop()
        .map(|b| b.runtime_session_id))
}

pub(crate) fn session_ids_of<C: EventCodec + ?Sized>(
    conn: &Connection,
    codec: &C,
    conversation: ConversationId,
) -> Result<Vec<String>, StorageError> {
    let mut out: Vec<String> = Vec::new();
    for b in session_bindings(conn, codec, conversation)? {
        let ids = b
            .replaced
            .map(|r| r.old_runtime_session_id)
            .into_iter()
            .chain([b.runtime_session_id]);
        for id in ids {
            if !out.contains(&id) {
                out.push(id);
            }
        }
    }
    Ok(out)
}

pub(crate) fn quit_counts(conn: &Connection) -> Result<QuitCounts, StorageError> {
    let count = |keep: fn(TurnState) -> bool| -> Result<u32, StorageError> {
        let (in_, states) = kinds_where(keep);
        let n: i64 = conn
            .prepare_cached(&format!(
                "SELECT COUNT(*) FROM turns WHERE state IN ({in_})"
            ))
            .and_then(|mut s| s.query_row(params_from_iter(&states), |r| r.get(0)))
            .map_err(sql_err)?;
        Ok(u32::try_from(n).unwrap_or(u32::MAX))
    };
    Ok(QuitCounts {
        running_turns: count(TurnState::holds_process)?,
        queued_turns: count(|s| s == TurnState::Queued)?,
    })
}

pub(crate) fn snapshot_inputs<C: EventCodec + ?Sized>(
    conn: &Connection,
    codec: &C,
    conversation: ConversationId,
) -> Result<SnapshotInputs, StorageError> {
    let id = conversation.to_string();
    let snapshot: Vec<u8> = conn
        .prepare_cached("SELECT snapshot FROM conversations WHERE id = ?1")
        .and_then(|mut s| s.query_row([&id], |r| r.get(0)).optional())
        .map_err(sql_err)?
        .ok_or_else(|| StorageError::NotFound {
            entity: "conversation",
            id: id.clone(),
        })?;
    let member_modes = rows(
        conn,
        "SELECT assistant_id, mode FROM conversation_member_modes WHERE conversation_id = ?1 \
         ORDER BY assistant_id",
        [&id],
        |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)),
    )?
    .into_iter()
    .map(|(a, m)| {
        const T: &str = "conversation_member_modes";
        Ok((
            id_col(T, "assistant_id", &a)?,
            enum_col::<ApprovalMode>(T, "mode", &m)?,
        ))
    })
    .collect::<Result<_, StorageError>>()?;
    let projects = blobs(conn, "SELECT snapshot FROM projects ORDER BY id", [])?
        .iter()
        .map(|s| decode_project(codec, s))
        .collect::<Result<_, _>>()?;
    let assistants = blobs(conn, "SELECT snapshot FROM assistants ORDER BY id", [])?
        .iter()
        .map(|s| decode_assistant(codec, s))
        .collect::<Result<_, _>>()?;
    let security_lists = blobs(conn, crate::read_model::SECURITY_LISTS, [])?
        .iter()
        .map(|s| decode_rule(codec, s))
        .collect::<Result<_, _>>()?;
    let lists_seq: i64 = conn
        .query_row(
            "SELECT COALESCE(MAX(seq), 0) FROM events WHERE stream_kind = ?1",
            [StreamKind::Rule.to_text()],
            |r| r.get(0),
        )
        .map_err(sql_err)?;
    Ok(SnapshotInputs {
        conversation: decode_conversation(codec, &snapshot)?,
        member_modes,
        projects,
        assistants,
        security_lists,
        lists_seq: seq_col("events", "seq", lists_seq)?,
    })
}

/// 回合与它所在的对话（受理回合命令用）。回合在、对话不在是投影坏了（级联删除不留这种行，X5）。
pub(crate) fn turn_with_conversation<C: EventCodec + ?Sized>(
    conn: &Connection,
    codec: &C,
    turn: iwork_domain::id::TurnId,
) -> Result<
    Option<(
        iwork_domain::turn::Turn,
        iwork_domain::conversation::Conversation,
    )>,
    StorageError,
> {
    let key = turn.to_string();
    let row: Option<(Vec<u8>, Option<Vec<u8>>)> = conn
        .prepare_cached(
            "SELECT t.snapshot, c.snapshot FROM turns t \
             LEFT JOIN conversations c ON c.id = t.conversation_id WHERE t.id = ?1",
        )
        .and_then(|mut s| {
            s.query_row([&key], |r| Ok((r.get(0)?, r.get(1)?)))
                .optional()
        })
        .map_err(sql_err)?;
    let Some((turn_snapshot, conversation_snapshot)) = row else {
        return Ok(None);
    };
    let Some(conversation_snapshot) = conversation_snapshot else {
        return Err(crate::sql::bad_column(
            "turns",
            "conversation_id",
            format!("回合 {key} 指向的对话不在"),
        ));
    };
    Ok(Some((
        decode_turn(codec, &turn_snapshot)?,
        decode_conversation(codec, &conversation_snapshot)?,
    )))
}
