//! 全局设置。投影器不持有默认值：没给的列保持 NULL，读模型在读时补（方案 2.6）。

use iwork_domain::event::GlobalSettingsChanged;
use iwork_ports::store::{EventCodec, StorageError};
use rusqlite::params;

use super::Cx;

/// 只更新事件里给了值的列（`COALESCE(新值, 旧值)`）。
pub(super) fn changed<C: EventCodec + ?Sized>(
    cx: &Cx<'_, C>,
    e: &GlobalSettingsChanged,
) -> Result<(), StorageError> {
    cx.exec(
        "INSERT INTO global_settings(id, playful_ui, autostart, projects_root, updated_seq) \
         VALUES (1, ?1, ?2, ?3, ?4) ON CONFLICT(id) DO UPDATE SET \
         playful_ui = COALESCE(excluded.playful_ui, playful_ui), \
         autostart = COALESCE(excluded.autostart, autostart), \
         projects_root = COALESCE(excluded.projects_root, projects_root), \
         updated_seq = excluded.updated_seq",
        params![
            e.playful_ui,
            e.autostart,
            e.projects_root
                .as_ref()
                .map(|p| p.to_string_lossy().into_owned()),
            cx.seq()
        ],
    )?;
    Ok(())
}
