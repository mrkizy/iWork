//! 助手与助手配置的版本。

use iwork_domain::assistant::{AssistantConfig, AssistantVersion};
use iwork_domain::common::Timestamp;
use iwork_domain::event::{AssistantCreated, AssistantDeleted};
use iwork_domain::id::AssistantId;
use iwork_ports::store::{EventCodec, StorageError};
use rusqlite::params;

use super::{Cx, conversations};
use crate::snapshot::{decode_assistant, encode_assistant, encode_assistant_version};

pub(super) fn created<C: EventCodec + ?Sized>(
    cx: &Cx<'_, C>,
    e: &AssistantCreated,
) -> Result<(), StorageError> {
    let a = &e.assistant;
    cx.exec(
        "INSERT INTO assistants(id, name, builtin, current_version, snapshot, updated_seq) \
         VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
        params![
            a.id.to_string(),
            a.current.config.name,
            a.builtin,
            a.current.version,
            encode_assistant(cx.codec, a)?,
            cx.seq()
        ],
    )?;
    insert_version(
        cx,
        a.id,
        a.current.version,
        &a.current.config,
        a.current.created_at,
    )
}

/// `AssistantUpdated` 与 `AssistantReset` 都带完整配置：成为新的当前版本，并记一行版本。
/// 版本的创建时间取事件信封的 `at`（事件没有单独的时间字段）。
pub(super) fn new_version<C: EventCodec + ?Sized>(
    cx: &Cx<'_, C>,
    id: AssistantId,
    version: u32,
    config: &AssistantConfig,
) -> Result<(), StorageError> {
    let key = id.to_string();
    let mut a = decode_assistant(cx.codec, &cx.snapshot("assistants", "id", &key)?)?;
    a.current = AssistantVersion {
        version,
        config: config.clone(),
        created_at: cx.env.at,
    };
    cx.exec_hit(
        "UPDATE assistants SET name = ?1, current_version = ?2, snapshot = ?3, updated_seq = ?4 \
         WHERE id = ?5",
        params![
            config.name,
            version,
            encode_assistant(cx.codec, &a)?,
            cx.seq(),
            key
        ],
        "assistants",
        &key,
    )?;
    insert_version(cx, id, version, config, cx.env.at)
}

fn insert_version<C: EventCodec + ?Sized>(
    cx: &Cx<'_, C>,
    id: AssistantId,
    version: u32,
    config: &AssistantConfig,
    created_at: Timestamp,
) -> Result<(), StorageError> {
    cx.exec(
        "INSERT INTO assistant_versions(assistant_id, version, created_at, snapshot, updated_seq) \
         VALUES (?1, ?2, ?3, ?4, ?5)",
        params![
            id.to_string(),
            version,
            created_at.0,
            encode_assistant_version(cx.codec, id, version, config)?,
            cx.seq()
        ],
    )?;
    Ok(())
}

/// 删助手、它的版本与它的私聊（连同私聊里的一切）。项目对话里它的消息保留：
/// 名字快照在消息里（设计 9.1a）。
pub(super) fn deleted<C: EventCodec + ?Sized>(
    cx: &Cx<'_, C>,
    e: &AssistantDeleted,
) -> Result<(), StorageError> {
    let id = e.assistant_id.to_string();
    let dms: Vec<String> = cx
        .conn
        .prepare_cached("SELECT id FROM conversations WHERE assistant_id = ?1")
        .and_then(|mut s| s.query_map([&id], |r| r.get(0))?.collect())
        .map_err(crate::sql::sql_err)?;
    super::owner::require_cascade(cx, &dms)?;
    for dm in dms {
        conversations::delete_cascade(cx, &dm)?;
    }
    cx.exec(
        "DELETE FROM assistant_versions WHERE assistant_id = ?1",
        [&id],
    )?;
    cx.exec_hit(
        "DELETE FROM assistants WHERE id = ?1",
        [&id],
        "assistants",
        &id,
    )
}
