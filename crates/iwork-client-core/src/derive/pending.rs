//! 「待你处理」页（任务卡 3b）：列出全部待处理项，点击跳到对话对应行。

use iwork_proto::pb;

use super::{conversation_title, text};
use crate::store::Store;

/// 待处理项的 id（审批 id、卡片 id、草稿卡片 id、回合 id）。
pub fn item_id(item: &pb::PendingItem) -> Option<&str> {
    Some(match item.item.as_ref()? {
        pb::pending_item::Item::Approval(a) => &a.id,
        pb::pending_item::Item::UserAction(c) => &c.id,
        pb::pending_item::Item::AgentsMdDraft(d) => &d.card_id,
        pb::pending_item::Item::Resume(r) => &r.turn_id,
    })
}

fn kind(item: &pb::PendingItem) -> pb::PendingKind {
    match item.item {
        Some(pb::pending_item::Item::Approval(_)) => pb::PendingKind::Approval,
        Some(pb::pending_item::Item::UserAction(_)) => pb::PendingKind::UserAction,
        Some(pb::pending_item::Item::AgentsMdDraft(_)) => pb::PendingKind::AgentsMdDraft,
        Some(pb::pending_item::Item::Resume(_)) => pb::PendingKind::Resume,
        None => pb::PendingKind::Unspecified,
    }
}

/// 一项的标题：审批写动作摘要，亲手执行写命令或补丁目标，草稿写项目名，继续卡片写中断原因。
pub fn item_title(store: &Store, item: &pb::PendingItem) -> String {
    match item.item.as_ref() {
        Some(pb::pending_item::Item::Approval(a)) => format!(
            "{}：{}",
            text::pending_kind(pb::PendingKind::Approval),
            a.action
                .as_ref()
                .map(|x| x.raw_summary.as_str())
                .unwrap_or_default()
        ),
        Some(pb::pending_item::Item::UserAction(c)) => match &c.content {
            Some(pb::user_action_card::Content::Command(x)) => x.command.clone(),
            Some(pb::user_action_card::Content::Patch(x)) => format!("修改 {}", x.path),
            None => String::new(),
        },
        Some(pb::pending_item::Item::AgentsMdDraft(d)) => store
            .summary
            .project(&d.project_id)
            .map_or_else(String::new, |p| p.name.clone()),
        Some(pb::pending_item::Item::Resume(r)) => {
            text::interrupt_reason(pb::InterruptReason::try_from(r.reason).unwrap_or_default())
                .to_owned()
        }
        None => String::new(),
    }
}

/// 待处理项在时间线上的行身份（与 `store::timeline::Row::identity` 同一套写法）。
fn row_key(item: &pb::PendingItem) -> Option<String> {
    match item.item.as_ref()? {
        pb::pending_item::Item::Approval(a) => Some(a.id.clone()),
        pb::pending_item::Item::UserAction(c) => Some(c.id.clone()),
        pb::pending_item::Item::Resume(r) => Some(format!("resume:{}", r.turn_id)),
        pb::pending_item::Item::AgentsMdDraft(_) => None,
    }
}

pub fn pending(store: &Store) -> pb::PendingView {
    let entries: Vec<pb::PendingEntry> = store
        .summary
        .pending
        .iter()
        .filter_map(|item| {
            let k = kind(item);
            let conversation_id = super::pending_conversation(item).map(str::to_owned);
            let context_label = match (&conversation_id, item.item.as_ref()) {
                (Some(cid), _) => store
                    .summary
                    .conversation(cid)
                    .map(|c| conversation_title(store, c))
                    .unwrap_or_default(),
                (None, Some(pb::pending_item::Item::AgentsMdDraft(d))) => store
                    .summary
                    .project(&d.project_id)
                    .map_or_else(String::new, |p| p.name.clone()),
                _ => String::new(),
            };
            Some(pb::PendingEntry {
                item_id: item_id(item)?.to_owned(),
                kind: k.into(),
                kind_label: text::pending_kind(k).into(),
                title: item_title(store, item),
                context_label,
                conversation_id,
                row_key: row_key(item),
                project_id: match item.item.as_ref() {
                    Some(pb::pending_item::Item::AgentsMdDraft(d)) => Some(d.project_id.clone()),
                    _ => None,
                },
            })
        })
        .collect();
    pb::PendingView {
        empty_text: if entries.is_empty() {
            text::PENDING_EMPTY.into()
        } else {
            String::new()
        },
        entries,
    }
}
