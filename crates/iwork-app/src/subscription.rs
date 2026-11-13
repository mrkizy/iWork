//! 订阅范围与事件路由（T03 方案 2.8「范围匹配事件」「快照的组成」）。
//!
//! 三件事的真值都在这里，`iwork-ipc` 只拿结果做 [`StreamKey::covers`] 匹配，不解释键的含义：
//!
//! - [`scope_streams`]：订阅范围 → 它关心的路由键；
//! - [`snapshot_queries`]：订阅范围 → 快照由哪些查询组成（执行归 T10 的查询受理）；
//! - [`routes_for`]：领域事件 → 它落在哪些路由键上。历史补发与实时推送都调它，两条路径算出的路由
//!   因此一致。
//!
//! 键的 `kind` 一律由 [`stream_kind_name`] 从领域的 [`StreamKind`] 得出，这里不手写类名字符串。

use iwork_domain::event::{DomainEvent, StreamKind};
use iwork_domain::id::ConversationId;
use iwork_proto::pb;
use iwork_proto::pb::query::Kind as Q;
use iwork_proto::pb::subscription_scope::Kind as Scope;
use iwork_proto::routing::StreamKey;

/// 会话范围快照取的最新一页消息条数。更早的消息由客户端翻页查询（`before_message_id`）。
pub const CONVERSATION_SNAPSHOT_PAGE_SIZE: u32 = 50;

/// [`StreamKind`] 的蛇形名，即路由键的 `kind`。穷举 `match`：领域加一种流，这里编译不过。
pub fn stream_kind_name(kind: StreamKind) -> &'static str {
    match kind {
        StreamKind::Project => "project",
        StreamKind::Assistant => "assistant",
        StreamKind::Conversation => "conversation",
        StreamKind::Turn => "turn",
        StreamKind::Approval => "approval",
        StreamKind::Audit => "audit",
        StreamKind::Rule => "rule",
        StreamKind::Runtime => "runtime",
        StreamKind::Settings => "settings",
        StreamKind::System => "system",
    }
}

/// 一种范围在两张表里的那一行：订阅哪些键、快照跑哪些查询。
struct ScopeEntry {
    streams: Vec<StreamKey>,
    snapshot: Vec<Q>,
}

/// 范围登记表。穷举 `Scope`：proto 加一种范围，编译器逼着在这里加一行，两列写在一起不会漏一列。
///
/// 返回 `None` 表示范围不合法（会话 ID 为空），调用方据此回 `INVALID_ARGUMENT`。
fn scope_entry(scope: &Scope) -> Option<ScopeEntry> {
    use StreamKind as K;
    let all = |k| StreamKey::all(stream_kind_name(k));
    let entry = match scope {
        Scope::Sidebar(_) => ScopeEntry {
            streams: vec![all(K::Project), all(K::Conversation), all(K::Assistant)],
            snapshot: vec![
                Q::ListProjects(pb::ListProjects {}),
                Q::ListConversations(pb::ListConversations {
                    project_id: None,
                    title_contains: None,
                    before: None,
                    // 侧栏快照不分页（T09 方案 G8）。
                    limit: 0,
                }),
                Q::ListAssistants(pb::ListAssistants {}),
            ],
        },
        Scope::Conversation(c) => {
            if c.conversation_id.is_empty() {
                return None;
            }
            ScopeEntry {
                // 回合、审批、卡片事件经 routes_for 的会话副键也落在这个键上（方案 Q3）。
                streams: vec![StreamKey::one(
                    stream_kind_name(K::Conversation),
                    c.conversation_id.clone(),
                )],
                snapshot: vec![
                    Q::GetConversation(pb::GetConversation {
                        after_message_id: None,
                        around_seq: None,
                        before_seq: None,
                        after_seq: None,
                        conversation_id: c.conversation_id.clone(),
                        before_message_id: None,
                        limit: CONVERSATION_SNAPSHOT_PAGE_SIZE,
                    }),
                    Q::ListPendingItems(pb::ListPendingItems {}),
                ],
            }
        }
        // project：AGENTS.md 草稿卡；turn：「继续」卡片。
        Scope::Pending(_) => ScopeEntry {
            streams: vec![all(K::Approval), all(K::Project), all(K::Turn)],
            snapshot: vec![Q::ListPendingItems(pb::ListPendingItems {})],
        },
        Scope::Quota(_) => ScopeEntry {
            streams: vec![all(K::Runtime)],
            snapshot: vec![Q::GetQuota(pb::GetQuota {})],
        },
        Scope::RuntimeStatus(_) => ScopeEntry {
            streams: vec![all(K::Runtime)],
            snapshot: vec![Q::GetRuntimeStatus(pb::GetRuntimeStatus {})],
        },
        // project：项目设置。
        Scope::Settings(_) => ScopeEntry {
            streams: vec![all(K::Settings), all(K::Project)],
            snapshot: vec![Q::GetSettings(pb::GetSettings { project_id: None })],
        },
    };
    Some(entry)
}

/// 范围 → 路由键。范围未设置（oneof 为空）或不合法时返回空，ipc 据此回 `INVALID_ARGUMENT`。
pub fn scope_streams(scope: &pb::SubscriptionScope) -> Vec<StreamKey> {
    scope
        .kind
        .as_ref()
        .and_then(scope_entry)
        .map(|e| e.streams)
        .unwrap_or_default()
}

