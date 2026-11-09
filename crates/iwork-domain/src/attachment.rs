//! 附件（技术设计 9.2）。

use std::path::PathBuf;

use crate::common::{ArtifactRef, Sha256, Timestamp};
use crate::id::{AttachmentId, ConversationId};

/// 附件内容从哪来。
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum AttachmentOrigin {
    /// 仓库内文件，直接引用路径
    RepoPath(PathBuf),
    Copied(ArtifactRef),
    Pasted(ArtifactRef),
    /// 用户确认附加的凭据文件：内容存私有区，任何查询都读不到
    CredentialInline(ArtifactRef),
}

/// 附件实体。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Attachment {
    pub id: AttachmentId,
    pub conversation_id: ConversationId,
    pub sha256: Sha256,
    pub size: u64,
    pub mime: String,
    pub name: String,
    pub origin: AttachmentOrigin,
    /// 疑似凭据文件，用户确认后才附加（技术设计 9.2 第 3a 条）
    pub needs_confirmation: bool,
    pub created_at: Timestamp,
}
