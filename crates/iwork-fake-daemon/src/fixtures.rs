//! 造事件的小函数：场景、命令与测试共用，免得各处手写一长串 proto 字面量。
//!
//! 只造形状，不定规则：seq、created_seq、标题等由折叠（[`crate::tables`]）算。

use iwork_proto::pb;
use iwork_proto::pb::domain_event::Event as E;

/// 信封时间的起点（2025-11-01 附近，录像可复现）。
pub const BASE_TIME: i64 = 1_762_000_000;

pub const RUNTIME: &str = "rt-claude";
pub const IWORK: &str = "as-iwork";

pub fn ev(e: E) -> pb::DomainEvent {
    pb::DomainEvent { event: Some(e) }
}

pub fn ts(secs: i64) -> prost_types::Timestamp {
    prost_types::Timestamp {
        seconds: BASE_TIME + secs,
        nanos: 0,
    }
}

pub fn text(t: impl Into<String>) -> Option<pb::MessageBody> {
    Some(pb::MessageBody {
        content: Some(pb::message_body::Content::InlineText(t.into())),
    })
}

pub fn project(id: &str, name: &str, root: &str) -> pb::DomainEvent {
    ev(E::ProjectAdded(pb::ProjectAdded {
        project: Some(pb::Project {
            id: id.into(),
            root_path: root.into(),
            vcs_kind: "git".into(),
            name: name.into(),
            pinned: false,
            last_active_at: Some(ts(0)),
            path_status: pb::PathStatus::Ok.into(),
            settings: Some(pb::ProjectSettings {
                default_approval_mode: pb::ApprovalMode::Manual.into(),
                default_model: None,
                ai_review: None,
            }),
            unread: 0,
        }),
    }))
}

pub fn assistant(id: &str, name: &str, builtin: bool) -> pb::DomainEvent {
    let initial = name.chars().next().map(String::from).unwrap_or_default();
    ev(E::AssistantCreated(pb::AssistantCreated {
        assistant: Some(pb::Assistant {
            id: id.into(),
            builtin,
            version: 1,
            config: Some(pb::AssistantConfig {
                name: name.into(),
                avatar: Some(pb::Avatar {
                    initial,
                    color: "ch4".into(),
                }),
                prompt: format!("你是{name}。"),
                runtime_id: RUNTIME.into(),
                model: Some(pb::ModelSelection {
                    model_id: "fake-large".into(),
                    effort: Some("medium".into()),
                }),
                disabled_groups: vec![],
            }),
            updated_at: Some(ts(0)),
        }),
    }))
}

fn conversation(id: &str, kind: pb::conversation::Kind) -> pb::DomainEvent {
    ev(E::ConversationCreated(pb::ConversationCreated {
        conversation: Some(pb::Conversation {
            id: id.into(),
            kind: Some(kind),
            state: pb::ConversationState::Active.into(),
            ..Default::default()
        }),
    }))
}

pub fn project_conversation(id: &str, project_id: &str) -> pb::DomainEvent {
    conversation(
        id,
        pb::conversation::Kind::Project(pb::ProjectConversation {
            project_id: project_id.into(),
        }),
    )
}

pub fn dm(id: &str, assistant_id: &str) -> pb::DomainEvent {
    conversation(
        id,
        pb::conversation::Kind::AssistantDm(pb::AssistantDm {
            assistant_id: assistant_id.into(),
            context_project_id: None,
        }),
    )
}

/// 改项目名（只带 `name`，其余字段不动）。
pub fn project_renamed(id: &str, name: &str) -> pb::DomainEvent {
    ev(E::ProjectUpdated(pb::ProjectUpdated {
        project_id: id.into(),
        name: Some(name.into()),
        pinned: None,
        last_active_at: None,
    }))
}

/// 助手私聊切换上下文项目；`None` 是切回纯私聊。
pub fn dm_project_switched(conversation_id: &str, project_id: Option<&str>) -> pb::DomainEvent {
    ev(E::DmProjectSwitched(pb::DmProjectSwitched {
        conversation_id: conversation_id.into(),
        project_id: project_id.map(Into::into),
    }))
}

