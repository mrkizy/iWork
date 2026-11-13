//! 项目与 AGENTS.md 草稿。

use iwork_domain::event::{
    AgentsMdDraftDecided, AgentsMdDraftProposed, ProjectAdded, ProjectPathStatusChanged,
    ProjectRelocated, ProjectRemoved, ProjectSettingsUpdated, ProjectUpdated,
};
use iwork_domain::project::{PathStatus, Project, check_settings_committed};
use iwork_ports::store::{EventCodec, StorageError};
use rusqlite::params;

use super::Cx;
use super::owner::precondition;
use crate::snapshot::{decode_project, encode_project};
use crate::sql::{ColumnText, sha_text, u64_val};

pub(super) fn added<C: EventCodec + ?Sized>(
    cx: &Cx<'_, C>,
    e: &ProjectAdded,
) -> Result<(), StorageError> {
    let p = &e.project;
    cx.exec(
        "INSERT INTO projects(id, root_path, name, pinned, last_active_at, path_status, \
         default_approval_mode, snapshot, updated_seq) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
        params![
            p.id.to_string(),
            p.root_path.to_string_lossy(),
            p.name,
            p.pinned,
            p.last_active_at.map(|t| t.0),
            p.path_status.to_text(),
            p.settings.default_approval_mode.to_text(),
            encode_project(cx.codec, p)?,
            cx.seq()
        ],
    )?;
    Ok(())
}

/// 读快照 → 改 → 写回全部索引列（各索引列都由快照推出，所以整行同步写不会盖掉别的写者）。
fn modify<C: EventCodec + ?Sized>(
    cx: &Cx<'_, C>,
    id: impl std::fmt::Display,
    change: impl FnOnce(&mut Project) -> Result<(), StorageError>,
) -> Result<(), StorageError> {
    let id = id.to_string();
    let mut p = decode_project(cx.codec, &cx.snapshot("projects", "id", &id)?)?;
    change(&mut p)?;
    cx.exec_hit(
        "UPDATE projects SET root_path = ?1, name = ?2, pinned = ?3, last_active_at = ?4, \
         path_status = ?5, default_approval_mode = ?6, snapshot = ?7, updated_seq = ?8 WHERE id = ?9",
        params![
            p.root_path.to_string_lossy(),
            p.name,
            p.pinned,
            p.last_active_at.map(|t| t.0),
            p.path_status.to_text(),
            p.settings.default_approval_mode.to_text(),
            encode_project(cx.codec, &p)?,
            cx.seq(),
            id
        ],
        "projects",
        &id,
    )
}

pub(super) fn updated<C: EventCodec + ?Sized>(
    cx: &Cx<'_, C>,
    e: &ProjectUpdated,
) -> Result<(), StorageError> {
    modify(cx, e.project_id, |p| {
        if let Some(name) = &e.name {
            p.name = name.clone();
        }
        if let Some(pinned) = e.pinned {
            p.pinned = pinned;
        }
        if let Some(at) = e.last_active_at {
            p.last_active_at = Some(at);
        }
        Ok(())
    })
}

/// 重新定位只在新位置读出的项目 ID 一致时发生（`Project::relocate`），所以路径状态同时回到 `Ok`。
pub(super) fn relocated<C: EventCodec + ?Sized>(
    cx: &Cx<'_, C>,
    e: &ProjectRelocated,
) -> Result<(), StorageError> {
    modify(cx, e.project_id, |p| {
        p.root_path = e.new_path.clone();
        p.path_status = PathStatus::Ok;
        Ok(())
    })
}

pub(super) fn path_status_changed<C: EventCodec + ?Sized>(
    cx: &Cx<'_, C>,
    e: &ProjectPathStatusChanged,
) -> Result<(), StorageError> {
    modify(cx, e.project_id, |p| {
        p.path_status = e.path_status;
        Ok(())
    })
}

/// 整份设置写回之前先过状态前提（T10a 1/F-1）：库里的设置还是事件据以合并的那一份。
pub(super) fn settings_updated<C: EventCodec + ?Sized>(
    cx: &Cx<'_, C>,
    e: &ProjectSettingsUpdated,
) -> Result<(), StorageError> {
    modify(cx, e.project_id, |p| {
        precondition(
            cx,
            &format!("项目 {}", e.project_id),
            check_settings_committed(&p.settings, e),
        )?;
        p.settings = e.settings.clone();
        Ok(())
    })
}

/// 项目默认档位（`ApprovalModeChanged{target: Project}`，方案 Q1 (a)）。
pub(super) fn set_default_mode<C: EventCodec + ?Sized>(
    cx: &Cx<'_, C>,
    id: impl std::fmt::Display,
    mode: iwork_domain::approval_mode::ProjectDefaultMode,
) -> Result<(), StorageError> {
    modify(cx, id, |p| {
        p.settings.default_approval_mode = mode;
        Ok(())
    })
}

/// 删项目行与它的草稿卡片。项目对话由 app 先发 `ConversationDeleted`（设计 9.0），这里不替它删。
pub(super) fn removed<C: EventCodec + ?Sized>(
    cx: &Cx<'_, C>,
    e: &ProjectRemoved,
) -> Result<(), StorageError> {
    let id = e.project_id.to_string();
    cx.exec(
        "DELETE FROM artifact_refs WHERE ref_kind = 'agents_md_draft' AND ref_id IN \
         (SELECT card_id FROM agents_md_drafts WHERE project_id = ?1)",
        [&id],
    )?;
    cx.exec("DELETE FROM agents_md_drafts WHERE project_id = ?1", [&id])?;
    cx.exec_hit("DELETE FROM projects WHERE id = ?1", [&id], "projects", &id)
}

pub(super) fn draft_proposed<C: EventCodec + ?Sized>(
    cx: &Cx<'_, C>,
    e: &AgentsMdDraftProposed,
) -> Result<(), StorageError> {
    let a = &e.content_artifact;
    cx.exec(
        "INSERT INTO agents_md_drafts(card_id, project_id, content_sha256, content_area, content_size, \
         proposed_at, decision, updated_seq) VALUES (?1, ?2, ?3, ?4, ?5, ?6, NULL, ?7)",
        params![
            e.card_id.to_string(),
            e.project_id.to_string(),
            sha_text(&a.sha256),
            a.area.to_text(),
            u64_val("制品大小", a.size)?,
            cx.env.at.0,
            cx.seq()
        ],
    )?;
    Ok(())
}

pub(super) fn draft_decided<C: EventCodec + ?Sized>(
    cx: &Cx<'_, C>,
    e: &AgentsMdDraftDecided,
) -> Result<(), StorageError> {
    let id = e.card_id.to_string();
    cx.exec_hit(
        "UPDATE agents_md_drafts SET decision = ?1, updated_seq = ?2 WHERE card_id = ?3",
        params![e.decision.to_text(), cx.seq(), id],
        "agents_md_drafts",
        &id,
    )
}
