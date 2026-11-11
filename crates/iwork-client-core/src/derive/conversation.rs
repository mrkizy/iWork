//! 对话页：标题栏、输入框一带、行窗口。

use iwork_proto::pb;

use super::{commands_enabled, conversation_title, text};
use crate::store::Store;
use crate::store::timeline::{RowKey, Timeline};

/// 窗口前后各多带的行数（方案 2.4 第 9 条）。
pub const WINDOW_MARGIN: usize = 30;
/// 网页还没声明窗口时，先给最新的这么多行。
pub const INITIAL_TAIL: usize = 60;

/// 窗口里要发的行（按位置排序）与窗口信息。
pub fn window(store: &Store, tl: &Timeline) -> (Vec<(RowKey, String)>, pb::WindowInfo) {
    let all = tl.identities();
    let total = all.len();
    let (start, end) = match store.window {
        Some((first, last)) => {
            let lo = all.partition_point(|(k, _)| k.ordinal() < first);
            let hi = all.partition_point(|(k, _)| k.ordinal() <= last);
            (
                lo.saturating_sub(WINDOW_MARGIN),
                (hi + WINDOW_MARGIN).min(total),
            )
        }
        None => (total.saturating_sub(INITIAL_TAIL), total),
    };
    let rows = all[start..end].to_vec();
    let info = pb::WindowInfo {
        first_index: u32::try_from(start).unwrap_or(u32::MAX),
        count: u32::try_from(end - start).unwrap_or(u32::MAX),
        total: u32::try_from(total).unwrap_or(u32::MAX),
        has_older: tl.has_older,
        at_tail: tl.at_tail,
        new_below: tl.new_below,
        loading_older: false,
    };
    (rows, info)
}

/// 这个对话由哪个助手答（项目对话 M1 是内置 iWork，私聊是那个助手）。
fn answering_assistant<'a>(store: &'a Store, conv: &pb::Conversation) -> Option<&'a pb::Assistant> {
    match &conv.kind {
        Some(pb::conversation::Kind::AssistantDm(dm)) => store.summary.assistant(&dm.assistant_id),
        _ => store.summary.assistants.iter().find(|a| a.builtin),
    }
}

/// 对话本身：时间线里的（打开对话时查询带回、随事件更新）优先，没有就用摘要表里的。
fn conversation<'a>(store: &'a Store, tl: &'a Timeline) -> Option<&'a pb::Conversation> {
    tl.conversation
        .as_ref()
        .or_else(|| store.summary.conversation(&tl.conversation_id))
}

pub fn header(store: &Store, tl: &Timeline) -> pb::ConversationHeader {
    let Some(conv) = conversation(store, tl) else {
        return pb::ConversationHeader::default();
    };
    // 标题与能否重命名是摘要值：摘要表里的更新（改名后重查），时间线里的只是打开那一刻的。
    let summary_conv = store
        .summary
        .conversation(&tl.conversation_id)
        .unwrap_or(conv);
    let (is_dm, project_id, selector) = match &conv.kind {
        Some(pb::conversation::Kind::AssistantDm(dm)) => {
            let current = dm.context_project_id.as_deref().unwrap_or_default();
            let mut options = vec![pb::ProjectOption {
                project_id: String::new(),
                label: "私聊".into(),
                selected: current.is_empty(),
            }];
            options.extend(store.summary.projects.iter().map(|p| pb::ProjectOption {
                project_id: p.id.clone(),
                label: p.name.clone(),
                selected: p.id == current,
            }));
            let label = options.iter().find(|o| o.selected).map(|o| o.label.clone());
            (true, current.to_owned(), (label, options))
        }
        Some(pb::conversation::Kind::Project(p)) => {
            (false, p.project_id.clone(), (None, Vec::new()))
        }
        None => (false, String::new(), (None, Vec::new())),
    };
    let usage = tl.usage_total.unwrap_or_default();
    pb::ConversationHeader {
        title: conversation_title(store, summary_conv),
        can_rename: super::can_rename(summary_conv),
        usage_text: text::usage_text(&usage),
        usage_tooltip: text::usage_tooltip(&usage),
        project_selector_label: selector.0,
        project_options: selector.1,
        notice: None,
        is_dm,
        project_id,
    }
}

