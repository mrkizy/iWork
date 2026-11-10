//! 侧栏（ui-m1.md 第 2 节第 3 条与 2a）：项目行、每个项目最近 5 个对话、助手行、额度一行、待处理计数。

use std::collections::BTreeSet;

use iwork_proto::pb;

use super::{
    Ctx, avatar, can_rename, conversation_title, expression_of_turn, pending_conversation, text,
    to_system_time,
};
use crate::store::Store;

/// 侧栏每个项目下显示的对话数。
pub const RECENT_PER_PROJECT: usize = 5;

pub fn sidebar(store: &Store, ctx: Ctx) -> pb::SidebarView {
    let waiting = waiting_conversations(store);
    let projects = store
        .summary
        .projects
        .iter()
        .map(|p| project_row(store, &waiting, ctx, p))
        .collect();
    pb::SidebarView {
        pending_count: u32::try_from(store.summary.pending.len()).unwrap_or(u32::MAX),
        projects,
        assistants: store
            .summary
            .assistants
            .iter()
            .map(|a| assistant_row(store, a))
            .collect(),
        quota: quota_cells(store, ctx),
    }
}

/// 在待处理表里有项的对话。
pub(crate) fn waiting_conversations(store: &Store) -> BTreeSet<&str> {
    store
        .summary
        .pending
        .iter()
        .filter_map(pending_conversation)
        .collect()
}

/// 圆点状态（2a 第 1 条）：在待处理表里有项 → 等你处理；最近回合失败 → 出错；在跑 → 运行中；其余空闲。
pub fn dot_state(waiting: bool, last_turn_state: Option<i32>) -> pb::DotState {
    use pb::TurnState as S;
    if waiting {
        return pb::DotState::Waiting;
    }
    match last_turn_state.and_then(|s| S::try_from(s).ok()) {
        Some(S::Failed) => pb::DotState::Error,
        Some(S::Queued | S::Running | S::WaitingApproval | S::Cancelling) => pb::DotState::Running,
        _ => pb::DotState::Idle,
    }
}

pub(crate) fn conversation_item(
    store: &Store,
    waiting: &BTreeSet<&str>,
    ctx: Ctx,
    c: &pb::Conversation,
) -> pb::ConversationItem {
    let dot = dot_state(waiting.contains(c.id.as_str()), c.last_turn_state);
    pb::ConversationItem {
        conversation_id: c.id.clone(),
        title: conversation_title(store, c),
        dot: dot.into(),
        dot_label: text::dot_label(dot).into(),
        can_rename: can_rename(c),
        can_delete: super::is_project_conversation(c),
        unread: c.unread,
        selected: store.visible.conversation_id.as_deref() == Some(c.id.as_str()),
        last_message_first_line: c.last_message_first_line.clone(),
        last_active_label: c
            .updated_at
            .as_ref()
            .map(|t| text::relative_past(ctx.now, to_system_time(t)))
            .unwrap_or_default(),
    }
}

fn project_row(
    store: &Store,
    waiting: &BTreeSet<&str>,
    ctx: Ctx,
    p: &pb::Project,
) -> pb::ProjectRow {
    let mut convs: Vec<&pb::Conversation> = store
        .summary
        .conversations
        .iter()
        .filter(|c| matches!(&c.kind, Some(pb::conversation::Kind::Project(pc)) if pc.project_id == p.id))
        .filter(|c| c.state != pb::ConversationState::Deleted as i32)
        .collect();
    // 最近活动倒序；同一时刻按 id 定序，免得两帧之间顺序抖动。
    convs.sort_by(|a, b| {
        let ka = a.updated_at.map(|t| (t.seconds, t.nanos));
        let kb = b.updated_at.map(|t| (t.seconds, t.nanos));
        kb.cmp(&ka).then_with(|| a.id.cmp(&b.id))
    });
    let has_more = convs.len() > RECENT_PER_PROJECT;
    let path_missing = p.path_status == pb::PathStatus::Missing as i32;
    pb::ProjectRow {
        project_id: p.id.clone(),
        name: p.name.clone(),
        unread: p.unread,
        pinned: p.pinned,
        path_missing,
        path_missing_label: if path_missing {
            text::PATH_MISSING.into()
        } else {
            String::new()
        },
        recent: convs
            .into_iter()
            .take(RECENT_PER_PROJECT)
            .map(|c| conversation_item(store, waiting, ctx, c))
            .collect(),
        has_more_conversations: has_more,
        root_path: p.root_path.clone(),
    }
}

