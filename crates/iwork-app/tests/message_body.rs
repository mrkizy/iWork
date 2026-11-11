//! 消息正文缺失的处理：只有流式中的助手消息允许没有正文，其余缺失一律点名报错，不静默当成空串。

use iwork_app::convert::{ConvertError, event_from_proto};
use iwork_domain::event::DomainEvent;
use iwork_domain::message::{MessageBody, MessageStatus};
use iwork_proto::pb;

const ID: &str = "01ARZ3NDEKTSV4RRFFQ69G5FAV";

fn completed(body: Option<pb::MessageBody>, status: pb::MessageStatus) -> pb::DomainEvent {
    pb::DomainEvent {
        event: Some(pb::domain_event::Event::MessageCompleted(
            pb::MessageCompleted {
                conversation_id: ID.into(),
                message_id: ID.into(),
                body,
                status: status as i32,
            },
        )),
    }
}

fn inline(text: &str) -> Option<pb::MessageBody> {
    Some(pb::MessageBody {
        content: Some(pb::message_body::Content::InlineText(text.into())),
    })
}

fn appended(
    author: pb::author::Kind,
    status: pb::MessageStatus,
    body: Option<pb::MessageBody>,
) -> pb::DomainEvent {
    pb::DomainEvent {
        event: Some(pb::domain_event::Event::MessageAppended(
            pb::MessageAppended {
                message: Some(pb::Message {
                    id: ID.into(),
                    conversation_id: ID.into(),
                    author: Some(pb::Author { kind: Some(author) }),
                    context_project_id: None,
                    status: status as i32,
                    body,
                    attachment_ids: vec![],
                    created_at: Some(prost_types::Timestamp {
                        seconds: 1,
                        nanos: 0,
                    }),
                    turn_id: None,
                    created_seq: 0,
                }),
            },
        )),
    }
}

fn assistant() -> pb::author::Kind {
    pb::author::Kind::Assistant(pb::AssistantAuthor {
        assistant_id: ID.into(),
        name_snapshot: "iWork".into(),
    })
}

#[test]
fn completed_message_requires_body() {
    // 显式的空正文合法
    let ok = event_from_proto(completed(inline(""), pb::MessageStatus::Completed)).unwrap();
    assert!(
        matches!(ok, DomainEvent::MessageCompleted(e) if e.body == MessageBody::Inline(String::new()))
    );

    assert_eq!(
        event_from_proto(completed(None, pb::MessageStatus::Completed)),
        Err(ConvertError::MissingField {
            kind: "MessageCompleted",
            field: "body"
        })
    );
    assert_eq!(
        event_from_proto(completed(
            Some(pb::MessageBody { content: None }),
            pb::MessageStatus::Completed
        )),
        Err(ConvertError::MissingField {
            kind: "MessageBody",
            field: "content"
        })
    );
}

#[test]
fn completed_message_status_is_an_outcome() {
    for status in [
        pb::MessageStatus::Completed,
        pb::MessageStatus::Interrupted,
        pb::MessageStatus::Failed,
    ] {
        assert!(
            event_from_proto(completed(inline("x"), status)).is_ok(),
            "{status:?}"
        );
    }
    for status in [pb::MessageStatus::Sent, pb::MessageStatus::Streaming] {
        assert!(
            matches!(
                event_from_proto(completed(inline("x"), status)),
                Err(ConvertError::UnsupportedEnum {
                    kind: "MessageCompleted",
                    field: "status",
                    ..
                })
            ),
            "{status:?}"
        );
    }
}

#[test]
fn only_streaming_assistant_message_may_omit_body() {
    let streaming =
        event_from_proto(appended(assistant(), pb::MessageStatus::Streaming, None)).unwrap();
    assert!(matches!(
        streaming,
        DomainEvent::MessageAppended(e) if e.message.status == MessageStatus::Streaming && e.message.body == MessageBody::Inline(String::new())
    ));

    assert_eq!(
        event_from_proto(appended(
            pb::author::Kind::User(pb::UserAuthor {}),
            pb::MessageStatus::Sent,
            None
        )),
        Err(ConvertError::MissingField {
            kind: "Message",
            field: "body"
        })
    );
    assert_eq!(
        event_from_proto(appended(assistant(), pb::MessageStatus::Completed, None)),
        Err(ConvertError::MissingField {
            kind: "Message",
            field: "body"
        })
    );
    // 正文消息在、内容缺：流式也不行
    assert_eq!(
        event_from_proto(appended(
            assistant(),
            pb::MessageStatus::Streaming,
            Some(pb::MessageBody { content: None })
        )),
        Err(ConvertError::MissingField {
            kind: "MessageBody",
            field: "content"
        })
    );
    // 对照：用户消息带空正文合法
    assert!(
        event_from_proto(appended(
            pb::author::Kind::User(pb::UserAuthor {}),
            pb::MessageStatus::Sent,
            inline("")
        ))
        .is_ok()
    );
}
