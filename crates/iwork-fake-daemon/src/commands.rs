//! 命令受理：只受理场景要用的几种，按当前表决定追加哪些事件。其余一律回「假后台未实现」。
//!
//! 纯函数：读 [`Tables`]、返回要追加的事件（或错误）；追加、幂等记录、发布在外壳里做。

use iwork_proto::pb;
use iwork_proto::pb::command::Kind as C;
use iwork_proto::pb::domain_event::Event as E;

use crate::fixtures::{self, ev};
use crate::tables::{Tables, user_action_open};

/// 受理结果：按顺序追加的事件，以及受理后要不要跑一个脚本化回合。
#[derive(Debug, Default)]
pub struct Decision {
    pub events: Vec<pb::DomainEvent>,
    /// SendMessage 之后自动回一条助手消息（`FakeDaemon::set_auto_reply` 打开时）。
    pub reply_in: Option<String>,
}

/// `fresh` 生成新实体的 id（创建对话、用户消息）。
pub fn decide(
    tables: &Tables,
    command: &C,
    head: u64,
    fresh: &mut dyn FnMut(&str) -> String,
) -> Result<Decision, pb::Error> {
    let mut d = Decision::default();
    match command {
        C::SendMessage(m) => {
            let conv = active_conversation(tables, &m.conversation_id)?;
            let id = fresh("msg");
            let mut e = fixtures::user_message(&id, &conv.id, &m.text);
            if let Some(E::MessageAppended(a)) = &mut e.event
                && let Some(msg) = &mut a.message
            {
                msg.attachment_ids = m.attachment_ids.clone();
            }
            d.events.push(e);
            d.reply_in = Some(conv.id.clone());
        }
        C::MarkRead(m) => {
            let conv = conversation(tables, &m.conversation_id)?;
            let up_to = if m.up_to_seq == 0 {
                head
            } else {
                m.up_to_seq.min(head)
            };
            if up_to > conv.last_read_seq {
                d.events.push(fixtures::conversation_read(&conv.id, up_to));
            }
        }
        C::CreateConversation(c) => {
            if !tables.projects.contains_key(&c.project_id) {
                return Err(not_found("project_id", &c.project_id));
            }
            d.events.push(fixtures::project_conversation(
                &fresh("conv"),
                &c.project_id,
            ));
        }
        C::SetConversationModel(m) => {
            let conv = active_conversation(tables, &m.conversation_id)?;
            d.events.push(ev(E::ConversationModelChanged(
                pb::ConversationModelChanged {
                    conversation_id: conv.id.clone(),
                    model: m.model.clone(),
                },
            )));
        }
        // 只做对话级（首页新建对话时带上档位）；项目默认档位归 T10。
        C::SetApprovalMode(m) => {
            let Some(pb::approval_mode_target::Kind::ConversationId(id)) =
                m.target.as_ref().and_then(|t| t.kind.as_ref())
            else {
                return Err(error(
                    pb::ErrorCode::Unavailable,
                    "假后台只支持对话级档位",
                    false,
                ));
            };
            active_conversation(tables, id)?;
            d.events
                .push(ev(E::ApprovalModeChanged(pb::ApprovalModeChanged {
                    target: m.target.clone(),
                    mode: Some(m.mode),
                    cause: pb::ApprovalModeChangeCause::User.into(),
                })));
        }
        C::DecideApproval(a) => {
            let approval = tables
                .approvals
                .get(&a.approval_id)
                .ok_or_else(|| not_found("approval_id", &a.approval_id))?;
            if approval.state != i32::from(pb::ApprovalState::Pending) {
                return Err(error(
                    pb::ErrorCode::ApprovalNotPending,
                    "这张审批卡片已经不能处理了",
                    false,
                ));
            }
            let allow = a.decision == i32::from(pb::ApprovalDecision::Allow);
            d.events.push(ev(E::ApprovalDecided(pb::ApprovalDecided {
                approval_id: a.approval_id.clone(),
                state: if allow {
                    pb::ApprovalState::Approved
                } else {
                    pb::ApprovalState::Denied
                }
                .into(),
                decider: pb::ApprovalDecider::User.into(),
                remember: allow.then_some(a.remember).filter(|r| *r != 0),
            })));
        }
        C::DeleteConversation(x) => {
            let conv = conversation(tables, &x.conversation_id)?;
            if conv
                .kind
                .as_ref()
                .is_some_and(|k| matches!(k, pb::conversation::Kind::AssistantDm(_)))
            {
                return Err(error(
                    pb::ErrorCode::PreconditionFailed,
                    "助手私聊不能删除",
                    false,
                ));
            }
            d.events.extend(delete_events(tables, &conv.id));
        }
        C::ExecuteUserAction(x) => {
            let card = open_card(tables, &x.card_id, pb::UserActionState::Pending)?;
            if card.proposal_hash != x.proposal_hash {
                return Err(error(
                    pb::ErrorCode::UserActionHashMismatch,
                    "卡片内容已经变了，请重新确认",
                    false,
                ));
            }
            use pb::UserActionState as S;
            d.events.push(fixtures::user_action_state(
                &x.card_id,
                S::Executing,
                None,
                None,
                None,
            ));
            d.events.push(fixtures::user_action_state(
                &x.card_id,
                S::Done,
                Some(0),
                None,
                None,
            ));
        }
        C::DismissUserAction(x) => {
            open_card(tables, &x.card_id, pb::UserActionState::Pending)?;
            d.events.push(fixtures::user_action_state(
                &x.card_id,
                pb::UserActionState::Dismissed,
                None,
                None,
                None,
            ));
        }
        C::ConfirmUserActionEnded(x) => {
            let card = open_card(tables, &x.card_id, pb::UserActionState::Unknown)?;
            d.events.push(fixtures::user_action_state(
                &x.card_id,
                pb::UserActionState::Ended,
                None,
                None,
                None,
            ));
            // 技术设计 7.2a：删除对话停在「删除中」等这张卡片核对；核对完、没有别的未知卡片就把删除做完。
            let deleting = tables
                .conversations
                .get(&card.conversation_id)
                .is_some_and(|r| {
                    r.conversation.state == i32::from(pb::ConversationState::Deleting)
                });
            let others_unknown = tables.user_actions.values().any(|c| {
                c.conversation_id == card.conversation_id
                    && c.id != card.id
                    && c.state == i32::from(pb::UserActionState::Unknown)
            });
            if deleting && !others_unknown {
                d.events
                    .push(ev(E::ConversationDeleted(pb::ConversationDeleted {
                        conversation_id: card.conversation_id.clone(),
                    })));
            }
        }
        // 本机文件附件：只认路径来源（粘贴的分片上传假后台不做）。不读文件，大小记 0；凭据、私钥判定归真后台。
        C::UploadAttachment(x) => {
            let Some(pb::upload_attachment::Source::LocalPath(_)) = &x.source else {
                return Err(unimplemented());
            };
            let conv = active_conversation(tables, &x.conversation_id)?;
            d.events
                .push(fixtures::attachment(&fresh("att"), &conv.id, &x.name, 0));
        }
        // 重新定位：真后台要读新位置的 project.toml、项目 ID 一致才关联（技术设计 8.2）；假后台只认项目在不在。
        C::RelocateProject(x) => {
            if !tables.projects.contains_key(&x.project_id) {
                return Err(not_found("project_id", &x.project_id));
            }
            d.events.push(ev(E::ProjectRelocated(pb::ProjectRelocated {
                project_id: x.project_id.clone(),
                new_path: x.new_path.clone(),
            })));
        }
        // 添加项目（步 7d）：真后台要检查目录、写 project.toml（技术设计 8.2）；假后台按路径的最后一段起名。
        C::AddProject(x) => {
            if tables.projects.values().any(|p| p.root_path == x.path) {
                return Err(error(
                    pb::ErrorCode::InvalidArgument,
                    "这个文件夹已经是项目了",
                    false,
                ));
            }
            let name = x
                .path
                .trim_end_matches('/')
                .rsplit('/')
                .next()
                .unwrap_or_default();
            d.events
                .push(fixtures::project(&fresh("project"), name, &x.path));
        }
        // 设置（步 7d）：只带变了的字段
        C::SetPlayfulUi(x) => {
            d.events
                .push(ev(E::GlobalSettingsChanged(pb::GlobalSettingsChanged {
                    playful_ui: Some(x.enabled),
                    ..Default::default()
                })))
        }
        C::SetAutostart(x) => {
            d.events
                .push(ev(E::GlobalSettingsChanged(pb::GlobalSettingsChanged {
                    autostart: Some(x.enabled),
                    ..Default::default()
                })))
        }
        C::SetProjectsRoot(x) => {
            d.events
                .push(ev(E::GlobalSettingsChanged(pb::GlobalSettingsChanged {
                    projects_root: Some(x.path.clone()),
                    ..Default::default()
                })))
        }
        C::RevokeRule(x) => {
            if !tables.rules.contains_key(&x.rule_id) {
                return Err(not_found("rule_id", &x.rule_id));
            }
            d.events.push(ev(E::RuleRevoked(pb::RuleRevoked {
                rule_id: x.rule_id.clone(),
            })));
        }
        // 助手（步 7c 的配置页、新建弹窗、删除）：乐观并发照协议（版本对不上回 CONFLICT），名字为空回
        // INVALID_ARGUMENT，配置没变不产生事件；新建同时建它的私聊；内置 iWork 不能删。
        C::CreateAssistant(x) => {
            let config = x.config.clone().unwrap_or_default();
            check_assistant_name(&config)?;
            let id = fresh("assistant");
            d.events.push(ev(E::AssistantCreated(pb::AssistantCreated {
                assistant: Some(pb::Assistant {
                    id: id.clone(),
                    builtin: false,
                    version: 1,
                    config: Some(config),
                    updated_at: None,
                }),
            })));
            d.events.push(fixtures::dm(&fresh("dm"), &id));
        }
        C::UpdateAssistant(x) => {
            let a = tables
                .assistants
                .get(&x.assistant_id)
                .ok_or_else(|| not_found("assistant_id", &x.assistant_id))?;
            if a.version != x.expected_version {
                return Err(error(
                    pb::ErrorCode::Conflict,
                    "助手配置已经被改过，请按最新的再改",
                    false,
                ));
            }
            let config = x.config.clone().unwrap_or_default();
            check_assistant_name(&config)?;
            if a.config.as_ref() != Some(&config) {
                d.events.push(ev(E::AssistantUpdated(pb::AssistantUpdated {
                    assistant_id: a.id.clone(),
                    version: a.version + 1,
                    config: Some(config),
                })));
            }
        }
        C::DeleteAssistant(x) => {
            let a = tables
                .assistants
                .get(&x.assistant_id)
                .ok_or_else(|| not_found("assistant_id", &x.assistant_id))?;
            if a.builtin {
                return Err(error(
                    pb::ErrorCode::InvalidArgument,
                    "iWork 不能删除",
                    false,
                ));
            }
            d.events.push(ev(E::AssistantDeleted(pb::AssistantDeleted {
                assistant_id: a.id.clone(),
                name_snapshot: a
                    .config
                    .as_ref()
                    .map(|c| c.name.clone())
                    .unwrap_or_default(),
            })));
        }
        _ => return Err(unimplemented()),
    }
    Ok(d)
}