pub fn composer(store: &Store, tl: &Timeline) -> pb::ComposerView {
    let live = commands_enabled(store);
    let conv = conversation(store, tl);
    let assistant = conv.and_then(|c| answering_assistant(store, c));
    let config = assistant.and_then(|a| a.config.as_ref());
    let runtime = config.and_then(|c| store.runtimes.get(&c.runtime_id));
    let runtime_name = runtime.map_or_else(String::new, |r| r.display_name.clone());
    let model = conv
        .and_then(|c| c.model_override.as_ref())
        .filter(|m| !m.model_id.is_empty())
        .or_else(|| config.and_then(|c| c.model.as_ref()));
    let runtime_model_label = match model {
        Some(m) => format!("{runtime_name} · {}", text::model_label(m)),
        None => runtime_name.clone(),
    };

    let project_missing = match conv.and_then(|c| c.kind.as_ref()) {
        Some(pb::conversation::Kind::Project(p)) => store
            .summary
            .project(&p.project_id)
            .is_some_and(|p| p.path_status == pb::PathStatus::Missing as i32),
        _ => false,
    };
    let deleting = conv.is_some_and(|c| c.state == pb::ConversationState::Deleting as i32);
    let runtime_state = runtime.map_or(pb::RuntimeState::Unspecified, |r| {
        pb::RuntimeState::try_from(r.state).unwrap_or_default()
    });

    // 挡住发送的原因，按先后取第一条。
    let block = if !live {
        Some(Block::plain(text::COMPOSER_OFFLINE.into()))
    } else if deleting {
        Some(Block::plain(text::COMPOSER_DELETING.into()))
    } else if project_missing {
        Some(Block {
            tone: pb::Tone::Warning,
            action: Some((text::RELOCATE, pb::BlockAction::RelocateProject)),
            ..Block::plain(text::COMPOSER_PATH_MISSING.into())
        })
    } else {
        text::composer_block(runtime_state, &runtime_name).map(|msg| match runtime_state {
            pb::RuntimeState::ReadyUnverified => Block {
                busy: true,
                ..Block::plain(msg)
            },
            pb::RuntimeState::Incompatible => Block {
                tone: pb::Tone::Warning,
                action: Some((text::DETAILS, pb::BlockAction::OpenRuntimeDetails)),
                ..Block::plain(msg)
            },
            _ => Block {
                action: Some((text::DETAILS, pb::BlockAction::OpenRuntimeDetails)),
                ..Block::plain(msg)
            },
        })
    };

    let running = tl.running_turn();
    let (block_reason, block_tone, busy, action) = match block {
        Some(b) => (Some(b.reason), b.tone, b.busy, b.action),
        None => (None, pb::Tone::Unspecified, false, None),
    };
    pb::ComposerView {
        can_send: block_reason.is_none(),
        block_reason,
        block_tone: block_tone.into(),
        busy,
        block_action_label: action.map(|a| a.0.to_owned()),
        block_action: action.map_or(pb::BlockAction::Unspecified, |a| a.1).into(),
        can_stop: running.is_some() && live,
        running_turn_id: running.map(|t| t.id.clone()),
        runtime_model_label,
        // 对话开始后运行时一节置灰（ui-m1.md 第 2 节第 14 条）。
        runtime_locked: !tl.turns.is_empty(),
        approval_mode: Some(approval_mode(store, conv, assistant)),
        attach_error: store.attach_error.clone(),
        tray: tray(tl),
    }
}

/// 附件托盘：本对话已上传、还没被任何消息引用的附件（从时间线算，不另存一份）。
fn tray(tl: &Timeline) -> Vec<pb::AttachmentChip> {
    let used: std::collections::BTreeSet<&str> = tl
        .rows()
        .filter_map(|(_, r)| match r {
            crate::store::Row::Message(m) => Some(m.attachment_ids.iter().map(String::as_str)),
            _ => None,
        })
        .flatten()
        .collect();
    tl.attachments
        .values()
        .filter(|a| !used.contains(a.id.as_str()))
        .map(|a| pb::AttachmentChip {
            attachment_id: a.id.clone(),
            name: a.name.clone(),
            size_label: text::size_label(a.size),
            needs_confirmation: a.needs_confirmation,
        })
        .collect()
}

/// 输入框被挡住的一条说明。
struct Block {
    reason: String,
    tone: pb::Tone,
    busy: bool,
    action: Option<(&'static str, pb::BlockAction)>,
}

impl Block {
    fn plain(reason: String) -> Self {
        Self {
            reason,
            tone: pb::Tone::Neutral,
            busy: false,
            action: None,
        }
    }
}

fn approval_mode(
    store: &Store,
    conv: Option<&pb::Conversation>,
    assistant: Option<&pb::Assistant>,
) -> pb::ApprovalModeView {
    let project_default = conv
        .and_then(|c| match &c.kind {
            Some(pb::conversation::Kind::Project(p)) => store.summary.project(&p.project_id),
            _ => None,
        })
        .and_then(|p| p.settings.as_ref())
        .map(|s| s.default_approval_mode);
    let current = conv
        .and_then(|c| c.approval_mode)
        .or(project_default)
        .and_then(|m| pb::ApprovalMode::try_from(m).ok())
        .filter(|m| *m != pb::ApprovalMode::Unspecified)
        .unwrap_or(pb::ApprovalMode::Manual);
    let dm_name = match conv.and_then(|c| c.kind.as_ref()) {
        Some(pb::conversation::Kind::AssistantDm(_)) => assistant
            .and_then(|a| a.config.as_ref())
            .map(|c| c.name.as_str()),
        _ => None,
    };
    pb::ApprovalModeView {
        label: text::approval_mode(current).into(),
        full_access: current == pb::ApprovalMode::FullAccess,
        overridden: false,
        options: [
            pb::ApprovalMode::Manual,
            pb::ApprovalMode::Auto,
            pb::ApprovalMode::FullAccess,
        ]
        .into_iter()
        .map(|m| pb::OptionView {
            value: m as i32,
            label: text::approval_mode(m).into(),
        })
        .collect(),
        current: current as i32,
        scope_title: text::mode_scope_title(dm_name),
    }
}
