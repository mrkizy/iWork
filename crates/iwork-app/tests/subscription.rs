//! 订阅登记表：范围 → 键与快照、事件 → 路由键（T03 方案 2.8、7.2）。

mod support;

use std::borrow::Cow;

use iwork_app::subscription::{
    CONVERSATION_SNAPSHOT_PAGE_SIZE, routes_for, scope_streams, snapshot_queries, stream_kind_name,
};
use iwork_domain::event::{DomainEvent, StreamKind};
use iwork_domain::id::{ConversationId, RawUlid};
use iwork_proto::pb;
use iwork_proto::pb::query::Kind as Q;
use iwork_proto::pb::subscription_scope::Kind as Scope;
use iwork_proto::routing::StreamKey;
use proptest::strategy::{Strategy, ValueTree};
use proptest::test_runner::TestRunner;
use support::arb::{event_strategies, with_conversation};
use support::oneof_fields;

/// 每种事件抽多少个样例。
const SAMPLES_PER_KIND: usize = 16;

fn scope(kind: Scope) -> pb::SubscriptionScope {
    pb::SubscriptionScope { kind: Some(kind) }
}

fn all(kind: &'static str) -> StreamKey {
    StreamKey::all(kind)
}

fn query(kind: Q) -> pb::Query {
    pb::Query { kind: Some(kind) }
}

/// 六种范围与按方案 2.8 表手写的期望（键、快照）。
fn scope_table() -> Vec<(pb::SubscriptionScope, Vec<StreamKey>, Vec<pb::Query>)> {
    vec![
        (
            scope(Scope::Sidebar(pb::SidebarScope {})),
            vec![all("project"), all("conversation"), all("assistant")],
            vec![
                query(Q::ListProjects(pb::ListProjects {})),
                query(Q::ListConversations(pb::ListConversations {
                    project_id: None,
                    title_contains: None,
                    before: None,
                    limit: 0,
                })),
                query(Q::ListAssistants(pb::ListAssistants {})),
            ],
        ),
        (
            scope(Scope::Conversation(pb::ConversationScope {
                conversation_id: "c1".into(),
            })),
            vec![
                StreamKey::one("conversation", "c1"),
                StreamKey::one("live", "c1"),
            ],
            vec![
                query(Q::GetConversation(pb::GetConversation {
                    conversation_id: "c1".into(),
                    before_message_id: None,
                    after_message_id: None,
                    around_seq: None,
                    before_seq: None,
                    after_seq: None,
                    limit: CONVERSATION_SNAPSHOT_PAGE_SIZE,
                })),
                query(Q::ListPendingItems(pb::ListPendingItems {})),
            ],
        ),
        (
            scope(Scope::Pending(pb::PendingScope {})),
            vec![all("approval"), all("project"), all("turn")],
            vec![query(Q::ListPendingItems(pb::ListPendingItems {}))],
        ),
        (
            scope(Scope::Quota(pb::QuotaScope {})),
            vec![all("runtime")],
            vec![query(Q::GetQuota(pb::GetQuota {}))],
        ),
        (
            scope(Scope::RuntimeStatus(pb::RuntimeStatusScope {})),
            vec![all("runtime")],
            vec![query(Q::GetRuntimeStatus(pb::GetRuntimeStatus {}))],
        ),
        (
            scope(Scope::Settings(pb::SettingsScope {})),
            vec![all("settings"), all("project"), all("rule")],
            vec![query(Q::GetSettings(pb::GetSettings { project_id: None }))],
        ),
    ]
}

#[test]
fn every_scope_has_streams_and_snapshot() {
    let table = scope_table();
    // 对照：手写表覆盖了 proto 里的全部范围，加一种范围这里就红。
    assert_eq!(table.len(), oneof_fields("SubscriptionScope").len());
    for (scope, streams, snapshot) in table {
        assert!(!streams.is_empty() && !snapshot.is_empty(), "{scope:?}");
        assert_eq!(scope_streams(&scope), streams, "{scope:?} 的键");
        assert_eq!(snapshot_queries(&scope), snapshot, "{scope:?} 的快照");
    }
}

