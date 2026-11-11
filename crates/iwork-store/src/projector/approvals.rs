//! 审批。主状态的真值在 `state / decider / void_reason` 三列，快照只存 `Pending` 基底
//! （方案 2.6 快照覆盖范围核对表）。`awaits_user` 每次都由 domain 的 `Approval::awaits_user` 算。

use iwork_domain::approval::check_committed;
use iwork_domain::approval::{Approval, ApprovalState};
use iwork_domain::event::{ApprovalRequested, ApprovalReviewUpdated, ApprovalVoided, DomainEvent};
use iwork_domain::id::ApprovalId;
use iwork_ports::store::{EventCodec, StorageError};
use rusqlite::{OptionalExtension, params};

use super::Cx;
use super::owner::{precondition, require_owner};
use crate::snapshot::{
    approval_state_columns, approval_state_from_columns, approval_state_text, decode_approval,
    encode_approval,
};
use crate::sql::{ColumnText, sql_err};

pub(super) fn requested<C: EventCodec + ?Sized>(
    cx: &Cx<'_, C>,
    e: &ApprovalRequested,
    event: &DomainEvent,
) -> Result<(), StorageError> {
    let a = &e.approval;
    require_owner(cx, event, &format!("审批 {}", a.id), a.turn_id)?;
    let (decider, void_reason) = approval_state_columns(a.state);
    cx.exec(
        "INSERT INTO approvals(id, turn_id, conversation_id, state, decider, void_reason, review_phase, \
         awaits_user, requested_seq, requested_at, decided_at, snapshot, updated_seq) \
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, NULL, ?11, ?9)",
        params![
            a.id.to_string(),
            a.turn_id.to_string(),
            a.conversation_id.to_string(),
            approval_state_text(a.state),
            decider,
            void_reason,
            a.review.phase().to_text(),
            a.awaits_user(),
            cx.seq(),
            a.requested_at.0,
            encode_approval(cx.codec, a)?
        ],
    )?;
    Ok(())
}

/// 读出一行：快照基底叠加三列主状态。
fn load<C: EventCodec + ?Sized>(cx: &Cx<'_, C>, id: &str) -> Result<Approval, StorageError> {
    type Row = (Vec<u8>, String, Option<String>, Option<String>);
    let row: Option<Row> = cx
        .conn
        .prepare_cached("SELECT snapshot, state, decider, void_reason FROM approvals WHERE id = ?1")
        .and_then(|mut s| {
            s.query_row([id], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)))
                .optional()
        })
        .map_err(sql_err)?;
    let (snapshot, state, decider, void_reason) = row.ok_or_else(|| cx.missing("approvals", id))?;
    let state =
        approval_state_from_columns(id, &state, decider.as_deref(), void_reason.as_deref())?;
    decode_approval(cx.codec, &snapshot, state)
}

pub(super) fn review_updated<C: EventCodec + ?Sized>(
    cx: &Cx<'_, C>,
    e: &ApprovalReviewUpdated,
    event: &DomainEvent,
) -> Result<(), StorageError> {
    let id = e.approval_id.to_string();
    let mut a = load(cx, &id)?;
    precondition(
        cx,
        &format!("审批 {id}"),
        check_committed(a.state, &a.review, event),
    )?;
    a.review = e.review.clone();
    cx.exec_hit(
        "UPDATE approvals SET review_phase = ?1, awaits_user = ?2, snapshot = ?3, updated_seq = ?4 \
         WHERE id = ?5",
        params![
            a.review.phase().to_text(),
            a.awaits_user(),
            encode_approval(cx.codec, &a)?,
            cx.seq(),
            id
        ],
        "approvals",
        &id,
    )
}

/// `ApprovalDecided` / `ApprovalVoided` 共用：写主状态三列、`awaits_user` 与决定时间（信封 `at`）。
/// 快照不动（仍是 `Pending` 基底）。
pub(super) fn decided<C: EventCodec + ?Sized>(
    cx: &Cx<'_, C>,
    approval_id: ApprovalId,
    state: ApprovalState,
    event: &DomainEvent,
) -> Result<(), StorageError> {
    let id = approval_id.to_string();
    let mut a = load(cx, &id)?;
    precondition(
        cx,
        &format!("审批 {id}"),
        check_committed(a.state, &a.review, event),
    )?;
    require_owner(cx, event, &format!("审批 {id}"), a.turn_id)?;
    a.state = state;
    let (decider, void_reason) = approval_state_columns(state);
    cx.exec_hit(
        "UPDATE approvals SET state = ?1, decider = ?2, void_reason = ?3, awaits_user = ?4, \
         decided_at = ?5, updated_seq = ?6 WHERE id = ?7",
        params![
            approval_state_text(state),
            decider,
            void_reason,
            a.awaits_user(),
            cx.env.at.0,
            cx.seq(),
            id
        ],
        "approvals",
        &id,
    )
}

pub(super) fn voided<C: EventCodec + ?Sized>(
    cx: &Cx<'_, C>,
    e: &ApprovalVoided,
    event: &DomainEvent,
) -> Result<(), StorageError> {
    decided(
        cx,
        e.approval_id,
        ApprovalState::Voided { reason: e.reason },
        event,
    )
}
