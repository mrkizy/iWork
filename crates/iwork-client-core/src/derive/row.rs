//! 时间线一行 → `RowView`（行登记表的每一种行一臂）。
//!
//! **行外读取与标脏**：一行的视图模型读了行外的状态，那份状态变了这一行就得重发，否则网页停在旧值上。
//! 下面是这里全部的行外读取，每一处都有对应的标脏方（`tests/row_dependencies.rs` 逐行一条用例）；
//! 新加一处行外读取，就在这张表加一行、在那里加一条用例：
//!
//! | 读的什么 | 用在哪 | 标脏方 |
//! | --- | --- | --- |
//! | 连接状态（`commands_enabled`） | 审批、亲手执行、继续卡片的按钮 | `Store::set_connection` |
//! | 回合状态与原因（`tl.turns`） | 消息表情、失败说明行 | 时间线 `TurnStateChanged` / `TurnFinished`（`messages_of_turn`） |
//! | 同回合消息的 status（`has_failed_message`） | 回合结束行要不要说原因 | 时间线 `MessageCompleted`（`end:<turn_id>`） |
//! | 对话是否在删除中（`tl.conversation.state`） | 亲手执行卡片的说明 | 时间线 `ConversationDeleting`（全部亲手执行卡片行） |
//! | 助手表（`avatar`） | 消息头像 | `Store` 应用 `ListAssistants`（内容变了才标） |
//! | 项目表（`project_name`） | 分隔线、当次说明 | `Store` 应用 `ListProjects`（内容变了才标） |
//! | 附件表（`tl.attachments`） | 消息的附件 | 时间线 `AttachmentStored` / `AttachmentConfirmed` |
//! | 流式正文、工具输出（`tl.text`） | 各自本行 | 增量事件本身（追加或整行） |

use iwork_proto::pb;
use pb::row_view::Row as V;

use super::{avatar, commands_enabled, expression_of_turn, text};
use crate::store::Store;
use crate::store::timeline::{NoteText, Row, RowKey, Timeline, tool_identity};

pub fn row(store: &Store, tl: &Timeline, key: RowKey, identity: &str, row: &Row) -> pb::RowView {
    let live = commands_enabled(store);
    let v = match row {
        Row::Message(m) => V::Message(message(store, tl, m)),
        Row::Tool(t) => V::Tool(tool(tl, t)),
        Row::Approval(a) => V::Approval(approval(a, live)),
        Row::UserAction(c) => V::UserAction(user_action(tl, c, live)),
        Row::TurnEnd(t) => V::TurnEnd(pb::TurnEndRow {
            turn_id: t.id.clone(),
            celebrate: t.celebrate,
            summary_line: t
                .change_summary
                .as_ref()
                .filter(|c| c.files_changed > 0)
                .map(text::change_summary)
                .unwrap_or_default(),
            paths: t
                .change_summary
                .as_ref()
                .map(|c| c.paths.clone())
                .unwrap_or_default(),
            // 原因已经挂在失败的助手消息上时（2a 第 10 条），回合结束行不再重复。
            outcome_label: if t.state == pb::TurnState::Failed as i32
                && has_failed_message(tl, &t.id)
            {
                String::new()
            } else {
                text::turn_outcome(t)
            },
            failed: t.state == pb::TurnState::Failed as i32,
        }),
        Row::Resume(r) => V::Resume(pb::ResumeRow {
            turn_id: r.turn_id.clone(),
            reason: text::interrupt_reason(
                pb::InterruptReason::try_from(r.reason).unwrap_or_default(),
            )
            .into(),
            can_resume: live,
            can_abandon: live,
        }),
        Row::ModelSwitch { model } => V::ModelSwitch(pb::NoteRow {
            text: text::switched_model(model),
        }),
        Row::Divider { project_id } => V::Divider(pb::NoteRow {
            text: text::switched_project(project_name(store, project_id.as_deref()).as_deref()),
        }),
        Row::Note { text: t, .. } => V::Note(pb::NoteRow {
            text: match t {
                NoteText::ApprovalMode(m) => {
                    text::switched_mode(m.and_then(|m| pb::ApprovalMode::try_from(m).ok()))
                }
                NoteText::Model(m) => text::switched_model(m),
                NoteText::DmProject(p) => {
                    text::switched_project(project_name(store, p.as_deref()).as_deref())
                }
            },
        }),
    };
    pb::RowView {
        ordinal: key.ordinal(),
        key: identity.to_owned(),
        row: Some(v),
    }
}

