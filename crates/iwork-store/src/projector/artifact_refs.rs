//! 事件里的制品引用 → 投影表 `artifact_refs`（方案 2.10、ADR-0009）。
//!
//! 「谁引用了这个对象」的真值在这张表：由事件推导、随投影重建，对应实体被删时级联删行。
//! [`refs_in`] 是穷举 `match`：加一个事件不在这里表态就编译不过；新字段漏登记由对拍测试
//! `artifact_refs_match_arbitrary_events`（把事件编成 proto 后独立递归扫描）抓出来。

use iwork_domain::attachment::AttachmentOrigin;
use iwork_domain::common::ArtifactRef;
use iwork_domain::event::DomainEvent;
use iwork_domain::message::MessageBody;
use iwork_ports::store::{EventCodec, StorageError};
use rusqlite::params;

use super::Cx;
use crate::sql::{ColumnText, sha_text};

/// 引用方的种类（`artifact_refs.ref_kind`）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum RefKind {
    Message,
    ToolCall,
    UserAction,
    AgentsMdDraft,
    Attachment,
}

impl RefKind {
    pub(crate) fn text(self) -> &'static str {
        match self {
            Self::Message => "message",
            Self::ToolCall => "tool_call",
            Self::UserAction => "user_action",
            Self::AgentsMdDraft => "agents_md_draft",
            Self::Attachment => "attachment",
        }
    }
}

/// 事件里出现的一处引用。`replaces`：这条事件给出的是引用方的**新**内容，旧引用作废
/// （消息结束时的正文、卡片新的输出）；`artifact` 为 `None` 时只作废、不加新的。
struct Found {
    artifact: Option<ArtifactRef>,
    kind: RefKind,
    ref_id: String,
    replaces: bool,
}

fn refs_in(event: &DomainEvent) -> Vec<Found> {
    use DomainEvent as E;
    let one = |artifact: ArtifactRef, kind, ref_id: String, replaces| {
        vec![Found {
            artifact: Some(artifact),
            kind,
            ref_id,
            replaces,
        }]
    };
    match event {
        // 用户或系统发的长消息创建时就是 Sent + Artifact，不会再有 MessageCompleted
        E::MessageAppended(e) => match &e.message.body {
            MessageBody::Artifact(r) => one(*r, RefKind::Message, e.message.id.to_string(), false),
            MessageBody::Inline(_) => vec![],
        },
        // 正文以结束时的为准：先删这条消息原有的引用
        E::MessageCompleted(e) => match &e.body {
            MessageBody::Artifact(r) => one(*r, RefKind::Message, e.message_id.to_string(), true),
            MessageBody::Inline(_) => vec![Found {
                artifact: None,
                kind: RefKind::Message,
                ref_id: e.message_id.to_string(),
                replaces: true,
            }],
        },
        E::ToolCallFinished(e) => match e.output_artifact {
            Some(r) => one(
                r,
                RefKind::ToolCall,
                format!("{}/{}", e.turn_id, e.tool_call_id.0),
                false,
            ),
            None => vec![],
        },
        E::UserActionProposed(e) => match e.card.output_artifact {
            Some(r) => one(r, RefKind::UserAction, e.card.id.to_string(), false),
            None => vec![],
        },
        E::UserActionStateChanged(e) => match e.output_artifact {
            Some(r) => one(r, RefKind::UserAction, e.card_id.to_string(), true),
            None => vec![],
        },
        E::AgentsMdDraftProposed(e) => one(
            e.content_artifact,
            RefKind::AgentsMdDraft,
            e.card_id.to_string(),
            false,
        ),
        E::AttachmentStored(e) => match &e.attachment.origin {
            AttachmentOrigin::Copied(r)
            | AttachmentOrigin::Pasted(r)
            | AttachmentOrigin::CredentialInline(r) => {
                one(*r, RefKind::Attachment, e.attachment.id.to_string(), false)
            }
            AttachmentOrigin::RepoPath(_) => vec![],
        },
        E::ProjectAdded(_)
        | E::ProjectUpdated(_)
        | E::ProjectRelocated(_)
        | E::ProjectPathStatusChanged(_)
        | E::ProjectRemoved(_)
        | E::ProjectSettingsUpdated(_)
        | E::RepositoryInitialized(_)
        | E::CloneFinished(_)
        | E::AgentsMdDraftDecided(_)
        | E::AssistantCreated(_)
        | E::AssistantUpdated(_)
        | E::AssistantDeleted(_)
        | E::AssistantReset(_)
        | E::ConversationCreated(_)
        | E::ConversationDeleting(_)
        | E::ConversationDeleted(_)
        | E::ConversationModelChanged(_)
        | E::ApprovalModeChanged(_)
        | E::DmProjectSwitched(_)
        | E::ConversationRead(_)
        | E::TurnQueued(_)
        | E::TurnStateChanged(_)
        | E::TurnFinished(_)
        | E::TurnModelReported(_)
        | E::ToolCallStarted(_)
        | E::ApprovalRequested(_)
        | E::ApprovalReviewUpdated(_)
        | E::ApprovalDecided(_)
        | E::ApprovalVoided(_)
        | E::ApprovalEvaluated(_)
        | E::RuleRemembered(_)
        | E::RuleRevoked(_)
        | E::RulesDeleted(_)
        | E::ApprovalStaleResultDropped(_)
        | E::SecurityListEntryAdded(_)
        | E::SecurityListEntryRemoved(_)
        | E::AttachmentConfirmed(_)
        | E::RuntimeUpdated(_)
        | E::QuotaObserved(_)
        | E::UsageReported(_)
        | E::LocalImportPrompted(_)
        | E::LocalConfigImported(_)
        | E::McpSecretSet(_)
        | E::GlobalSettingsChanged(_)
        | E::RecoveryCompleted(_) => vec![],
    }
}

/// 事件里的全部制品引用（对拍测试与诊断用）。
#[doc(hidden)]
pub fn artifact_refs_in(event: &DomainEvent) -> Vec<ArtifactRef> {
    refs_in(event)
        .into_iter()
        .filter_map(|f| f.artifact)
        .collect()
}

pub(super) fn record<C: EventCodec + ?Sized>(
    cx: &Cx<'_, C>,
    event: &DomainEvent,
) -> Result<(), StorageError> {
    for f in refs_in(event) {
        if f.replaces {
            cx.exec(
                "DELETE FROM artifact_refs WHERE ref_kind = ?1 AND ref_id = ?2",
                params![f.kind.text(), f.ref_id],
            )?;
        }
        let Some(artifact) = f.artifact else { continue };
        cx.exec(
            "INSERT INTO artifact_refs(area, sha256, ref_kind, ref_id, updated_seq) \
             VALUES (?1, ?2, ?3, ?4, ?5) ON CONFLICT(area, sha256, ref_kind, ref_id) \
             DO UPDATE SET updated_seq = excluded.updated_seq",
            params![
                artifact.area.to_text(),
                sha_text(&artifact.sha256),
                f.kind.text(),
                f.ref_id,
                cx.seq()
            ],
        )?;
    }
    Ok(())
}
