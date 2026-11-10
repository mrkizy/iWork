//! 亲手执行卡片。快照即真值（proto `UserActionCard` 装得下全部状态，方案 2.6 核对表）。

use iwork_domain::event::{DomainEvent, UserActionProposed, UserActionStateChanged};
use iwork_domain::user_action::check_committed;
use iwork_ports::store::{EventCodec, StorageError};
use rusqlite::params;

use super::Cx;
use super::owner::{precondition, require_owner};
use crate::snapshot::{decode_card, encode_card};
use crate::sql::ColumnText;

pub(super) fn proposed<C: EventCodec + ?Sized>(
    cx: &Cx<'_, C>,
    e: &UserActionProposed,
    event: &DomainEvent,
) -> Result<(), StorageError> {
    let c = &e.card;
    require_owner(cx, event, &format!("卡片 {}", c.id), c.turn_id)?;
    cx.exec(
        "INSERT INTO user_actions(id, turn_id, conversation_id, state_kind, proposed_seq, snapshot, \
         updated_seq) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?5)",
        params![
            c.id.to_string(),
            c.turn_id.to_string(),
            c.conversation_id.to_string(),
            c.state.kind().to_text(),
            cx.seq(),
            encode_card(cx.codec, c)?
        ],
    )?;
    Ok(())
}

/// 直接写事件里的目标状态；带了输出就换成新的输出，没带保留原来的。
pub(super) fn state_changed<C: EventCodec + ?Sized>(
    cx: &Cx<'_, C>,
    e: &UserActionStateChanged,
    event: &DomainEvent,
) -> Result<(), StorageError> {
    let id = e.card_id.to_string();
    let mut c = decode_card(cx.codec, &cx.snapshot("user_actions", "id", &id)?)?;
    precondition(cx, &format!("卡片 {id}"), check_committed(&c.state, event))?;
    require_owner(cx, event, &format!("卡片 {id}"), c.turn_id)?;
    c.state = e.state.clone();
    if let Some(out) = e.output_artifact {
        c.output_artifact = Some(out);
    }
    cx.exec_hit(
        "UPDATE user_actions SET state_kind = ?1, snapshot = ?2, updated_seq = ?3 WHERE id = ?4",
        params![
            c.state.kind().to_text(),
            encode_card(cx.codec, &c)?,
            cx.seq(),
            id
        ],
        "user_actions",
        &id,
    )
}