fn check_assistant_name(c: &pb::AssistantConfig) -> Result<(), pb::Error> {
    if c.name.trim().is_empty() {
        return Err(error(pb::ErrorCode::InvalidArgument, "名字不能为空", false));
    }
    Ok(())
}

fn unimplemented() -> pb::Error {
    pb::Error {
        code: pb::ErrorCode::Unavailable.into(),
        message: "假后台未实现".into(),
        retryable: false,
        details: [("fake".to_owned(), "unimplemented".to_owned())].into(),
    }
}

/// 删除对话（技术设计 9.0）：先进「删除中」、作废待定审批与卡片；有结果未知的卡片就停在删除中。
fn delete_events(tables: &Tables, conversation_id: &str) -> Vec<pb::DomainEvent> {
    let mut out = vec![ev(E::ConversationDeleting(pb::ConversationDeleting {
        conversation_id: conversation_id.into(),
    }))];
    for a in tables.approvals.values() {
        if a.conversation_id == conversation_id && a.state == i32::from(pb::ApprovalState::Pending)
        {
            out.push(fixtures::approval_voided(
                &a.id,
                pb::VoidReason::ConversationDeleted,
            ));
        }
    }
    let mut unknown = false;
    for c in tables.user_actions.values() {
        if c.conversation_id != conversation_id || !user_action_open(c.state) {
            continue;
        }
        if c.state == i32::from(pb::UserActionState::Unknown) {
            unknown = true;
        } else {
            out.push(fixtures::user_action_state(
                &c.id,
                pb::UserActionState::Voided,
                None,
                None,
                Some(pb::VoidReason::ConversationDeleted),
            ));
        }
    }
    if !unknown {
        out.push(ev(E::ConversationDeleted(pb::ConversationDeleted {
            conversation_id: conversation_id.into(),
        })));
    }
    out
}

