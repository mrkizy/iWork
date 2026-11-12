//! 网页操作 → 命令 / 本地视图状态 / 效果（T09 方案 2.1 `intent.rs`：一张表）。
//!
//! 加一种用户操作 = `UiIntent` 一个变体 + [`map`] 一臂。可否点击已经在 derive 里算过；这里再按状态校验一次
//! 会被命令拒绝的输入（如改名长度、没变的标题），提议哈希由 Rust 从实体取（R7）。

use iwork_proto::pb;
use pb::command::Kind as Cmd;
use pb::ui_intent::Kind as I;

use crate::store::{ListQuery, Row, Store, Visible};

/// 一个操作落到哪。
#[derive(Debug, Clone, PartialEq)]
pub enum Action {
    /// 改本地视图状态，不发命令。
    Local(Local),
    /// 转成命令发给后台。
    Command(Cmd),
    /// 交给壳做的事。
    Effect(Effect),
    /// 粘贴的附件数据，由 attach 模块处理。本机文件不经网页（见 [`Pick`]）。
    AttachPasted(pb::AttachPasted),
    /// 要用户在系统选择框里选路径：壳弹框，选好后会话按 [`Pick`] 发命令。
    Pick(Pick),
    /// 本地就能判定不受理（回 `IntentAck.error`）。
    Reject(pb::IntentError),
    /// 首页：新建对话并发出首条消息（会话层 session::start 一步步推进）。
    Start { project_id: String, text: String },
    /// 什么都不用做（如标题没变）。
    Nothing,
}

#[derive(Debug, Clone, PartialEq)]
pub enum Local {
    SetVisible(Visible),
    SetWindow {
        conversation_id: String,
        first: i64,
        last: i64,
        by_user: bool,
    },
    LoadOlder(String),
    JumpToLatest(String),
    /// 消息锚点（方案 2.9）：翻看到边缘重取、跳到一处、相对跳一处。
    AnchorBrowse {
        conversation_id: String,
        key: String,
    },
    JumpToAnchor {
        conversation_id: String,
        key: String,
    },
    JumpRelative {
        conversation_id: String,
        delta: i32,
    },
    SetFocused(bool),
    LoadMoreConversations,
    Retry,
    /// 「完全退出 iWork」确认框的结果。
    Quit {
        confirm: bool,
    },
    /// 首页选模型 / 档位（记在 Rust，新建对话时带上）。
    HomeModel(pb::ModelSelection),
    HomeMode(pb::ApprovalMode),
}

/// 选好路径以后做什么。路径只来自系统选择框（壳经 `ShellEvent::Picked` 交回），网页拿不到也给不了（R7）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Pick {
    /// 回形针：选中的文件附到这个对话。
    Attachments { conversation_id: String },
    /// 「重新定位」：选中的文件夹交给后台，由它核对 project.toml（技术设计 8.2）。
    ProjectLocation { project_id: String },
}

impl Pick {
    pub fn kind(&self) -> PickKind {
        match self {
            Pick::Attachments { .. } => PickKind::Files,
            Pick::ProjectLocation { .. } => PickKind::Folder,
        }
    }
}

/// 壳弹哪种选择框。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PickKind {
    /// 选一个或多个文件。
    Files,
    /// 选一个文件夹。
    Folder,
}

/// 壳要做的副作用（Dock 角标与跳动、打开外链、选择框、退出）。系统通知由后台发（T09 方案 2.8），客户端不发。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Effect {
    OpenExternal(String),
    /// Dock 角标：「待你处理」徽标的文字（超过 99 写「99+」）；`None` 清掉角标。只在 macOS 上落地。
    SetBadge(Option<String>),
    /// 出现了新的待处理项：让 Dock 图标跳一次（Q15：Informational）。系统只在应用不在前台时跳。
    RequestAttention,
    /// 弹系统的选择框；壳做完一定回一次 `ShellEvent::Picked`（取消或弹不出来时路径为空）。
    PickPaths(PickKind),
    /// 用户确认了「完全退出 iWork」：壳结束进程。
    Exit,
}

pub fn reject(code: &str, message: &str) -> Action {
    Action::Reject(pb::IntentError {
        code: code.into(),
        message: message.into(),
        retryable: false,
    })
}

/// 重命名标题上限（技术设计 9.0a）。
pub const TITLE_MAX_CHARS: usize = 50;