/// 这个回合有一条失败的助手消息：失败原因已经挂在那条消息的说明行上（ui-m1.md 2a 第 10 条）。
fn has_failed_message(tl: &Timeline, turn_id: &str) -> bool {
    !tl.identities_where(|r| {
        matches!(r, Row::Message(m) if m.turn_id.as_deref() == Some(turn_id)
            && m.status == pb::MessageStatus::Failed as i32)
    })
    .is_empty()
}

fn project_name(store: &Store, id: Option<&str>) -> Option<String> {
    let id = id?;
    Some(
        store
            .summary
            .project(id)
            .map_or_else(|| id.to_owned(), |p| p.name.clone()),
    )
}

fn message(store: &Store, tl: &Timeline, m: &pb::Message) -> pb::MessageRow {
    use pb::author::Kind as A;
    let (speaker, name, av, expression) = match m.author.as_ref().and_then(|a| a.kind.as_ref()) {
        Some(A::Assistant(a)) => {
            let state = m
                .turn_id
                .as_deref()
                .and_then(|t| tl.turns.get(t))
                .map(|t| t.state);
            (
                pb::Speaker::Assistant,
                a.name_snapshot.clone(),
                Some(avatar(store, &a.assistant_id)),
                expression_of_turn(state),
            )
        }
        Some(A::System(_)) => (
            pb::Speaker::System,
            text::SYSTEM_NAME.to_owned(),
            None,
            pb::Expression::Unspecified,
        ),
        Some(A::User(_)) | None => (
            pb::Speaker::User,
            text::USER_NAME.to_owned(),
            None,
            pb::Expression::Unspecified,
        ),
    };
    let status = pb::MessageStatus::try_from(m.status).unwrap_or_default();
    let body_is_artifact = matches!(
        m.body.as_ref().and_then(|b| b.content.as_ref()),
        Some(pb::message_body::Content::Artifact(_))
    );
    let text_now = tl.text(&m.id).unwrap_or_default().to_owned();
    // 这一轮失败（ui-m1.md 2a 第 10 条）：内容为空 → 不画空气泡，只画「没能开始」；有内容 → 末尾加「中途失败」。
    let failed = status == pb::MessageStatus::Failed;
    let bubble_hidden = failed && text_now.is_empty();
    let failure = failed.then(|| {
        let turn = m.turn_id.as_deref().and_then(|t| tl.turns.get(t));
        text::turn_failure(text_now.is_empty(), turn)
    });
    pb::MessageRow {
        message_id: m.id.clone(),
        speaker: speaker.into(),
        author_name: name,
        avatar: av,
        expression: expression.into(),
        body_pending: body_is_artifact && text_now.is_empty(),
        text: text_now,
        streaming: status == pb::MessageStatus::Streaming,
        // 失败由说明行讲清楚，不再另挂「失败」标签。
        status_label: if failed {
            String::new()
        } else {
            text::message_status(status).into()
        },
        bubble_hidden,
        failure,
        attachments: m
            .attachment_ids
            .iter()
            .map(|id| {
                let a = tl.attachments.get(id);
                pb::AttachmentChip {
                    attachment_id: id.clone(),
                    name: a.map(|a| a.name.clone()).unwrap_or_default(),
                    size_label: a.map(|a| text::size_label(a.size)).unwrap_or_default(),
                    needs_confirmation: a.is_some_and(|a| a.needs_confirmation),
                }
            })
            .collect(),
    }
}

fn tool(tl: &Timeline, t: &pb::ToolCall) -> pb::ToolRow {
    let failed = t.finished && !t.ok;
    pb::ToolRow {
        tool_call_id: t.tool_call_id.clone(),
        title: t.tool_name.clone(),
        summary: t
            .action
            .as_ref()
            .map(|a| a.raw_summary.clone())
            .unwrap_or_default(),
        running: !t.finished,
        failed,
        status_label: if !t.finished {
            "运行中".into()
        } else if failed {
            "失败".into()
        } else {
            String::new()
        },
        output: tl
            .text(&tool_identity(&t.turn_id, &t.tool_call_id))
            .unwrap_or_default()
            .to_owned(),
    }
}