/// 范围 → 快照的查询列表，按登记顺序。范围未设置或不合法时返回空。
pub fn snapshot_queries(scope: &pb::SubscriptionScope) -> Vec<pb::Query> {
    scope
        .kind
        .as_ref()
        .and_then(scope_entry)
        .map(|e| {
            e.snapshot
                .into_iter()
                .map(|kind| pb::Query { kind: Some(kind) })
                .collect()
        })
        .unwrap_or_default()
}

/// 旧事件没带所属对话时只 warn 一次（补发会把整段历史再算一遍路由，逐条 warn 会刷屏）。
static LEGACY_WARNED: std::sync::Once = std::sync::Once::new();

fn carried<'a>(event: &DomainEvent, id: Option<&'a ConversationId>) -> Option<&'a ConversationId> {
    if id.is_none() {
        LEGACY_WARNED.call_once(|| {
            tracing::warn!(
                kind = event.kind(),
                "事件没有所属对话（加 conversation_id 之前落盘的旧事件），只走主流"
            );
        });
    }
    id
}

/// 事件 → 路由键。第一个键永远是 [`DomainEvent::stream`] 那条流；回合、审批、亲手执行卡片事件
/// 再加一个 `conversation:<id>`，让会话范围也收到它们（方案 Q3）；回合、工具调用、审批后续、卡片状态事件的对话
/// 取自事件自己带的 `conversation_id`（G5）。
pub fn routes_for(event: &DomainEvent) -> Vec<StreamKey> {
    let (kind, id) = event.stream();
    let kind = stream_kind_name(kind);
    let primary = match id {
        Some(id) => StreamKey::one(kind, id.0),
        None => StreamKey::all(kind),
    };
    let mut routes = vec![primary];
    if let Some(conversation_id) = owning_conversation(event) {
        routes.push(StreamKey::one(
            stream_kind_name(StreamKind::Conversation),
            conversation_id.to_string(),
        ));
    }
    routes
}

/// 主流不是会话、但属于某个对话的事件，返回那个对话。穷举 `match`：加一种事件要在这里表态。
fn owning_conversation(event: &DomainEvent) -> Option<&ConversationId> {
    use DomainEvent as E;
    match event {
        E::TurnQueued(e) => Some(&e.turn.conversation_id),
        E::ApprovalRequested(e) => Some(&e.approval.conversation_id),
        E::UserActionProposed(e) => Some(&e.card.conversation_id),

        // 事件自己带着所属对话（G5，T10a 方案 2.7）。`None` 只出现在加这个字段之前落盘的旧事件上：
        // 只走主流，warn 一次，不回填（T03 方案 Q3）；那时这些事件本来就只走主流，会话范围靠快照对齐。
        E::TurnStateChanged(e) => carried(event, e.conversation_id.as_ref()),
        E::TurnFinished(e) => carried(event, e.conversation_id.as_ref()),
        E::TurnModelReported(e) => carried(event, e.conversation_id.as_ref()),
        E::TurnSessionBound(e) => carried(event, e.conversation_id.as_ref()),
        E::TurnIntentRecorded(e) => carried(event, e.conversation_id.as_ref()),
        E::ToolCallStarted(e) => carried(event, e.conversation_id.as_ref()),
        E::ToolCallFinished(e) => carried(event, e.conversation_id.as_ref()),
        E::ApprovalDelivered(e) => carried(event, e.conversation_id.as_ref()),
        E::ApprovalReviewUpdated(e) => carried(event, e.conversation_id.as_ref()),
        E::ApprovalDecided(e) => carried(event, e.conversation_id.as_ref()),
        E::ApprovalVoided(e) => carried(event, e.conversation_id.as_ref()),
        E::UserActionStateChanged(e) => carried(event, e.conversation_id.as_ref()),

        // 主流已经是 conversation，或者不属于单个对话（审计流不进任何范围）。
        E::ProjectAdded(_)
        | E::ProjectUpdated(_)
        | E::ProjectRelocated(_)
        | E::ProjectPathStatusChanged(_)
        | E::ProjectRemoved(_)
        | E::ProjectSettingsUpdated(_)
        | E::RepositoryInitialized(_)
        | E::CloneFinished(_)
        | E::AgentsMdDraftProposed(_)
        | E::AgentsMdDraftDecided(_)
        | E::AssistantCreated(_)
        | E::AssistantUpdated(_)
        | E::AssistantDeleted(_)
        | E::AssistantReset(_)
        | E::ConversationCreated(_)
        | E::ConversationDeleting(_)
        | E::ConversationDeleted(_)
        | E::ConversationModelChanged(_)
        | E::ApprovalModeChanged(_)
        | E::DmProjectSwitched(_)
        | E::ConversationRead(_)
        | E::MessageAppended(_)
        | E::MessageCompleted(_)
        | E::ApprovalEvaluated(_)
        | E::RuntimeSessionForgetFailed(_)
        | E::RuleRemembered(_)
        | E::RuleRevoked(_)
        | E::RulesDeleted(_)
        | E::ApprovalStaleResultDropped(_)
        | E::SecurityListEntryAdded(_)
        | E::SecurityListEntryRemoved(_)
        | E::AttachmentStored(_)
        | E::AttachmentConfirmed(_)
        | E::RuntimeUpdated(_)
        | E::QuotaObserved(_)
        | E::UsageReported(_)
        | E::LocalImportPrompted(_)
        | E::LocalConfigImported(_)
        | E::McpSecretSet(_)
        | E::GlobalSettingsChanged(_)
        | E::RecoveryCompleted(_) => None,
    }
}