pub fn map(store: &Store, intent: pb::UiIntent) -> Action {
    let Some(kind) = intent.kind else {
        return reject("invalid_intent", "空操作");
    };
    match kind {
        I::SetVisible(v) => Action::Local(Local::SetVisible(Visible {
            sidebar: v.sidebar,
            conversation_id: v.conversation_id.filter(|s| !s.is_empty()),
            pending: v.pending,
            settings: v.settings,
            assistant_id: v.assistant_id.filter(|s| !s.is_empty()),
            add_project: v.add_project,
            conversation_list: v.conversation_list.map(|l| ListQuery {
                project_id: l.project_id,
                filter: l.filter,
            }),
            home: v.home.map(|h| h.project_id),
        })),
        I::SetVisibleRows(w) => Action::Local(Local::SetWindow {
            conversation_id: w.conversation_id,
            first: w.first_ordinal,
            last: w.last_ordinal,
            by_user: w.by_user,
        }),
        I::LoadOlder(x) => Action::Local(Local::LoadOlder(x.conversation_id)),
        I::JumpToLatest(x) => Action::Local(Local::JumpToLatest(x.conversation_id)),
        I::AnchorBrowse(x) => Action::Local(Local::AnchorBrowse {
            conversation_id: x.conversation_id,
            key: x.key,
        }),
        I::JumpToAnchor(x) => Action::Local(Local::JumpToAnchor {
            conversation_id: x.conversation_id,
            key: x.key,
        }),
        I::JumpRelative(x) if x.delta == 1 || x.delta == -1 => Action::Local(Local::JumpRelative {
            conversation_id: x.conversation_id,
            delta: x.delta,
        }),
        I::JumpRelative(_) => reject("invalid_delta", "只能往前或往后跳一处"),
        I::SetFocused(x) => Action::Local(Local::SetFocused(x.focused)),
        I::LoadMoreConversations(_) => Action::Local(Local::LoadMoreConversations),
        I::Retry(_) => Action::Local(Local::Retry),
        I::OpenExternal(x) => {
            // 只放行 http / https（R7）。
            let lower = x.url.to_ascii_lowercase();
            if lower.starts_with("https://") || lower.starts_with("http://") {
                Action::Effect(Effect::OpenExternal(x.url))
            } else {
                reject("url_not_allowed", "只能打开 http 或 https 链接")
            }
        }

        I::SendMessage(x) => Action::Command(Cmd::SendMessage(pb::SendMessage {
            conversation_id: x.conversation_id,
            text: x.text,
            attachment_ids: x.attachment_ids,
        })),
        I::CreateConversation(x) => {
            Action::Command(Cmd::CreateConversation(pb::CreateConversation {
                project_id: x.project_id,
            }))
        }
        I::RenameConversation(x) => rename(store, x),
        I::DeleteConversation(x) => {
            Action::Command(Cmd::DeleteConversation(pb::DeleteConversation {
                conversation_id: x.conversation_id,
            }))
        }
        I::CancelTurn(x) => Action::Command(Cmd::CancelTurn(pb::CancelTurn { turn_id: x.turn_id })),
        I::ResumeTurn(x) => Action::Command(Cmd::ResumeTurn(pb::ResumeTurn { turn_id: x.turn_id })),
        I::AbandonTurn(x) => {
            Action::Command(Cmd::AbandonTurn(pb::AbandonTurn { turn_id: x.turn_id }))
        }
        I::SetModel(x) => Action::Command(Cmd::SetConversationModel(pb::SetConversationModel {
            conversation_id: x.conversation_id,
            model: Some(pb::ModelSelection {
                model_id: x.model_id,
                effort: x.effort,
            }),
        })),
        I::StartConversation(x) => {
            if x.text.trim().is_empty() {
                reject("empty_message", "消息是空的")
            } else if store.summary.project(&x.project_id).is_none() {
                reject("project_not_found", "没有这个项目")
            } else {
                Action::Start {
                    project_id: x.project_id,
                    text: x.text,
                }
            }
        }
        I::SetHomeModel(x) => Action::Local(Local::HomeModel(pb::ModelSelection {
            model_id: x.model_id,
            effort: x.effort,
        })),
        I::SetHomeApprovalMode(x) => match pb::ApprovalMode::try_from(x.mode) {
            Ok(m) if m != pb::ApprovalMode::Unspecified => Action::Local(Local::HomeMode(m)),
            _ => reject("invalid_mode", "没有这个档位"),
        },
        I::SwitchDmProject(x) => Action::Command(Cmd::SwitchDmProject(pb::SwitchDmProject {
            conversation_id: x.conversation_id,
            project_id: (!x.project_id.is_empty()).then_some(x.project_id),
        })),
        I::SetApprovalMode(x) => Action::Command(Cmd::SetApprovalMode(pb::SetApprovalMode {
            target: Some(pb::ApprovalModeTarget {
                kind: Some(pb::approval_mode_target::Kind::ConversationId(
                    x.conversation_id,
                )),
            }),
            mode: x.mode,
            follow_supervisor: false,
        })),

        I::DecideApproval(x) => Action::Command(Cmd::DecideApproval(pb::DecideApproval {
            approval_id: x.approval_id,
            decision: if x.allow {
                pb::ApprovalDecision::Allow
            } else {
                pb::ApprovalDecision::Deny
            } as i32,
            remember: if x.allow { x.remember } else { 0 },
        })),
        I::ExecuteUserAction(x) => match card_hash(store, &x.card_id) {
            Some(hash) => Action::Command(Cmd::ExecuteUserAction(pb::ExecuteUserAction {
                card_id: x.card_id,
                proposal_hash: hash,
            })),
            None => reject("card_not_found", "找不到这张卡片"),
        },
        I::DismissUserAction(x) => Action::Command(Cmd::DismissUserAction(pb::DismissUserAction {
            card_id: x.card_id,
        })),
        I::StopUserAction(x) => Action::Command(Cmd::StopUserAction(pb::StopUserAction {
            card_id: x.card_id,
        })),
        I::ConfirmUserActionEnded(x) => {
            Action::Command(Cmd::ConfirmUserActionEnded(pb::ConfirmUserActionEnded {
                card_id: x.card_id,
            }))
        }
        I::ConfirmCredentialAttachment(x) => Action::Command(Cmd::ConfirmCredentialAttachment(
            pb::ConfirmCredentialAttachment {
                attachment_id: x.attachment_id,
            },
        )),
        I::DecideAgentsMdDraft(x) => {
            Action::Command(Cmd::ConfirmAgentsMdDraft(pb::ConfirmAgentsMdDraft {
                card_id: x.card_id,
                decision: if x.accept {
                    pb::DraftDecision::Accept
                } else {
                    pb::DraftDecision::Discard
                } as i32,
            }))
        }

        I::AttachPasted(x) => Action::AttachPasted(x),
        I::PickAttachments(x) => Action::Pick(Pick::Attachments {
            conversation_id: x.conversation_id,
        }),

        I::AddProject(x) => Action::Command(Cmd::AddProject(pb::AddProject { path: x.path })),
        I::PinProject(x) => Action::Command(Cmd::PinProject(pb::PinProject {
            project_id: x.project_id,
            pinned: x.pinned,
        })),
        I::RemoveProject(x) => Action::Command(Cmd::RemoveProject(pb::RemoveProject {
            project_id: x.project_id,
        })),
        I::PickProjectLocation(x) => Action::Pick(Pick::ProjectLocation {
            project_id: x.project_id,
        }),

        I::CreateAssistant(x) => Action::Command(Cmd::CreateAssistant(pb::CreateAssistant {
            config: Some(assistant_config(x)),
        })),
        I::UpdateAssistant(x) => {
            let expected_version = store.summary.assistant(&x.assistant_id).map(|a| a.version);
            match expected_version {
                Some(v) => Action::Command(Cmd::UpdateAssistant(pb::UpdateAssistant {
                    assistant_id: x.assistant_id.clone(),
                    config: Some(assistant_config(x)),
                    expected_version: v,
                })),
                None => reject("assistant_not_found", "找不到这个助手"),
            }
        }
        I::DeleteAssistant(x) => Action::Command(Cmd::DeleteAssistant(pb::DeleteAssistant {
            assistant_id: x.assistant_id,
        })),
        I::SetPlayfulUi(x) => {
            Action::Command(Cmd::SetPlayfulUi(pb::SetPlayfulUi { enabled: x.on }))
        }
        I::SetAutostart(x) => {
            Action::Command(Cmd::SetAutostart(pb::SetAutostart { enabled: x.on }))
        }
        I::RevokeRule(x) => Action::Command(Cmd::RevokeRule(pb::RevokeRule { rule_id: x.rule_id })),
        I::Quit(x) => Action::Local(Local::Quit { confirm: x.confirm }),
        I::RedetectRuntime(x) => Action::Command(Cmd::RedetectRuntime(pb::RedetectRuntime {
            runtime_id: x.runtime_id,
        })),
    }
}

