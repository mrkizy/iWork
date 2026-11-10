//! 附件。`confirmed` 不是 domain 字段，单独一列，只由 `AttachmentConfirmed` 写（方案 2.6 列归属）。

use iwork_domain::attachment::AttachmentOrigin;
use iwork_domain::event::{AttachmentConfirmed, AttachmentStored};
use iwork_ports::store::{EventCodec, StorageError};
use rusqlite::params;

use super::Cx;
use crate::snapshot::encode_attachment;
use crate::sql::{sha_text, u64_val};

fn origin_kind(origin: &AttachmentOrigin) -> &'static str {
    match origin {
        AttachmentOrigin::RepoPath(_) => "repo_path",
        AttachmentOrigin::Copied(_) => "copied",
        AttachmentOrigin::Pasted(_) => "pasted",
        AttachmentOrigin::CredentialInline(_) => "credential_inline",
    }
}

pub(super) fn stored<C: EventCodec + ?Sized>(
    cx: &Cx<'_, C>,
    e: &AttachmentStored,
) -> Result<(), StorageError> {
    let a = &e.attachment;
    cx.exec(
        "INSERT INTO attachments(id, conversation_id, sha256, size, mime, name, origin_kind, \
         needs_confirmation, confirmed, created_at, snapshot, updated_seq) \
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, 0, ?9, ?10, ?11)",
        params![
            a.id.to_string(),
            a.conversation_id.to_string(),
            sha_text(&a.sha256),
            u64_val("附件大小", a.size)?,
            a.mime,
            a.name,
            origin_kind(&a.origin),
            a.needs_confirmation,
            a.created_at.0,
            encode_attachment(cx.codec, a)?,
            cx.seq()
        ],
    )?;
    Ok(())
}

pub(super) fn confirmed<C: EventCodec + ?Sized>(
    cx: &Cx<'_, C>,
    e: &AttachmentConfirmed,
) -> Result<(), StorageError> {
    let id = e.attachment_id.to_string();
    cx.exec_hit(
        "UPDATE attachments SET confirmed = 1, updated_seq = ?1 WHERE id = ?2",
        params![cx.seq(), id],
        "attachments",
        &id,
    )
}