#[test]
fn unset_or_invalid_scope_has_nothing() {
    let unset = pb::SubscriptionScope { kind: None };
    let empty_id = scope(Scope::Conversation(pb::ConversationScope {
        conversation_id: String::new(),
    }));
    for s in [unset, empty_id] {
        assert!(scope_streams(&s).is_empty(), "{s:?}");
        assert!(snapshot_queries(&s).is_empty(), "{s:?}");
    }
}

/// `StreamKind` 的全部变体。`index` 的穷举 match 保证领域加一种流时这里编译不过、跟着补。
const ALL_STREAM_KINDS: [StreamKind; 10] = [
    StreamKind::Project,
    StreamKind::Assistant,
    StreamKind::Conversation,
    StreamKind::Turn,
    StreamKind::Approval,
    StreamKind::Audit,
    StreamKind::Rule,
    StreamKind::Runtime,
    StreamKind::Settings,
    StreamKind::System,
];

fn index(kind: StreamKind) -> usize {
    match kind {
        StreamKind::Project => 0,
        StreamKind::Assistant => 1,
        StreamKind::Conversation => 2,
        StreamKind::Turn => 3,
        StreamKind::Approval => 4,
        StreamKind::Audit => 5,
        StreamKind::Rule => 6,
        StreamKind::Runtime => 7,
        StreamKind::Settings => 8,
        StreamKind::System => 9,
    }
}

/// 驼峰变体名机械转成蛇形名，作为 `stream_kind_name` 的独立期望值。
fn snake(name: &str) -> String {
    let mut out = String::new();
    for (i, ch) in name.chars().enumerate() {
        if ch.is_ascii_uppercase() {
            if i > 0 {
                out.push('_');
            }
            out.push(ch.to_ascii_lowercase());
        } else {
            out.push(ch);
        }
    }
    out
}

#[test]
fn stream_kind_names_match_domain() {
    for (i, kind) in ALL_STREAM_KINDS.into_iter().enumerate() {
        assert_eq!(index(kind), i, "ALL_STREAM_KINDS 顺序与 index 不一致");
        assert_eq!(stream_kind_name(kind), snake(&format!("{kind:?}")));
    }
    // 对照：多词变体也按规则转（目前 StreamKind 都是单词，snake 自身在这里证明会加下划线）。
    assert_eq!(snake("RuntimeStatus"), "runtime_status");
}

/// 每种事件抽若干样例：(种类名, 事件)。
fn sampled_events() -> Vec<(&'static str, DomainEvent)> {
    let strategies = event_strategies();
    assert_eq!(
        strategies.len(),
        DomainEvent::KINDS.len(),
        "对照：策略覆盖全部事件"
    );
    let mut runner = TestRunner::deterministic();
    let mut out = Vec::new();
    for (kind, strategy) in strategies {
        for _ in 0..SAMPLES_PER_KIND {
            let event = strategy
                .new_tree(&mut runner)
                .expect("能生成样例")
                .current();
            assert_eq!(event.kind(), kind, "策略登记的种类名与事件不符");
            out.push((kind, event));
        }
    }
    out
}

/// `stream()` 转成路由键，独立于实现：kind 取变体名的蛇形。
fn expected_primary(event: &DomainEvent) -> StreamKey {
    let (kind, id) = event.stream();
    StreamKey {
        kind: Cow::Owned(snake(&format!("{kind:?}"))),
        id: id.map(|id| id.0),
    }
}

#[test]
fn routes_for_matches_domain_stream() {
    for (kind, event) in sampled_events() {
        let routes = routes_for(&event);
        assert_eq!(routes.first(), Some(&expected_primary(&event)), "{kind}");
    }
}