fn assistant_row(store: &Store, a: &pb::Assistant) -> pb::AssistantRow {
    let dm = store.summary.dm_of(&a.id);
    let activity = store
        .summary
        .activities
        .iter()
        .find(|x| x.assistant_id == a.id);
    let expression = match activity.and_then(|x| pb::AssistantActivityKind::try_from(x.kind).ok()) {
        Some(pb::AssistantActivityKind::Waiting) => pb::Expression::Waiting,
        Some(pb::AssistantActivityKind::Running) => pb::Expression::Working,
        _ => expression_of_turn(dm.and_then(|c| c.last_turn_state)),
    };
    pb::AssistantRow {
        assistant_id: a.id.clone(),
        name: a
            .config
            .as_ref()
            .map(|c| c.name.clone())
            .unwrap_or_default(),
        avatar: Some(avatar(store, &a.id)),
        expression: expression.into(),
        activity_line: activity.and_then(|x| activity_line(store, x)),
        // 助手未读 = 它那条私聊的未读（2a 第 3 条）。
        unread: dm.map_or(0, |c| c.unread),
        can_delete: !a.builtin,
        dm_conversation_id: dm.map(|c| c.id.clone()).unwrap_or_default(),
        builtin: a.builtin,
        selected: dm
            .is_some_and(|c| store.visible.conversation_id.as_deref() == Some(c.id.as_str())),
    }
}

/// 助手行第二行（G7）：项目对话前加「标题：」，私聊不加；等你处理用待处理项的文案，运行中写「工作中」。
pub fn activity_line(store: &Store, x: &pb::AssistantActivity) -> Option<String> {
    let conv = store.summary.conversation(&x.conversation_id);
    let prefix = match conv {
        Some(c) if super::is_project_conversation(c) => {
            format!("{}：", conversation_title(store, c))
        }
        _ => String::new(),
    };
    let body = match pb::AssistantActivityKind::try_from(x.kind).ok()? {
        pb::AssistantActivityKind::Waiting => {
            let id = x.pending_item_id.as_deref()?;
            let item = store
                .summary
                .pending
                .iter()
                .find(|p| super::pending::item_id(p) == Some(id))?;
            super::pending::item_title(store, item)
        }
        pb::AssistantActivityKind::Running => text::RUNNING_ACTIVITY.to_owned(),
        pb::AssistantActivityKind::Message => x.message_first_line.clone()?,
        pb::AssistantActivityKind::Unspecified => return None,
    };
    let line = format!("{prefix}{body}");
    Some(line.lines().next().unwrap_or_default().to_owned())
}

/// 额度一行（ui-m1.md 第 2 节第 5 条）：每个运行时一格，取用率最高的窗口；取不到写「—」。
fn quota_cells(store: &Store, ctx: Ctx) -> Vec<pb::QuotaCell> {
    let mut ids: BTreeSet<&str> = store.runtimes.keys().map(String::as_str).collect();
    ids.extend(store.quota.keys().map(String::as_str));
    ids.into_iter()
        .map(|id| {
            let label = store
                .runtimes
                .get(id)
                .map_or_else(|| id.to_owned(), |r| r.display_name.clone());
            let windows = store
                .quota
                .get(id)
                .map(|q| q.windows.as_slice())
                .unwrap_or_default();
            match windows.iter().max_by_key(|w| w.used_pct) {
                None => pb::QuotaCell {
                    runtime_label: label,
                    value_text: text::QUOTA_MISSING.into(),
                    tooltip: text::QUOTA_MISSING_TIP.into(),
                },
                Some(top) => pb::QuotaCell {
                    runtime_label: label,
                    value_text: format!("{}%", top.used_pct),
                    tooltip: windows
                        .iter()
                        .map(|w| {
                            text::quota_tooltip(
                                &w.window,
                                w.used_pct,
                                w.resets_at
                                    .as_ref()
                                    .map(|t| text::relative_future(ctx.now, to_system_time(t))),
                            )
                        })
                        .collect::<Vec<_>>()
                        .join("\n"),
                },
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dot_state_priority() {
        use pb::TurnState as S;
        let failed = Some(S::Failed as i32);
        let running = Some(S::Running as i32);
        // 逐对比较：等你处理 > 出错 > 运行中 > 空闲。
        assert_eq!(dot_state(true, failed), pb::DotState::Waiting);
        assert_eq!(dot_state(true, running), pb::DotState::Waiting);
        assert_eq!(dot_state(true, None), pb::DotState::Waiting);
        assert_eq!(dot_state(false, failed), pb::DotState::Error);
        assert_eq!(dot_state(false, running), pb::DotState::Running);
        for s in [S::Queued, S::WaitingApproval, S::Cancelling] {
            assert_eq!(
                dot_state(false, Some(s as i32)),
                pb::DotState::Running,
                "{s:?}"
            );
        }
        for s in [S::Completed, S::Cancelled, S::Interrupted] {
            assert_eq!(
                dot_state(false, Some(s as i32)),
                pb::DotState::Idle,
                "{s:?}"
            );
        }
        assert_eq!(dot_state(false, None), pb::DotState::Idle);
    }
}