/// 给消息事件标上上下文项目（助手私聊里的消息记着发出时的项目，设计 9.1）。
pub fn in_project(mut e: pb::DomainEvent, project_id: &str) -> pb::DomainEvent {
    if let Some(E::MessageAppended(pb::MessageAppended { message: Some(m) })) = &mut e.event {
        m.context_project_id = Some(project_id.into());
    } else {
        panic!("in_project 只用于 MessageAppended：{e:?}");
    }
    e
}

pub fn user_message(id: &str, conversation_id: &str, body: &str) -> pb::DomainEvent {
    ev(E::MessageAppended(pb::MessageAppended {
        message: Some(pb::Message {
            id: id.into(),
            conversation_id: conversation_id.into(),
            author: Some(pb::Author {
                kind: Some(pb::author::Kind::User(pb::UserAuthor {})),
            }),
            status: pb::MessageStatus::Sent.into(),
            body: text(body),
            ..Default::default()
        }),
    }))
}

/// 助手消息开头：STREAMING、正文为空（设计 3.5）。
pub fn assistant_message(
    id: &str,
    conversation_id: &str,
    turn_id: &str,
    assistant_id: &str,
    name: &str,
) -> pb::DomainEvent {
    ev(E::MessageAppended(pb::MessageAppended {
        message: Some(pb::Message {
            id: id.into(),
            conversation_id: conversation_id.into(),
            author: Some(pb::Author {
                kind: Some(pb::author::Kind::Assistant(pb::AssistantAuthor {
                    assistant_id: assistant_id.into(),
                    name_snapshot: name.into(),
                })),
            }),
            status: pb::MessageStatus::Streaming.into(),
            body: None,
            turn_id: Some(turn_id.into()),
            ..Default::default()
        }),
    }))
}

pub fn message_completed(conversation_id: &str, message_id: &str, body: &str) -> pb::DomainEvent {
    message_ended(
        conversation_id,
        message_id,
        body,
        pb::MessageStatus::Completed,
    )
}

pub fn message_ended(
    conversation_id: &str,
    message_id: &str,
    body: &str,
    status: pb::MessageStatus,
) -> pb::DomainEvent {
    ev(E::MessageCompleted(pb::MessageCompleted {
        conversation_id: conversation_id.into(),
        message_id: message_id.into(),
        body: text(body),
        status: status.into(),
    }))
}

pub fn delta(message_id: &str, offset: u64, t: &str) -> pb::TransientEvent {
    pb::TransientEvent {
        event: Some(pb::transient_event::Event::MessageDelta(pb::MessageDelta {
            message_id: message_id.into(),
            offset,
            text: t.into(),
        })),
    }
}

/// 工具输出的一段（临时事件）。
pub fn tool_output(tool_call_id: &str, offset: u64, t: &str) -> pb::TransientEvent {
    pb::TransientEvent {
        event: Some(pb::transient_event::Event::ToolOutputDelta(
            pb::ToolOutputDelta {
                tool_call_id: tool_call_id.into(),
                offset,
                text: t.into(),
            },
        )),
    }
}

pub fn project_path_status(project_id: &str, status: pb::PathStatus) -> pb::DomainEvent {
    ev(E::ProjectPathStatusChanged(pb::ProjectPathStatusChanged {
        project_id: project_id.into(),
        path_status: status.into(),
    }))
}

pub fn project_pinned(project_id: &str, pinned: bool) -> pb::DomainEvent {
    ev(E::ProjectUpdated(pb::ProjectUpdated {
        project_id: project_id.into(),
        name: None,
        pinned: Some(pinned),
        last_active_at: None,
    }))
}

pub fn attachment(id: &str, conversation_id: &str, name: &str, size: u64) -> pb::DomainEvent {
    ev(E::AttachmentStored(pb::AttachmentStored {
        attachment: Some(pb::Attachment {
            id: id.into(),
            conversation_id: conversation_id.into(),
            sha256: "00".repeat(32),
            size,
            mime: "application/octet-stream".into(),
            name: name.into(),
            origin: None,
            needs_confirmation: false,
            created_at: Some(ts(0)),
        }),
    }))
}