/// 改名（方案 2.7、2a 第 6 条）：去掉首尾空白后 1–50 字；没变不发命令；只限能改名的对话。
fn rename(store: &Store, x: pb::RenameConversationIntent) -> Action {
    let title = x.title.trim();
    let chars = title.chars().count();
    if chars == 0 {
        return reject("title_empty", "标题不能为空");
    }
    if chars > TITLE_MAX_CHARS {
        return reject("title_too_long", "标题最多 50 个字");
    }
    if let Some(c) = store.summary.conversation(&x.conversation_id) {
        if !crate::derive::can_rename(c) {
            return reject("not_renamable", "这个对话不能重命名");
        }
        if c.title == title {
            return Action::Nothing;
        }
    }
    Action::Command(Cmd::RenameConversation(pb::RenameConversation {
        conversation_id: x.conversation_id,
        title: title.to_owned(),
    }))
}

fn card_hash(store: &Store, card_id: &str) -> Option<bytes::Bytes> {
    let from_timeline = store.open.as_ref().and_then(|t| match t.row(card_id) {
        Some(Row::UserAction(c)) => Some(c.proposal_hash.clone()),
        _ => None,
    });
    from_timeline.or_else(|| {
        store.summary.pending.iter().find_map(|p| match &p.item {
            Some(pb::pending_item::Item::UserAction(c)) if c.id == card_id => {
                Some(c.proposal_hash.clone())
            }
            _ => None,
        })
    })
}

fn assistant_config(x: pb::AssistantEditIntent) -> pb::AssistantConfig {
    pb::AssistantConfig {
        name: x.name,
        avatar: Some(pb::Avatar {
            initial: x.avatar_initial,
            color: x.avatar_color,
        }),
        prompt: x.prompt,
        runtime_id: x.runtime_id,
        model: Some(pb::ModelSelection {
            model_id: x.model_id,
            effort: x.effort,
        }),
        disabled_groups: x.disabled_groups,
    }
}
