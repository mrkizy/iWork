//! 订阅登记表：范围 → 键与快照、事件 → 路由键（T03 方案 2.8、7.2）。

mod support;

use std::borrow::Cow;

use iwork_app::subscription::{
    CONVERSATION_SNAPSHOT_PAGE_SIZE, routes_for, scope_streams, snapshot_queries, stream_kind_name,
};
use iwork_domain::event::{DomainEvent, StreamKind};
use iwork_proto::pb;
use iwork_proto::pb::query::Kind as Q;
use iwork_proto::pb::subscription_scope::Kind as Scope;
use iwork_proto::routing::StreamKey;
use proptest::strategy::{Strategy, ValueTree};
use proptest::test_runner::TestRunner;
use support::arb::event_strategies;
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
            vec![StreamKey::one("conversation", "c1")],
            vec![
                query(Q::GetConversation(pb::GetConversation {
                    conversation_id: "c1".into(),
                    before_message_id: None,
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
            vec![all("settings"), all("project")],
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

/// 载荷里已经带会话 ID 的回合 / 审批 / 卡片事件 → 那个会话。
fn carried_conversation(event: &DomainEvent) -> Option<String> {
    match event {
        DomainEvent::TurnQueued(e) => Some(e.turn.conversation_id.to_string()),
        DomainEvent::ApprovalRequested(e) => Some(e.approval.conversation_id.to_string()),
        DomainEvent::UserActionProposed(e) => Some(e.card.conversation_id.to_string()),
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
            // 其余事件只有主键：主流本来就是会话的不重复加，其他流的不属于单个对话。
            None => assert_eq!(routes.len(), 1, "{kind} 多出了副键 {conversation_routes:?}"),
        }
    }
    // 对照：三种带会话 ID 的事件确实被抽到并检查过。
    assert_eq!(with_conversation, 3 * SAMPLES_PER_KIND);
}

/// 方案 Q3：回合、审批、卡片流上的事件都要能被会话范围收到。
#[test]
#[ignore = "等 T10 给回合/审批/卡片事件加 conversation_id"]
fn routes_for_turn_approval_card_events_include_conversation() {
    for (kind, event) in sampled_events() {
        let (stream, _) = event.stream();
        if !matches!(stream, StreamKind::Turn | StreamKind::Approval) {
            continue;
        }
        let routes = routes_for(&event);
        assert!(
            routes[1..]
                .iter()
                .any(|k| k.kind == "conversation" && k.id.is_some()),
            "{kind} 缺会话副键：{routes:?}"
        );
    }
}
