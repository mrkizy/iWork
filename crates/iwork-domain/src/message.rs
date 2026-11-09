//! 消息（技术设计 3.5、5.3）。

use crate::common::{ArtifactRef, Timestamp};
use crate::id::{AssistantId, AttachmentId, ConversationId, MessageId, ProjectId, TurnId};
use crate::machine::{IllegalTransition, Transition, finite_enum, lookup};

/// 消息作者。
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum Author {
    User,
    /// `name_snapshot`：发言时的名字。助手改名或删除后，历史消息仍显示当时的名字（技术设计 9.1a）
    Assistant {
        id: AssistantId,
        name_snapshot: String,
    },
    System,
}

finite_enum! {
    /// 消息状态。用户与系统消息创建即 `Sent`（终态）；助手消息 `Streaming` 到三种结局之一。
    #[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
    pub enum MessageStatus {
        Sent,
        Streaming,
        Completed,
        Interrupted,
        Failed,
    }
}

finite_enum! {
    /// 助手消息的结局，取自 `MessageCompleted.status`。
    #[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
    pub enum MessageTrigger {
        Completed,
        Interrupted,
        Failed,
    }
}

use MessageStatus as S;
use MessageTrigger as T;

/// 消息转换表（方案 4.3）。
pub const MESSAGE_TRANSITIONS: &[Transition<MessageStatus, MessageTrigger>] = &[
    (S::Streaming, T::Completed, S::Completed),
    (S::Streaming, T::Interrupted, S::Interrupted),
    (S::Streaming, T::Failed, S::Failed),
];

pub const MESSAGE_MACHINE: &str = "message";

pub type MessageIllegal = IllegalTransition<MessageStatus, MessageTrigger>;

/// 按 [`MESSAGE_TRANSITIONS`] 求下一状态。
pub fn next(
    status: MessageStatus,
    trigger: MessageTrigger,
) -> Result<MessageStatus, MessageIllegal> {
    lookup(MESSAGE_MACHINE, MESSAGE_TRANSITIONS, status, trigger)
}

impl MessageStatus {
    /// 新消息的初始状态：助手消息先流式输出，其他作者创建即发送完成。
    pub fn initial_for(author: &Author) -> Self {
        match author {
            Author::Assistant { .. } => S::Streaming,
            Author::User | Author::System => S::Sent,
        }
    }

    pub fn is_terminal(self) -> bool {
        self != S::Streaming
    }
}

/// 消息正文：短的内联，长的放制品存储（技术设计 3.7）。
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum MessageBody {
    Inline(String),
    Artifact(ArtifactRef),
}

/// 消息实体。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Message {
    pub id: MessageId,
    pub conversation_id: ConversationId,
    pub author: Author,
    /// 发送时私聊所选的项目（项目对话里就是对话所属项目）
    pub context_project_id: Option<ProjectId>,
    pub status: MessageStatus,
    /// 助手消息流式期间为空的内联正文，结束时由 `MessageCompleted` 写入
    pub body: MessageBody,
    pub attachments: Vec<AttachmentId>,
    pub created_at: Timestamp,
    /// 助手消息所属回合
    pub turn_id: Option<TurnId>,
}