/// 给 TurnQueued 换上别的模型（默认 fake-large · medium）。
pub fn with_model(mut e: pb::DomainEvent, model_id: &str) -> pb::DomainEvent {
    if let Some(E::TurnQueued(pb::TurnQueued { turn: Some(t) })) = &mut e.event {
        t.model = Some(pb::ModelSelection {
            model_id: model_id.into(),
            effort: Some("medium".into()),
        });
    } else {
        panic!("with_model 只用于 TurnQueued：{e:?}");
    }
    e
}

/// 本对话改选模型（「已切换为 X」的当次说明）。
pub fn model_changed(conversation_id: &str, model_id: &str) -> pb::DomainEvent {
    ev(E::ConversationModelChanged(pb::ConversationModelChanged {
        conversation_id: conversation_id.into(),
        model: Some(pb::ModelSelection {
            model_id: model_id.into(),
            effort: Some("medium".into()),
        }),
    }))
}

/// 本对话切档位（「审批方式已切换为…」的当次说明）。
pub fn approval_mode_changed(conversation_id: &str, mode: pb::ApprovalMode) -> pb::DomainEvent {
    ev(E::ApprovalModeChanged(pb::ApprovalModeChanged {
        target: Some(pb::ApprovalModeTarget {
            kind: Some(pb::approval_mode_target::Kind::ConversationId(
                conversation_id.into(),
            )),
        }),
        mode: Some(mode.into()),
        cause: pb::ApprovalModeChangeCause::User.into(),
    }))
}

pub fn turn_queued(id: &str, conversation_id: &str, assistant_id: &str) -> pb::DomainEvent {
    ev(E::TurnQueued(pb::TurnQueued {
        turn: Some(pb::Turn {
            id: id.into(),
            conversation_id: conversation_id.into(),
            assistant_id: assistant_id.into(),
            assistant_version: 1,
            runtime_id: RUNTIME.into(),
            model: Some(pb::ModelSelection {
                model_id: "fake-large".into(),
                effort: Some("medium".into()),
            }),
            state: pb::TurnState::Queued.into(),
            kind: pb::TurnKind::Assistant.into(),
            ..Default::default()
        }),
    }))
}

pub fn turn_state(
    id: &str,
    from: pb::TurnState,
    to: pb::TurnState,
    reason: Option<&str>,
) -> pb::DomainEvent {
    let interrupt = (to == pb::TurnState::Interrupted).then_some(pb::InterruptReason::Crash.into());
    ev(E::TurnStateChanged(pb::TurnStateChanged {
        // 所属对话由假后台追加时按折叠表填（`routes::stamp_conversation`）
        conversation_id: String::new(),
        turn_id: id.into(),
        from: from.into(),
        to: to.into(),
        reason: reason.map(Into::into),
        interrupt_reason: interrupt,
        from_interrupt_reason: None,
    }))
}

pub fn usage(input: u64, output: u64) -> pb::Usage {
    pb::Usage {
        input_tokens: input,
        output_tokens: output,
        cache_read_tokens: 0,
        cache_write_tokens: 0,
        cost_usd: None,
    }
}

pub fn turn_finished(
    id: &str,
    state: pb::TurnState,
    celebrate: bool,
    changes: Option<pb::ChangeSummary>,
) -> pb::DomainEvent {
    ev(E::TurnFinished(pb::TurnFinished {
        // 所属对话由假后台追加时按折叠表填（`routes::stamp_conversation`）
        conversation_id: String::new(),
        turn_id: id.into(),
        state: state.into(),
        usage: Some(usage(1200, 340)),
        celebrate,
        change_summary: changes,
    }))
}

pub fn changes(paths: &[&str], insertions: u32, deletions: u32) -> pb::ChangeSummary {
    pb::ChangeSummary {
        files_changed: paths.len() as u32,
        insertions,
        deletions,
        paths: paths.iter().map(|p| (*p).into()).collect(),
    }
}

