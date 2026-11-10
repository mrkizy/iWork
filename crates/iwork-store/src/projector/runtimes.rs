//! 运行时、额度、本机配置导入。

use iwork_domain::event::{
    LocalConfigImported, LocalImportPrompted, QuotaObserved, RuntimeUpdated,
};
use iwork_ports::store::{EventCodec, StorageError};
use rusqlite::params;

use super::Cx;
use crate::snapshot::encode_runtime;
use crate::sql::ColumnText;

/// `RuntimeUpdated` 拥有 `kind / state / snapshot`，不碰 `import_prompted`（那是 `LocalImportPrompted` 的）。
pub(super) fn updated<C: EventCodec + ?Sized>(
    cx: &Cx<'_, C>,
    e: &RuntimeUpdated,
) -> Result<(), StorageError> {
    let r = &e.runtime;
    cx.exec(
        "INSERT INTO runtimes(id, kind, state, import_prompted, snapshot, updated_seq) \
         VALUES (?1, ?2, ?3, 0, ?4, ?5) ON CONFLICT(id) DO UPDATE SET kind = excluded.kind, \
         state = excluded.state, snapshot = excluded.snapshot, updated_seq = excluded.updated_seq",
        params![
            r.id.to_string(),
            r.kind.0,
            r.state.to_text(),
            encode_runtime(cx.codec, r)?,
            cx.seq()
        ],
    )?;
    Ok(())
}

/// 运行时还没登记就提示导入是 app 层错误。
pub(super) fn import_prompted<C: EventCodec + ?Sized>(
    cx: &Cx<'_, C>,
    e: &LocalImportPrompted,
) -> Result<(), StorageError> {
    let id = e.runtime_id.to_string();
    cx.exec_hit(
        "UPDATE runtimes SET import_prompted = 1, updated_seq = ?1 WHERE id = ?2",
        params![cx.seq(), id],
        "runtimes",
        &id,
    )
}

/// 每个窗口只留最近一次。
pub(super) fn quota_observed<C: EventCodec + ?Sized>(
    cx: &Cx<'_, C>,
    e: &QuotaObserved,
) -> Result<(), StorageError> {
    for w in &e.windows {
        cx.exec(
            "INSERT INTO quota_snapshots(runtime_id, window, used_pct, resets_at, observed_at, updated_seq) \
             VALUES (?1, ?2, ?3, ?4, ?5, ?6) ON CONFLICT(runtime_id, window) DO UPDATE SET \
             used_pct = excluded.used_pct, resets_at = excluded.resets_at, \
             observed_at = excluded.observed_at, updated_seq = excluded.updated_seq",
            params![
                e.runtime_id.to_string(),
                w.window,
                w.used_pct,
                w.resets_at.map(|t| t.0),
                w.observed_at.0,
                cx.seq()
            ],
        )?;
    }
    Ok(())
}

pub(super) fn config_imported<C: EventCodec + ?Sized>(
    cx: &Cx<'_, C>,
    e: &LocalConfigImported,
) -> Result<(), StorageError> {
    for item in &e.items {
        cx.exec(
            "INSERT INTO local_config_items(runtime_id, id, kind, name, description, source, imported, \
             updated_seq) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8) ON CONFLICT(runtime_id, id) DO UPDATE SET \
             kind = excluded.kind, name = excluded.name, description = excluded.description, \
             source = excluded.source, imported = excluded.imported, updated_seq = excluded.updated_seq",
            params![
                e.runtime_id.to_string(),
                item.id,
                item.kind.to_text(),
                item.name,
                item.description,
                item.source,
                item.imported,
                cx.seq()
            ],
        )?;
    }
    Ok(())
}
