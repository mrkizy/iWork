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

/// 输入框的模型菜单要哪个运行时的清单（会话层据此取）：打开的对话的；没打开对话、看着首页时是内置 iWork 的。
pub fn menu_runtime(store: &Store) -> Option<&str> {
    let assistant = match store.open.as_ref() {
        Some(tl) => answering_assistant(store, conversation(store, tl)?)?,
        None if store.visible.home.is_some() => builtin_assistant(store)?,
        None => return None,
    };
    let id = assistant.config.as_ref()?.runtime_id.as_str();
    (!id.is_empty()).then_some(id)
}

/// 内置 iWork：项目对话（与首页）由它答。
fn builtin_assistant(store: &Store) -> Option<&pb::Assistant> {
    store.summary.assistants.iter().find(|a| a.builtin)
}

/// 这个对话由哪个助手答（项目对话 M1 是内置 iWork，私聊是那个助手）。
fn answering_assistant<'a>(store: &'a Store, conv: &pb::Conversation) -> Option<&'a pb::Assistant> {
    match &conv.kind {
        Some(pb::conversation::Kind::AssistantDm(dm)) => store.summary.assistant(&dm.assistant_id),
        _ => builtin_assistant(store),
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
        notice: (store.back_to_latest.as_deref() == Some(tl.conversation_id.as_str()))
            .then(|| text::BACK_TO_LATEST.to_owned()),
        is_dm,
        project_id,
    }
}

pub fn composer(store: &Store, tl: &Timeline) -> pb::ComposerView {
    let conv = conversation(store, tl);
    let assistant = conv.and_then(|c| answering_assistant(store, c));
    let project_id = conv.and_then(|c| match &c.kind {
        Some(pb::conversation::Kind::Project(p)) => Some(p.project_id.as_str()),
        _ => None,
    });
    let is_dm = matches!(
        conv.and_then(|c| c.kind.as_ref()),
        Some(pb::conversation::Kind::AssistantDm(_))
    );
    composer_view(
        store,
        ComposerInput {
            project_id,
            assistant,
            model_override: conv.and_then(|c| c.model_override.as_ref()),
            approval: approval_mode(
                store,
                conv.and_then(|c| c.approval_mode),
                project_id,
                assistant,
                is_dm,
            ),
            deleting: conv.is_some_and(|c| c.state == pb::ConversationState::Deleting as i32),
            // 对话开始后运行时一节置灰（ui-m1.md 第 2 节第 14 条）。
            runtime_locked: !tl.turns.is_empty(),
            running_turn_id: tl.running_turn().map(|t| t.id.as_str()),
            tray: tray(tl),
        },
    )
}

/// 首页（画布 01）：对着网页声明的项目，没声明或已不在时取侧栏里的第一个。
pub fn home(store: &Store) -> pb::HomeView {
    let wanted = store.visible.home.as_deref().filter(|id| !id.is_empty());
    let project = wanted
        .and_then(|id| store.summary.project(id))
        .or_else(|| store.summary.projects.first());
    let Some(p) = project else {
        return pb::HomeView::default();
    };
    pb::HomeView {
        project_id: p.id.clone(),
        project_name: p.name.clone(),
        composer: Some(home_composer(store, &p.id)),
        starting: store.home.start.is_some(),
        start_error: store.home.error.as_ref().map(text::start_error),
    }
}

/// 首页的输入框（画布 01）：和对话里的一样，答话的是内置 iWork；模型与档位用首页选过的值，没选过用默认。
pub fn home_composer(store: &Store, project_id: &str) -> pb::ComposerView {
    let assistant = builtin_assistant(store);
    composer_view(
        store,
        ComposerInput {
            project_id: Some(project_id),
            assistant,
            model_override: store.home.model.as_ref(),
            approval: approval_mode(
                store,
                store.home.mode.map(|m| m as i32),
                Some(project_id),
                assistant,
                false,
            ),
            deleting: false,
            runtime_locked: false,
            running_turn_id: None,
            tray: Vec::new(),
        },
    )
}

