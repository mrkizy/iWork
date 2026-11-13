//! 回合、回合用量、工具调用。

use iwork_domain::common::Usage;
use iwork_domain::event::{
    DomainEvent, ToolCallFinished, ToolCallStarted, TurnFinished, TurnIntentRecorded,
    TurnModelReported, TurnQueued, TurnSessionBound, TurnStateChanged,
};
use iwork_domain::id::TurnId;
use iwork_domain::turn::{
    Turn, TurnIntentKind, TurnState, check_intent_committed, check_turn_committed,
};
use iwork_ports::store::{EventCodec, StorageError};
use rusqlite::{OptionalExtension, params};

use super::Cx;
use super::owner::{precondition, require_owner};
use crate::snapshot::{decode_turn, encode_turn};
use crate::sql::{ColumnText, sha_text, sql_err, u64_val};

/// 排新回合：先过归属存续（对话在、不在删，助手在；T10a A3），再记 `created_seq`。
pub(super) fn queued<C: EventCodec + ?Sized>(
    cx: &Cx<'_, C>,
    e: &TurnQueued,
    event: &DomainEvent,
) -> Result<(), StorageError> {
    let t = &e.turn;
    require_owner(cx, event, "新回合", t.id)?;
    cx.exec(
        "INSERT INTO turns(id, conversation_id, state, started_at, ended_at, interrupted_seq, \
         snapshot, updated_seq, created_seq, ended_seq) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?8, 0)",
        params![
            t.id.to_string(),
            t.conversation_id.to_string(),
            t.state.to_text(),
            t.started_at.0,
            t.ended_at.map(|a| a.0),
            matches!(t.state, TurnState::Interrupted { .. }).then(|| cx.seq()),
            encode_turn(cx.codec, t)?,
            cx.seq()
        ],
    )?;
    Ok(())
}

fn load<C: EventCodec + ?Sized>(cx: &Cx<'_, C>, id: &str) -> Result<Turn, StorageError> {
    decode_turn(cx.codec, &cx.snapshot("turns", "id", id)?)
}

fn store<C: EventCodec + ?Sized>(
    cx: &Cx<'_, C>,
    id: &str,
    t: &Turn,
    interrupted_seq: Option<i64>,
) -> Result<(), StorageError> {
    cx.exec_hit(
        "UPDATE turns SET state = ?1, ended_at = ?2, interrupted_seq = ?3, snapshot = ?4, \
         updated_seq = ?5 WHERE id = ?6",
        params![
            t.state.to_text(),
            t.ended_at.map(|a| a.0),
            interrupted_seq,
            encode_turn(cx.codec, t)?,
            cx.seq(),
            id
        ],
        "turns",
        id,
    )
}

/// 写事件里的目标状态。先过状态前提（T10a A4 ①：库里现在的状态等于 `from`，且是转换表里的边）。
/// 进入终态记 `ended_at`（信封时间，与 `Turn::apply` 一致）；进入中断记下这条事件的 seq（未读计数用、
/// 继续意图按它比先后），并清掉上一次中断的继续意图；离开中断清空。从 `Queued` 或已中断进 `Running`
/// 是一趟编排的起点，记 `run_started_seq`（取消意图按它比先后）。
pub(super) fn state_changed<C: EventCodec + ?Sized>(
    cx: &Cx<'_, C>,
    e: &TurnStateChanged,
) -> Result<(), StorageError> {
    let id = e.turn_id.to_string();
    let mut t = load(cx, &id)?;
    precondition(cx, &format!("回合 {id}"), check_turn_committed(t.state, e))?;
    t.state = e.to;
    if e.to.is_terminal() {
        t.ended_at = Some(cx.env.at);
    }
    let interrupted = matches!(e.to, TurnState::Interrupted { .. });
    store(cx, &id, &t, interrupted.then(|| cx.seq()))?;
    if interrupted {
        cx.exec(
            "UPDATE turns SET resume_intent_seq = NULL WHERE id = ?1",
            [&id],
        )?;
    }
    let run_starts = matches!(e.from, TurnState::Queued | TurnState::Interrupted { .. })
        && e.to == TurnState::Running;
    if run_starts {
        cx.exec(
            "UPDATE turns SET run_started_seq = ?1 WHERE id = ?2",
            params![cx.seq(), id],
        )?;
    }
    Ok(())
}

/// 回合结束：记用量与 `ended_seq`。
pub(super) fn finished<C: EventCodec + ?Sized>(
    cx: &Cx<'_, C>,
    e: &TurnFinished,
) -> Result<(), StorageError> {
    usage(cx, e.turn_id, &e.usage)?;
    let id = e.turn_id.to_string();
    cx.exec_hit(
        "UPDATE turns SET ended_seq = ?1, updated_seq = ?1 WHERE id = ?2",
        params![cx.seq(), id],
        "turns",
        &id,
    )
}

/// 回合意图：先过状态前提（T10a A4 ②③），再记下这条意图的 seq（每种只留最近一条）。
pub(super) fn intent_recorded<C: EventCodec + ?Sized>(
    cx: &Cx<'_, C>,
    e: &TurnIntentRecorded,
) -> Result<(), StorageError> {
    let id = e.turn_id.to_string();
    let t = load(cx, &id)?;
    precondition(
        cx,
        &format!("回合 {id}"),
        check_intent_committed(t.state, e.intent),
    )?;
    let sql = match e.intent {
        TurnIntentKind::Cancel => {
            "UPDATE turns SET cancel_intent_seq = ?1, updated_seq = ?1 WHERE id = ?2"
        }
        TurnIntentKind::Resume => {
            "UPDATE turns SET resume_intent_seq = ?1, updated_seq = ?1 WHERE id = ?2"
        }
    };
    cx.exec_hit(sql, params![cx.seq(), id], "turns", &id)
}

