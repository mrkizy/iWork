//! 事件 → 路由键。
//!
//! 为什么不直接调 `iwork_app::subscription::routes_for`：它吃领域事件，而且回合、工具调用、审批决定、卡片状态
//! 这几类事件的载荷里还没有 `conversation_id`，现在只走主流（T09 方案第 3 节 G5，归 T10）。假后台按 G5 落地后的
//! 路由发：这些事件再加一个 `conversation:<id>`，对话 id 从折叠表里查。主键与 `routes_for` 第一个键相同，
//! 由 `tests/routes_parity.rs` 对拍；G5 落地后假后台改为直接调 `routes_for`，删掉这张表。

use iwork_proto::pb;
use iwork_proto::pb::domain_event::Event as E;
use iwork_proto::routing::StreamKey;

use crate::tables::Tables;

const PROJECT: &str = "project";
const ASSISTANT: &str = "assistant";
const CONVERSATION: &str = "conversation";
const TURN: &str = "turn";
const APPROVAL: &str = "approval";
const AUDIT: &str = "audit";
pub(crate) const RULE: &str = "rule";
const RUNTIME: &str = "runtime";
const SETTINGS: &str = "settings";
const SYSTEM: &str = "system";

/// 事件所属的主流。穷举 `match`：proto 加事件这里编译不过。与领域 `DomainEvent::stream` 逐条对应。
pub fn primary(event: &E) -> StreamKey {
    let of = |o: Option<String>| o.unwrap_or_default();
    match event {
        E::ProjectAdded(e) => one(PROJECT, of(e.project.as_ref().map(|p| p.id.clone()))),
        E::ProjectUpdated(e) => one(PROJECT, &e.project_id),
        E::ProjectRelocated(e) => one(PROJECT, &e.project_id),
        E::ProjectPathStatusChanged(e) => one(PROJECT, &e.project_id),
        E::ProjectRemoved(e) => one(PROJECT, &e.project_id),
        E::ProjectSettingsUpdated(e) => one(PROJECT, &e.project_id),
        E::RepositoryInitialized(e) => one(PROJECT, &e.path),
        E::CloneFinished(e) => one(PROJECT, &e.job_id),
        E::AgentsMdDraftProposed(e) => one(PROJECT, &e.project_id),
        E::AgentsMdDraftDecided(e) => one(PROJECT, &e.project_id),
        E::AssistantCreated(e) => one(ASSISTANT, of(e.assistant.as_ref().map(|a| a.id.clone()))),
        E::AssistantUpdated(e) => one(ASSISTANT, &e.assistant_id),
        E::AssistantDeleted(e) => one(ASSISTANT, &e.assistant_id),
        E::AssistantReset(e) => one(ASSISTANT, &e.assistant_id),
        E::ConversationCreated(e) => one(
            CONVERSATION,
            of(e.conversation.as_ref().map(|c| c.id.clone())),
        ),
        E::ConversationDeleting(e) => one(CONVERSATION, &e.conversation_id),
        E::ConversationDeleted(e) => one(CONVERSATION, &e.conversation_id),
        E::ConversationModelChanged(e) => one(CONVERSATION, &e.conversation_id),
        E::ApprovalModeChanged(e) => match e.target.as_ref().and_then(|t| t.kind.as_ref()) {
            Some(pb::approval_mode_target::Kind::ConversationId(id)) => one(CONVERSATION, id),
            Some(pb::approval_mode_target::Kind::Member(m)) => {
                one(CONVERSATION, &m.conversation_id)
            }
            Some(pb::approval_mode_target::Kind::ProjectId(id)) => one(PROJECT, id),
            None => one(CONVERSATION, ""),
        },
        E::DmProjectSwitched(e) => one(CONVERSATION, &e.conversation_id),
        E::ConversationRead(e) => one(CONVERSATION, &e.conversation_id),
        E::MessageAppended(e) => one(
            CONVERSATION,
            of(e.message.as_ref().map(|m| m.conversation_id.clone())),
        ),
        E::MessageCompleted(e) => one(CONVERSATION, &e.conversation_id),
        E::TurnQueued(e) => one(TURN, of(e.turn.as_ref().map(|t| t.id.clone()))),
        E::TurnStateChanged(e) => one(TURN, &e.turn_id),
        E::TurnFinished(e) => one(TURN, &e.turn_id),
        E::TurnModelReported(e) => one(TURN, &e.turn_id),
        E::TurnSessionBound(e) => one(TURN, &e.turn_id),
        E::ApprovalDelivered(e) => one(TURN, &e.turn_id),
        E::TurnIntentRecorded(e) => one(TURN, &e.turn_id),
        E::ToolCallStarted(e) => one(TURN, &e.turn_id),
        E::ToolCallFinished(e) => one(TURN, &e.turn_id),
        E::ApprovalRequested(e) => one(APPROVAL, of(e.approval.as_ref().map(|a| a.id.clone()))),
        E::ApprovalReviewUpdated(e) => one(APPROVAL, &e.approval_id),
        E::ApprovalDecided(e) => one(APPROVAL, &e.approval_id),
        E::ApprovalVoided(e) => one(APPROVAL, &e.approval_id),
        E::ApprovalEvaluated(e) => one(AUDIT, &e.turn_id),
        E::RuleRemembered(e) => one(RULE, of(e.rule.as_ref().map(|r| r.id.clone()))),
        E::RuleRevoked(e) => one(RULE, &e.rule_id),
        E::RulesDeleted(_) => StreamKey::all(RULE),
        E::ApprovalStaleResultDropped(e) => one(AUDIT, &e.approval_id),
        E::UserActionProposed(e) => one(APPROVAL, of(e.card.as_ref().map(|c| c.id.clone()))),
        E::UserActionStateChanged(e) => one(APPROVAL, &e.card_id),
        E::SecurityListEntryAdded(e) => one(RULE, of(e.rule.as_ref().map(|r| r.id.clone()))),
        E::SecurityListEntryRemoved(e) => one(RULE, &e.entry_id),
        E::AttachmentStored(e) => one(
            CONVERSATION,
            of(e.attachment.as_ref().map(|a| a.conversation_id.clone())),
        ),
        E::AttachmentConfirmed(e) => one(CONVERSATION, &e.conversation_id),
        E::RuntimeUpdated(e) => one(
            RUNTIME,
            of(e.runtime.as_ref().map(|r| r.runtime_id.clone())),
        ),
        E::QuotaObserved(e) => one(RUNTIME, &e.runtime_id),
        E::UsageReported(e) => one(RUNTIME, &e.runtime_id),
        E::RuntimeSessionForgetFailed(e) => one(AUDIT, &e.conversation_id),
        E::LocalImportPrompted(e) => one(RUNTIME, &e.runtime_id),
        E::LocalConfigImported(e) => one(RUNTIME, &e.runtime_id),
        E::McpSecretSet(e) => one(RUNTIME, &e.mcp_server_id),
        E::GlobalSettingsChanged(_) => StreamKey::all(SETTINGS),
        E::RecoveryCompleted(_) => StreamKey::all(SYSTEM),
    }
}