pub(crate) fn approval(a: &pb::Approval, live: bool) -> pb::ApprovalRow {
    let action = a.action.clone().unwrap_or_default();
    let pending = a.state == pb::ApprovalState::Pending as i32;
    let outside = a.kind == pb::ApprovalKind::OutsideSandbox as i32;
    let mut tags = Vec::new();
    if action.out_of_bounds {
        tags.push("越界".to_owned());
    }
    if outside || action.bypass_sandbox_requested {
        tags.push("沙箱外".to_owned());
    }
    let mut notes = Vec::new();
    if outside {
        notes.push(text::OUTSIDE_SANDBOX_NOTE.to_owned());
    }
    if action.opaque_script {
        notes.push(text::OPAQUE_SCRIPT_NOTE.to_owned());
    }
    if pending && !a.remember_label.is_empty() && !a.offered_remember.is_empty() {
        notes.push(a.remember_label.clone());
    }
    let decided_by_switch = a.state == pb::ApprovalState::Approved as i32
        && a.decider == Some(pb::ApprovalDecider::ModeSwitch as i32);
    pb::ApprovalRow {
        approval_id: a.id.clone(),
        title: if outside {
            "要在沙箱外运行".into()
        } else {
            "需要你批准".into()
        },
        tags,
        code: action.raw_summary,
        review_line: text::review_line(
            pb::ReviewState::try_from(a.review).unwrap_or_default(),
            a.review_reason.as_deref(),
        ),
        state_label: text::approval_state(a),
        can_decide: pending && live,
        // 只列后台给的 offered_remember（必问与清单外沙箱外执行时为空）。
        remember_options: if pending {
            a.offered_remember
                .iter()
                .map(|s| pb::OptionView {
                    value: *s,
                    label: text::remember_scope(
                        pb::RememberScope::try_from(*s).unwrap_or_default(),
                    )
                    .into(),
                    ..Default::default()
                })
                .collect()
        } else {
            Vec::new()
        },
        muted: a.state == pb::ApprovalState::Voided as i32,
        folded: decided_by_switch,
        notes,
    }
}

fn user_action(tl: &Timeline, c: &pb::UserActionCard, live: bool) -> pb::UserActionRow {
    use pb::UserActionState as S;
    let state = S::try_from(c.state).unwrap_or_default();
    let (command, cwd, patch_path, diff) = match &c.content {
        Some(pb::user_action_card::Content::Command(x)) => (
            x.command.clone(),
            x.cwd.clone(),
            String::new(),
            String::new(),
        ),
        Some(pb::user_action_card::Content::Patch(x)) => (
            String::new(),
            String::new(),
            x.path.clone(),
            patch_diff(&x.old_text, &x.new_text),
        ),
        None => Default::default(),
    };
    let deleting = tl
        .conversation
        .as_ref()
        .is_some_and(|conv| conv.state == pb::ConversationState::Deleting as i32);
    let notice = (state == S::Unknown).then(|| {
        if deleting {
            text::UNKNOWN_BLOCKS_DELETE.to_owned()
        } else {
            text::UNKNOWN_RESULT.to_owned()
        }
    });
    pb::UserActionRow {
        card_id: c.id.clone(),
        can_copy: !command.is_empty(),
        command,
        cwd,
        patch_path,
        diff,
        reason: c.reason.clone(),
        state_label: text::user_action_state(c),
        can_execute: state == S::Pending && live,
        can_dismiss: state == S::Pending && live,
        can_stop: state == S::Executing && live,
        can_confirm_ended: state == S::Unknown && live,
        exit_label: c.exit.as_ref().and_then(text::exit_label),
        output: tl
            .text(&format!("output:{}", c.id))
            .unwrap_or_default()
            .to_owned(),
        muted: state == S::Voided,
        notice,
    }
}

/// 单文件补丁的 diff：旧行加 `-`、新行加 `+`（只显示，不用于执行；执行用的是后台存的提议）。
fn patch_diff(old: &str, new: &str) -> String {
    let mut out = String::new();
    for l in old.lines() {
        out.push_str("- ");
        out.push_str(l);
        out.push('\n');
    }
    for l in new.lines() {
        out.push_str("+ ");
        out.push_str(l);
        out.push('\n');
    }
    out
}
