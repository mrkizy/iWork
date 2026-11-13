//! 系统通知（T10a 方案 2.10、第 0a.3 节 N1、N2、S5）。
//!
//! 通知只由后台发；发不发看各个桌面客户端报的「在看哪」（[`Presence`]）。用户点了通知，后台记下要打开的那一项
//! （[`OpenTarget`]），桌面客户端被叫到前台后来问。

use std::future::Future;

use iwork_domain::id::{ApprovalId, ConversationId, JobId, TurnId, UserActionId};

use crate::read_model::PendingItem;

/// 「待你处理」里的一项是哪一项：种类加它自己的编号（审批 id、卡片 id、草稿卡片 id、继续卡片所属的回合 id）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum PendingItemRef {
    Approval(ApprovalId),
    UserAction(UserActionId),
    AgentsMdDraft(JobId),
    Resume(TurnId),
}

impl PendingItemRef {
    /// 协议里的 `pending_item_id`（`GetOpenTargetResult`、锚点）：各种类自己的编号，不带种类。
    pub fn id_string(&self) -> String {
        match self {
            Self::Approval(id) => id.to_string(),
            Self::UserAction(id) => id.to_string(),
            Self::AgentsMdDraft(id) => id.to_string(),
            Self::Resume(id) => id.to_string(),
        }
    }
}

impl PendingItem {
    /// 这一项是哪一项。
    pub fn item_ref(&self) -> PendingItemRef {
        match self {
            Self::Approval(a) => PendingItemRef::Approval(a.id),
            Self::UserAction(c) => PendingItemRef::UserAction(c.id),
            Self::AgentsMdDraft(d) => PendingItemRef::AgentsMdDraft(d.card_id),
            Self::Resume(r) => PendingItemRef::Resume(r.turn_id),
        }
    }

    /// 这一项所在的对话；AGENTS.md 草稿没有对话归属。
    pub fn conversation_id(&self) -> Option<ConversationId> {
        match self {
            Self::Approval(a) => Some(a.conversation_id),
            Self::UserAction(c) => Some(c.conversation_id),
            Self::AgentsMdDraft(_) => None,
            Self::Resume(r) => Some(r.conversation_id),
        }
    }
}

/// 点了通知之后打开哪一项。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct OpenTarget {
    pub item: PendingItemRef,
    /// 这一项所在的对话；AGENTS.md 草稿没有
    pub conversation_id: Option<ConversationId>,
}

impl OpenTarget {
    pub fn of(item: &PendingItem) -> Self {
        Self {
            item: item.item_ref(),
            conversation_id: item.conversation_id(),
        }
    }
}

/// 一条系统通知；`open` 是点了它之后打开的那一项。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SystemNotification {
    pub title: String,
    pub body: String,
    pub open: Option<OpenTarget>,
}

pub trait Notifier: Send + Sync {
    fn notify(&self, n: SystemNotification)
    -> impl Future<Output = Result<(), NotifyError>> + Send;
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum NotifyError {
    #[error("系统通知不可用：{message}")]
    Unavailable { message: String },
}

impl NotifyError {
    /// 通知丢了不影响业务，不重试。
    pub fn retryable(&self) -> bool {
        false
    }
}

/// 一个桌面客户端报的「在看哪」（`ReportPresence`）。记在连接名下，连接断开即清（S5）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Presence {
    /// 窗口在前台、没有最小化
    pub window_focused: bool,
    pub viewing: Viewing,
}

/// 主区域正显示什么。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Viewing {
    /// 别的页面（设置、助手主页、还没加载完的对话等）
    Elsewhere,
    /// 某个对话，已加载完
    Conversation(ConversationId),
    /// 「待你处理」页
    PendingList,
}
