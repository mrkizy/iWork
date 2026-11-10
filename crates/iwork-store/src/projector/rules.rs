//! 审批规则与安全清单条目（都是 `RuleEntry`，同一张表 `approval_rules`）。

use iwork_domain::id::RuleId;
use iwork_domain::rule::{RuleEntry, RuleScope};
use iwork_ports::store::{EventCodec, StorageError};
use rusqlite::params;

use super::Cx;
use crate::snapshot::encode_rule;
use crate::sql::ColumnText;

/// `(scope_kind, scope_id)` 两列。
pub(crate) fn scope_columns(scope: &RuleScope) -> (&'static str, Option<String>) {
    match scope {
        RuleScope::Global => ("global", None),
        RuleScope::Project(id) => ("project", Some(id.to_string())),
        RuleScope::Assistant(id) => ("assistant", Some(id.to_string())),
        RuleScope::Conversation(id) => ("conversation", Some(id.to_string())),
    }
}

/// `RuleRemembered` 与 `SecurityListEntryAdded` 都插一行；快照一律用 `RuleRemembered{rule}` 的编码。
pub(super) fn added<C: EventCodec + ?Sized>(
    cx: &Cx<'_, C>,
    rule: &RuleEntry,
) -> Result<(), StorageError> {
    let (scope_kind, scope_id) = scope_columns(&rule.scope);
    cx.exec(
        "INSERT INTO approval_rules(id, layer, scope_kind, scope_id, created_at, snapshot, updated_seq) \
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
        params![
            rule.id.to_string(),
            rule.layer.to_text(),
            scope_kind,
            scope_id,
            rule.created_at.0,
            encode_rule(cx.codec, rule)?,
            cx.seq()
        ],
    )?;
    Ok(())
}

/// 撤销、删清单条目、批量删除：目标不存在是 app 层错误（方案 2.6「缺行报错」）。
pub(super) fn removed<C: EventCodec + ?Sized>(
    cx: &Cx<'_, C>,
    id: RuleId,
) -> Result<(), StorageError> {
    let id = id.to_string();
    cx.exec_hit(
        "DELETE FROM approval_rules WHERE id = ?1",
        [&id],
        "approval_rules",
        &id,
    )
}
