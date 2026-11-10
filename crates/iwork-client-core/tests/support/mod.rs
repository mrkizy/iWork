//! 测试用的协议数据构造：只造字段，不含任何客户端逻辑。

#![allow(dead_code, reason = "各测试文件用其中一部分")]

use iwork_client_core::store::{Item, QueryPurpose, ScopeKind, Store};
use iwork_proto::pb;
use pb::domain_event::Event as E;

pub const SIDEBAR: u32 = 1;
pub const PENDING: u32 = 2;
pub const CONV: u32 = 3;
pub const CID: &str = "c1";
pub const PID: &str = "p1";

/// 侧栏、待处理、对话 c1 三个订阅都已 Ready、时间线打开的状态库。
pub fn store_with_open() -> Store {
    let mut s = Store::new();
    s.register_subscription(SIDEBAR, ScopeKind::Sidebar);
    s.register_subscription(PENDING, ScopeKind::Pending);
    s.register_subscription(CONV, ScopeKind::Conversation(CID.into()));
    s.set_visible(iwork_client_core::store::Visible {
        sidebar: true,
        conversation_id: Some(CID.into()),
        ..Default::default()
    });
    for id in [SIDEBAR, PENDING, CONV] {
        s.apply(Item::Subscribed(pb::Subscribed {
            subscription_id: id,
            snapshot: None,
            snapshot_seq: 0,
        }));
    }
    s
}

pub fn persisted(seq: u64, subs: &[u32], event: E) -> Item {
    Item::Event(Box::new(pb::Event {
        subscription_ids: subs.to_vec(),
        body: Some(pb::event::Body::Persisted(pb::PersistedEvent {
            seq,
            at: Some(prost_types::Timestamp {
                seconds: 1_762_000_000 + seq as i64,
                nanos: 0,
            }),
            actor: "user".into(),
            event: Some(pb::DomainEvent { event: Some(event) }),
        })),
    }))
}

pub fn delta(subs: &[u32], message_id: &str, offset: u64, text: &str) -> Item {
    Item::Event(Box::new(pb::Event {
        subscription_ids: subs.to_vec(),
        body: Some(pb::event::Body::Transient(pb::TransientEvent {
            event: Some(pb::transient_event::Event::MessageDelta(pb::MessageDelta {
                message_id: message_id.into(),
                offset,
                text: text.into(),
            })),
        })),
    }))
}

pub fn conversation(id: &str, title: &str, unread: u32) -> pb::Conversation {
    pb::Conversation {
        id: id.into(),
        kind: Some(pb::conversation::Kind::Project(pb::ProjectConversation {
            project_id: PID.into(),
        })),
        state: pb::ConversationState::Active as i32,
        unread,
        title: title.into(),
        title_source: pb::TitleSource::Auto as i32,
        ..Default::default()
    }
}

pub fn assistant_msg(
    id: &str,
    created_seq: u64,
    status: pb::MessageStatus,
    text: &str,
) -> pb::Message {
    pb::Message {
        id: id.into(),
        conversation_id: CID.into(),
        author: Some(pb::Author {
            kind: Some(pb::author::Kind::Assistant(pb::AssistantAuthor {
                assistant_id: "a1".into(),
                name_snapshot: "iWork".into(),
            })),
        }),
        status: status as i32,
        body: (!text.is_empty()).then(|| pb::MessageBody {
            content: Some(pb::message_body::Content::InlineText(text.into())),
        }),
        created_seq,
        ..Default::default()
    }
}

pub fn appended(m: pb::Message) -> E {
    E::MessageAppended(pb::MessageAppended { message: Some(m) })
}

pub fn page(seq: u64, messages: Vec<pb::Message>, has_more: bool) -> pb::QueryResult {
    pb::QueryResult {
        seq,
        kind: Some(pb::query_result::Kind::GetConversation(
            pb::GetConversationResult {
                conversation: Some(conversation(CID, "", 0)),
                messages,
                has_more,
                ..Default::default()
            },
        )),
    }
}

pub fn snapshot(sub: u32, seq: u64, results: Vec<pb::QueryResult>) -> Item {
    Item::Subscribed(pb::Subscribed {
        subscription_id: sub,
        snapshot: Some(pb::Snapshot { results }),
        snapshot_seq: seq,
    })
}

pub fn view_result(purpose: QueryPurpose, result: pb::QueryResult) -> Item {
    Item::ViewResult {
        purpose,
        result: Box::new(result),
    }
}

pub fn row_ids(s: &Store) -> Vec<String> {
    s.open
        .as_ref()
        .unwrap()
        .identities()
        .into_iter()
        .map(|(_, id)| id)
        .collect()
}