pub fn tool_started(turn_id: &str, call_id: &str, tool: &str, summary: &str) -> pb::DomainEvent {
    ev(E::ToolCallStarted(pb::ToolCallStarted {
        // 所属对话由假后台追加时按折叠表填（`routes::stamp_conversation`）
        conversation_id: String::new(),
        turn_id: turn_id.into(),
        tool_call_id: call_id.into(),
        tool_name: tool.into(),
        action: Some(command_action(summary)),
    }))
}

pub fn tool_finished(turn_id: &str, call_id: &str, ok: bool) -> pb::DomainEvent {
    ev(E::ToolCallFinished(pb::ToolCallFinished {
        // 所属对话由假后台追加时按折叠表填（`routes::stamp_conversation`）
        conversation_id: String::new(),
        turn_id: turn_id.into(),
        tool_call_id: call_id.into(),
        ok,
        output_artifact: None,
    }))
}

pub fn command_action(raw: &str) -> pb::ActionSummary {
    let mut argv = raw.split_whitespace().map(String::from);
    let program = argv.next().unwrap_or_default();
    pb::ActionSummary {
        kind: pb::ActionKind::Execute.into(),
        target: Some(pb::ActionTarget {
            kind: Some(pb::action_target::Kind::Command(pb::CommandTarget {
                program: program.clone(),
                subcommand: None,
                argv: std::iter::once(program).chain(argv).collect(),
            })),
        }),
        tool_name: "Bash".into(),
        raw_summary: raw.into(),
        out_of_bounds: false,
        bypass_sandbox_requested: false,
        opaque_script: false,
        ..Default::default()
    }
}

/// 一张待定审批。`remember` 为空表示必问。
pub fn approval(
    id: &str,
    conversation_id: &str,
    turn_id: &str,
    action: pb::ActionSummary,
    kind: pb::ApprovalKind,
    remember: &[pb::RememberScope],
) -> pb::DomainEvent {
    ev(E::ApprovalRequested(pb::ApprovalRequested {
        approval: Some(pb::Approval {
            id: id.into(),
            turn_id: turn_id.into(),
            conversation_id: conversation_id.into(),
            kind: kind.into(),
            action: Some(action),
            matched_rule_ids: vec![],
            ruleset_version: "2026.11.1".into(),
            state: pb::ApprovalState::Pending.into(),
            review: pb::ReviewState::NotReviewed.into(),
            offered_remember: remember.iter().map(|r| (*r).into()).collect(),
            remember_label: if remember.is_empty() {
                String::new()
            } else {
                "本项目内同类命令不再问".into()
            },
            requested_at: Some(ts(0)),
            ..Default::default()
        }),
    }))
}

pub fn approval_decided(
    id: &str,
    state: pb::ApprovalState,
    decider: pb::ApprovalDecider,
) -> pb::DomainEvent {
    ev(E::ApprovalDecided(pb::ApprovalDecided {
        // 所属对话由假后台追加时按折叠表填（`routes::stamp_conversation`）
        conversation_id: String::new(),
        approval_id: id.into(),
        state: state.into(),
        decider: decider.into(),
        remember: None,
    }))
}

pub fn approval_voided(id: &str, reason: pb::VoidReason) -> pb::DomainEvent {
    ev(E::ApprovalVoided(pb::ApprovalVoided {
        // 所属对话由假后台追加时按折叠表填（`routes::stamp_conversation`）
        conversation_id: String::new(),
        approval_id: id.into(),
        reason: reason.into(),
    }))
}

pub fn user_action(
    id: &str,
    conversation_id: &str,
    turn_id: &str,
    command: &str,
) -> pb::DomainEvent {
    ev(E::UserActionProposed(pb::UserActionProposed {
        card: Some(pb::UserActionCard {
            id: id.into(),
            turn_id: turn_id.into(),
            conversation_id: conversation_id.into(),
            content: Some(pb::user_action_card::Content::Command(
                pb::CommandProposal {
                    command: command.into(),
                    cwd: "~/work/demo".into(),
                },
            )),
            reason: "这条命令要读你的凭据，iWork 不让助手自己执行".into(),
            proposal_hash: proposal_hash(command),
            state: pb::UserActionState::Pending.into(),
            ..Default::default()
        }),
    }))
}