fn conversation<'a>(tables: &'a Tables, id: &str) -> Result<&'a pb::Conversation, pb::Error> {
    tables
        .conversations
        .get(id)
        .map(|r| &r.conversation)
        .ok_or_else(|| not_found("conversation_id", id))
}

fn active_conversation<'a>(
    tables: &'a Tables,
    id: &str,
) -> Result<&'a pb::Conversation, pb::Error> {
    let c = conversation(tables, id)?;
    if c.state == i32::from(pb::ConversationState::Deleting) {
        return Err(error(
            pb::ErrorCode::ConversationDeleting,
            "对话正在删除",
            false,
        ));
    }
    Ok(c)
}

fn open_card<'a>(
    tables: &'a Tables,
    id: &str,
    want: pb::UserActionState,
) -> Result<&'a pb::UserActionCard, pb::Error> {
    let card = tables
        .user_actions
        .get(id)
        .ok_or_else(|| not_found("card_id", id))?;
    if card.state != i32::from(want) {
        return Err(error(
            pb::ErrorCode::UserActionNotPending,
            "这张卡片已经不能这样处理了",
            false,
        ));
    }
    Ok(card)
}

fn not_found(field: &str, id: &str) -> pb::Error {
    pb::Error {
        code: pb::ErrorCode::NotFound.into(),
        message: format!("找不到 {field} = {id}"),
        retryable: false,
        details: [(field.to_owned(), id.to_owned())].into(),
    }
}

fn error(code: pb::ErrorCode, message: &str, retryable: bool) -> pb::Error {
    pb::Error {
        code: code.into(),
        message: message.into(),
        retryable,
        details: Default::default(),
    }
}
