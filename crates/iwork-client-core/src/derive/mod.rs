//! 从状态库算视图模型（T09 方案 2.4、R6）。全是纯函数：输入状态与时钟，输出 `view.proto` 的消息。
//!
//! 计数、标签文字、按钮可否点击、表情状态只在这里算；网页只把结果画出来。后台给的文字（审批范围说明、
//! 错误说明）原样用；摘要值（未读、标题、最近动态）只读后台的，不在这里推演。

pub mod add_project;
pub mod anchors;
pub mod assistant;
pub mod avatar;
pub mod connection;
pub mod conversation;
pub mod model_menu;
pub mod pending;
pub mod row;
pub mod settings;
pub mod sidebar;
pub mod text;

use std::time::{Duration, SystemTime};

use iwork_proto::pb;

use crate::store::{Connection, Store};

/// 派生时的外部输入：只有时钟。
#[derive(Debug, Clone, Copy)]
pub struct Ctx {
    pub now: SystemTime,
}

pub(crate) fn to_system_time(ts: &prost_types::Timestamp) -> SystemTime {
    let secs = u64::try_from(ts.seconds).unwrap_or(0);
    let nanos = u32::try_from(ts.nanos).unwrap_or(0);
    SystemTime::UNIX_EPOCH + Duration::new(secs, nanos)
}

/// 会发命令的按钮能不能点：只有连接 Live 时能（方案 D8）。
pub(crate) fn commands_enabled(store: &Store) -> bool {
    store.connection == Connection::Live
}

/// 对话标题：AUTO 且还没有标题显示「新对话」；私聊没给标题时用助手名（技术设计 9.0a）。
pub fn conversation_title(store: &Store, c: &pb::Conversation) -> String {
    if !c.title.is_empty() {
        return c.title.clone();
    }
    if let Some(pb::conversation::Kind::AssistantDm(dm)) = &c.kind
        && let Some(a) = store.summary.assistant(&dm.assistant_id)
    {
        return a
            .config
            .as_ref()
            .map(|c| c.name.clone())
            .unwrap_or_default();
    }
    text::NEW_CONVERSATION.to_owned()
}

/// 只有项目对话、且标题不是固定的，才能重命名（技术设计 9.0a）。
pub fn can_rename(c: &pb::Conversation) -> bool {
    matches!(c.kind, Some(pb::conversation::Kind::Project(_)))
        && c.title_source != pb::TitleSource::Fixed as i32
}

pub fn is_project_conversation(c: &pb::Conversation) -> bool {
    matches!(c.kind, Some(pb::conversation::Kind::Project(_)))
}

/// 一个待处理项属于哪个对话。
pub(crate) fn pending_conversation(item: &pb::PendingItem) -> Option<&str> {
    match item.item.as_ref()? {
        pb::pending_item::Item::Approval(a) => Some(&a.conversation_id),
        pb::pending_item::Item::UserAction(c) => Some(&c.conversation_id),
        pb::pending_item::Item::Resume(r) => Some(&r.conversation_id),
        pb::pending_item::Item::AgentsMdDraft(_) => None,
    }
}