/// 输入框要的上下文：对话页从打开的对话算，首页从项目与首页的选择算。
struct ComposerInput<'a> {
    /// 项目对话（或首页）所在的项目；私聊为空。
    project_id: Option<&'a str>,
    assistant: Option<&'a pb::Assistant>,
    /// 本对话（首页：本次新建）改选的模型；没有就用助手配置的。
    model_override: Option<&'a pb::ModelSelection>,
    approval: pb::ApprovalModeView,
    deleting: bool,
    runtime_locked: bool,
    running_turn_id: Option<&'a str>,
    tray: Vec<pb::AttachmentChip>,
}

fn composer_view(store: &Store, input: ComposerInput<'_>) -> pb::ComposerView {
    let live = commands_enabled(store);
    let config = input.assistant.and_then(|a| a.config.as_ref());
    let runtime = config.and_then(|c| store.runtimes.get(&c.runtime_id));
    let runtime_name = runtime.map_or_else(String::new, |r| r.display_name.clone());
    let model = input
        .model_override
        .filter(|m| !m.model_id.is_empty())
        .or_else(|| config.and_then(|c| c.model.as_ref()));
    let runtime_model_label = match model {
        Some(m) => format!("{runtime_name} · {}", text::model_label(m)),
        None => runtime_name.clone(),
    };

    let project_missing = input
        .project_id
        .and_then(|id| store.summary.project(id))
        .is_some_and(|p| p.path_status == pb::PathStatus::Missing as i32);
    let runtime_state = runtime.map_or(pb::RuntimeState::Unspecified, |r| {
        pb::RuntimeState::try_from(r.state).unwrap_or_default()
    });

    // 挡住发送的原因，按先后取第一条。
    let block = if !live {
        Some(Block::plain(text::COMPOSER_OFFLINE.into()))
    } else if input.deleting {
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
        can_stop: input.running_turn_id.is_some() && live,
        running_turn_id: input.running_turn_id.map(str::to_owned),
        runtime_model_label,
        runtime_locked: input.runtime_locked,
        model_menu: Some(super::model_menu::model_menu(
            store,
            &super::model_menu::MenuInput {
                runtime_id: config.map_or("", |c| c.runtime_id.as_str()),
                model,
                runtime_locked: input.runtime_locked,
            },
        )),
        approval_mode: Some(input.approval),
        attach_error: store.attach_error.clone(),
        tray: input.tray,
        placeholder: text::composer_placeholder(config.map_or("", |c| c.name.as_str())),
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

/// 档位按钮：对话（首页：本次新建）单独切过的优先，否则用项目默认，都没有是手动。
fn approval_mode(
    store: &Store,
    chosen: Option<i32>,
    project_id: Option<&str>,
    assistant: Option<&pb::Assistant>,
    is_dm: bool,
) -> pb::ApprovalModeView {
    let project_default = project_id
        .and_then(|id| store.summary.project(id))
        .and_then(|p| p.settings.as_ref())
        .map(|s| s.default_approval_mode);
    let current = chosen
        .or(project_default)
        .and_then(|m| pb::ApprovalMode::try_from(m).ok())
        .filter(|m| *m != pb::ApprovalMode::Unspecified)
        .unwrap_or(pb::ApprovalMode::Manual);
    let dm_name = is_dm
        .then(|| {
            assistant
                .and_then(|a| a.config.as_ref())
                .map(|c| c.name.as_str())
        })
        .flatten();
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
            description: text::approval_mode_description(m).into(),
            tag: if m == pb::ApprovalMode::FullAccess {
                text::FULL_ACCESS_TAG.into()
            } else {
                String::new()
            },
        })
        .collect(),
        current: current as i32,
        scope_title: text::mode_scope_title(dm_name),
        // M1 一个对话只有一个助手作答：受影响的就是它（单独设置的成员 M2 才有）。
        full_access_confirm: (current != pb::ApprovalMode::FullAccess).then(|| pb::ConfirmView {
            for_value: pb::ApprovalMode::FullAccess as i32,
            ..text::full_access_confirm(
                assistant
                    .and_then(|a| a.config.as_ref())
                    .map_or("助手", |c| c.name.as_str()),
            )
        }),
        shield_checked: current != pb::ApprovalMode::Manual,
    }
}