/// 记住的放行规则（设置 › 审批 里列出、可撤销）。
pub fn remembered_rule(id: &str, label: &str, scope: pb::scope::Kind) -> pb::DomainEvent {
    ev(E::RuleRemembered(pb::RuleRemembered {
        rule: Some(pb::Rule {
            id: id.into(),
            layer: pb::RuleLayer::Remembered.into(),
            scope: Some(pb::Scope { kind: Some(scope) }),
            label: label.into(),
            created_at: Some(ts(0)),
            ..Default::default()
        }),
    }))
}

/// 添加项目后提议的 AGENTS.md 草稿（待你处理里的一项，没有对话归属）。
pub fn agents_md_draft(card_id: &str, project_id: &str) -> pb::DomainEvent {
    ev(E::AgentsMdDraftProposed(pb::AgentsMdDraftProposed {
        card_id: card_id.into(),
        project_id: project_id.into(),
        content_artifact: Some(pb::ArtifactRef {
            sha256: "0".repeat(64),
            area: pb::ArtifactArea::Unspecified as i32,
            size: 0,
        }),
    }))
}

/// 假的提议哈希：命令文字的字节（假后台只比相等）。
pub fn proposal_hash(command: &str) -> bytes::Bytes {
    bytes::Bytes::copy_from_slice(command.as_bytes())
}

pub fn user_action_state(
    id: &str,
    state: pb::UserActionState,
    exit_code: Option<i32>,
    error: Option<&str>,
    void: Option<pb::VoidReason>,
) -> pb::DomainEvent {
    ev(E::UserActionStateChanged(pb::UserActionStateChanged {
        // 所属对话由假后台追加时按折叠表填（`routes::stamp_conversation`）
        conversation_id: String::new(),
        card_id: id.into(),
        state: state.into(),
        exit: exit_code.map(|c| pb::ExitStatus {
            kind: Some(pb::exit_status::Kind::Code(c)),
        }),
        error: error.map(Into::into),
        output_artifact: None,
        void_reason: void.map(Into::into),
    }))
}

pub fn conversation_read(conversation_id: &str, last_read_seq: u64) -> pb::DomainEvent {
    ev(E::ConversationRead(pb::ConversationRead {
        conversation_id: conversation_id.into(),
        last_read_seq,
    }))
}

pub fn runtime(state: pb::RuntimeState) -> pb::DomainEvent {
    ev(E::RuntimeUpdated(pb::RuntimeUpdated {
        runtime: Some(pb::RuntimeStatus {
            runtime_id: RUNTIME.into(),
            kind: "claude".into(),
            display_name: "Claude Code".into(),
            cli_path: Some("/usr/local/bin/claude".into()),
            cli_version: Some("2.1.322".into()),
            adapter_version: "0.1.0".into(),
            state: state.into(),
            login: Some(pb::LoginInfo {
                logged_in: true,
                auth_method: "subscription".into(),
                subscription_type: "max".into(),
                login_expires_at: None,
            }),
        }),
    }))
}

pub fn quota(used_5h: u32, used_7d: u32) -> pb::DomainEvent {
    ev(E::QuotaObserved(pb::QuotaObserved {
        runtime_id: RUNTIME.into(),
        windows: vec![
            pb::QuotaWindow {
                window: "5h".into(),
                used_pct: used_5h,
                resets_at: Some(ts(5 * 3600)),
                observed_at: Some(ts(0)),
            },
            pb::QuotaWindow {
                window: "7d".into(),
                used_pct: used_7d,
                resets_at: Some(ts(7 * 86400)),
                observed_at: Some(ts(0)),
            },
        ],
        observed_at: Some(ts(0)),
    }))
}

pub fn settings(playful: bool) -> pb::DomainEvent {
    ev(E::GlobalSettingsChanged(pb::GlobalSettingsChanged {
        playful_ui: Some(playful),
        autostart: Some(false),
        projects_root: Some("~/iWork/projects".into()),
    }))
}