/// 载荷里带着所属对话的事件 → 那个对话：三种实体事件取实体上的，十二种后续事件取自己带的（G5，`None` = 旧事件）。
/// 独立于实现：直接读字段，不经 `routes_for`。
fn carried_conversation(event: &DomainEvent) -> Option<String> {
    use DomainEvent as E;
    let carried = |c: &Option<ConversationId>| c.map(|c| c.to_string());
    match event {
        E::TurnQueued(e) => Some(e.turn.conversation_id.to_string()),
        E::ApprovalRequested(e) => Some(e.approval.conversation_id.to_string()),
        E::UserActionProposed(e) => Some(e.card.conversation_id.to_string()),
        E::TurnStateChanged(e) => carried(&e.conversation_id),
        E::TurnFinished(e) => carried(&e.conversation_id),
        E::TurnModelReported(e) => carried(&e.conversation_id),
        E::TurnSessionBound(e) => carried(&e.conversation_id),
        E::TurnIntentRecorded(e) => carried(&e.conversation_id),
        E::ToolCallStarted(e) => carried(&e.conversation_id),
        E::ToolCallFinished(e) => carried(&e.conversation_id),
        E::ApprovalDelivered(e) => carried(&e.conversation_id),
        E::ApprovalReviewUpdated(e) => carried(&e.conversation_id),
        E::ApprovalDecided(e) => carried(&e.conversation_id),
        E::ApprovalVoided(e) => carried(&e.conversation_id),
        E::UserActionStateChanged(e) => carried(&e.conversation_id),
        _ => None,
    }
}

#[test]
fn routes_for_every_event_kind() {
    let mut with_conversation = 0;
    for (kind, event) in sampled_events() {
        let routes = routes_for(&event);
        assert!(!routes.is_empty(), "{kind} 没有路由键");
        let conversation_routes: Vec<_> = routes[1..]
            .iter()
            .filter(|k| k.kind == "conversation")
            .collect();
        match carried_conversation(&event) {
            Some(id) => {
                with_conversation += 1;
                assert_eq!(
                    routes[1..],
                    [StreamKey::one("conversation", id)],
                    "{kind} 应加会话副键"
                );
            }
            // 其余事件只有主键：主流本来就是会话的不重复加，其他流的不属于单个对话，旧事件没带所属对话。
            None => assert_eq!(routes.len(), 1, "{kind} 多出了副键 {conversation_routes:?}"),
        }
    }
    // 对照：三种实体事件的样例全带会话副键，十二种后续事件里也抽到了带着的
    assert!(
        with_conversation > 3 * SAMPLES_PER_KIND,
        "{with_conversation}"
    );
}

/// 回合流、审批流上的每一种事件，带着所属对话时都进会话范围（方案 Q3、T10a 方案 2.7 G5）。
/// 加一种回合或审批流的事件却没给它所属对话，这里红。
#[test]
fn turn_scoped_events_route_to_conversation() {
    let conversation = ConversationId::from_raw(RawUlid::from_u128(0xC0));
    let key = StreamKey::one("conversation", conversation.to_string());
    let mut kinds = std::collections::BTreeSet::new();
    for (kind, event) in sampled_events() {
        let (stream, _) = event.stream();
        if !matches!(stream, StreamKind::Turn | StreamKind::Approval) {
            continue;
        }
        let event = with_conversation(event, Some(conversation));
        let routes = routes_for(&event);
        // 三种实体事件的对话在实体上（随机的），别的都是刚换上的这一个
        let want = match carried_conversation(&event) {
            Some(id) => StreamKey::one("conversation", id),
            None => panic!("{kind} 在回合或审批流上，却没有所属对话"),
        };
        assert_eq!(routes, [expected_primary(&event), want], "{kind}");
        if !matches!(
            event,
            DomainEvent::TurnQueued(_)
                | DomainEvent::ApprovalRequested(_)
                | DomainEvent::UserActionProposed(_)
        ) {
            assert_eq!(routes[1], key, "{kind}");
        }
        kinds.insert(kind);
    }
    // 对照：十二种后续事件加三种实体事件都检查过
    assert_eq!(kinds.len(), 15, "{kinds:?}");
}

/// 加字段之前落盘的旧事件（没有所属对话）：只走主流，不猜、不回填（T03 方案 Q3）。
#[test]
fn legacy_event_without_conversation_routes_primary_only() {
    let mut checked = 0;
    for (kind, event) in sampled_events() {
        let legacy = with_conversation(event.clone(), None);
        if legacy == event && carried_conversation(&event).is_some() {
            // 三种实体事件：对话在实体上，没有「旧事件」一说
            continue;
        }
        let (stream, _) = legacy.stream();
        if !matches!(stream, StreamKind::Turn | StreamKind::Approval) {
            continue;
        }
        assert_eq!(routes_for(&legacy), [expected_primary(&legacy)], "{kind}");
        checked += 1;
    }
    // 对照：十二种都抽到了
    assert_eq!(checked, 12 * SAMPLES_PER_KIND);
}