pub(super) fn model_reported<C: EventCodec + ?Sized>(
    cx: &Cx<'_, C>,
    e: &TurnModelReported,
) -> Result<(), StorageError> {
    update_snapshot(cx, e.turn_id, |t| t.model = e.model.clone())
}

/// 会话 ID 只在快照里：读模型返回的 `Turn` 从快照解出，没有单独的查询按它过滤。
pub(super) fn session_bound<C: EventCodec + ?Sized>(
    cx: &Cx<'_, C>,
    e: &TurnSessionBound,
) -> Result<(), StorageError> {
    update_snapshot(cx, e.turn_id, |t| {
        t.runtime_session_id = Some(e.runtime_session_id.clone())
    })
}

/// 只改快照里状态以外的字段：`state`、`ended_at`、`interrupted_seq` 归 `TurnStateChanged`，原样保留。
fn update_snapshot<C: EventCodec + ?Sized>(
    cx: &Cx<'_, C>,
    turn_id: TurnId,
    change: impl FnOnce(&mut Turn),
) -> Result<(), StorageError> {
    let id = turn_id.to_string();
    let mut t = load(cx, &id)?;
    change(&mut t);
    let interrupted_seq: Option<i64> = cx
        .conn
        .query_row(
            "SELECT interrupted_seq FROM turns WHERE id = ?1",
            [&id],
            |r| r.get(0),
        )
        .map_err(sql_err)?;
    store(cx, &id, &t, interrupted_seq)
}

/// `TurnFinished` 与 `UsageReported` 都可能带用量，写同一组列，以后到的为准。
pub(super) fn usage<C: EventCodec + ?Sized>(
    cx: &Cx<'_, C>,
    turn_id: TurnId,
    u: &Usage,
) -> Result<(), StorageError> {
    let id = turn_id.to_string();
    let conversation_id: String = cx
        .conn
        .query_row(
            "SELECT conversation_id FROM turns WHERE id = ?1",
            [&id],
            |r| r.get(0),
        )
        .optional()
        .map_err(sql_err)?
        .ok_or_else(|| cx.missing("turns", &id))?;
    cx.exec(
        "INSERT INTO turn_usage(turn_id, conversation_id, input_tokens, output_tokens, \
         cache_read_tokens, cache_write_tokens, cost_usd, updated_seq) \
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8) ON CONFLICT(turn_id) DO UPDATE SET \
         input_tokens = excluded.input_tokens, output_tokens = excluded.output_tokens, \
         cache_read_tokens = excluded.cache_read_tokens, cache_write_tokens = excluded.cache_write_tokens, \
         cost_usd = excluded.cost_usd, updated_seq = excluded.updated_seq",
        params![
            id,
            conversation_id,
            u64_val("input_tokens", u.input_tokens)?,
            u64_val("output_tokens", u.output_tokens)?,
            u64_val("cache_read_tokens", u.cache_read_tokens)?,
            u64_val("cache_write_tokens", u.cache_write_tokens)?,
            u.cost_usd,
            cx.seq()
        ],
    )?;
    Ok(())
}

pub(super) fn tool_call_started<C: EventCodec + ?Sized>(
    cx: &Cx<'_, C>,
    e: &ToolCallStarted,
) -> Result<(), StorageError> {
    let turn = e.turn_id.to_string();
    let conversation_id: String = cx
        .conn
        .prepare_cached("SELECT conversation_id FROM turns WHERE id = ?1")
        .and_then(|mut s| s.query_row([&turn], |r| r.get(0)).optional())
        .map_err(sql_err)?
        .ok_or_else(|| cx.missing("turns", &turn))?;
    cx.exec(
        "INSERT INTO tool_calls(turn_id, tool_call_id, tool_name, ok, output_sha256, output_area, \
         output_size, started_seq, updated_seq, conversation_id) \
         VALUES (?1, ?2, ?3, NULL, NULL, NULL, NULL, ?4, ?4, ?5)",
        params![
            turn,
            e.tool_call_id.0,
            e.tool_name,
            cx.seq(),
            conversation_id
        ],
    )?;
    Ok(())
}

pub(super) fn tool_call_finished<C: EventCodec + ?Sized>(
    cx: &Cx<'_, C>,
    e: &ToolCallFinished,
) -> Result<(), StorageError> {
    let a = e.output_artifact.as_ref();
    let key = format!("{}/{}", e.turn_id, e.tool_call_id.0);
    cx.exec_hit(
        "UPDATE tool_calls SET ok = ?1, output_sha256 = ?2, output_area = ?3, output_size = ?4, \
         updated_seq = ?5 WHERE turn_id = ?6 AND tool_call_id = ?7",
        params![
            e.ok,
            a.map(|a| sha_text(&a.sha256)),
            a.map(|a| a.area.to_text()),
            a.map(|a| u64_val("制品大小", a.size)).transpose()?,
            cx.seq(),
            e.turn_id.to_string(),
            e.tool_call_id.0
        ],
        "tool_calls",
        key,
    )
}