fn one(kind: &'static str, id: impl Into<String>) -> StreamKey {
    StreamKey::one(kind, id)
}

/// 全部路由：主流，加上（主流不是会话时）它所属对话的 `conversation:<id>`。要在折叠这条事件之前算
/// （删除对话的事件折叠后就查不到归属了）。
pub fn routes(tables: &Tables, event: &E) -> Vec<StreamKey> {
    let main = primary(event);
    let mut routes = vec![main.clone()];
    if main.kind != CONVERSATION
        && let Some(conv) = tables.conversation_of(event)
    {
        routes.push(StreamKey::one(CONVERSATION, conv));
    }
    routes
}

/// 流式增量的路由：消息增量落在消息所在对话；工具输出落在工具调用所在对话；克隆进度落在任务的项目键。
pub fn transient_routes(tables: &Tables, event: &pb::TransientEvent) -> Vec<StreamKey> {
    use pb::transient_event::Event as T;
    match &event.event {
        Some(T::MessageDelta(d)) => tables
            .message_conversation
            .get(&d.message_id)
            .map(|c| vec![StreamKey::one(CONVERSATION, c.clone())])
            .unwrap_or_default(),
        Some(T::ToolOutputDelta(d)) => tables
            .tool_calls
            .values()
            .find(|c| c.tool_call_id == d.tool_call_id)
            .and_then(|c| tables.turns.get(&c.turn_id))
            .map(|t| vec![StreamKey::one(CONVERSATION, t.conversation_id.clone())])
            .unwrap_or_default(),
        Some(T::CloneProgress(p)) => vec![StreamKey::one(PROJECT, p.job_id.clone())],
        Some(T::SubscriptionLagging(_)) | None => Vec::new(),
    }
}
