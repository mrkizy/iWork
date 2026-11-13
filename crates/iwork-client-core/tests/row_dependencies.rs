//! 行的视图模型读了行外的状态：那份状态变了，读它的行要被标脏，没读它的行不标（两个方向都判）。
//! 与 `derive/row.rs` 模块文档里的「行外读取与标脏」表逐行对应：表里每一行在这里有一条用例。

mod support;

use iwork_client_core::store::{Connection, Item, QueryPurpose, Store, SummaryTable};
use iwork_proto::pb;
use pb::domain_event::Event as E;
use support::*;

fn live_with_card_and_message() -> Store {
    let mut s = store_with_open();
    s.set_connection(Connection::Live);
    s.apply(persisted(
        10,
        &[CONV],
        appended(assistant_msg(
            "m1",
            10,
            pb::MessageStatus::Completed,
            "好的",
        )),
    ));
    s.apply(persisted(
        11,
        &[CONV],
        E::ApprovalRequested(pb::ApprovalRequested {
            approval: Some(pb::Approval {
                id: "ap".into(),
                conversation_id: CID.into(),
                state: pb::ApprovalState::Pending as i32,
                created_seq: 11,
                ..Default::default()
            }),
        }),
    ));
    s.take_dirty();
    s
}

fn dirty_rows(s: &mut Store) -> Vec<String> {
    let mut v: Vec<String> = s.take_dirty().conversation.rows.into_iter().collect();
    v.sort();
    v
}

#[test]
fn connection_change_marks_card_rows_only() {
    let mut s = live_with_card_and_message();
    s.set_connection(Connection::Reconnecting);
    assert_eq!(
        dirty_rows(&mut s),
        ["ap"],
        "卡片的按钮随连接变；消息行不读连接"
    );
    // 对照：连接没变就不标
    s.set_connection(Connection::Reconnecting);
    assert!(dirty_rows(&mut s).is_empty());
}

fn assistants(round: u64, color: &str) -> Item {
    Item::ViewResult {
        purpose: QueryPurpose::Summary {
            round,
            table: SummaryTable::Assistants,
        },
        result: Box::new(pb::QueryResult {
            seq: 20 + round,
            kind: Some(pb::query_result::Kind::ListAssistants(
                pb::ListAssistantsResult {
                    assistants: vec![pb::Assistant {
                        id: "a1".into(),
                        config: Some(pb::AssistantConfig {
                            name: "iWork".into(),
                            avatar: Some(pb::Avatar {
                                initial: "i".into(),
                                color: color.into(),
                            }),
                            ..Default::default()
                        }),
                        ..Default::default()
                    }],
                    ..Default::default()
                },
            )),
        }),
    }
}

#[test]
fn assistant_table_change_marks_message_rows_only_when_changed() {
    let mut s = live_with_card_and_message();
    s.apply(assistants(1, "ch1"));
    assert_eq!(
        dirty_rows(&mut s),
        ["m1"],
        "头像进了消息行；审批卡片不读助手表"
    );
    // 对照：重查回来的表没变，不标
    s.apply(assistants(2, "ch1"));
    assert!(dirty_rows(&mut s).is_empty());
    s.apply(assistants(3, "ch2"));
    assert_eq!(dirty_rows(&mut s), ["m1"]);
}

/// 各种行都有一行的时间线：助手消息 m1（回合 t1，已结束 → end:t1）、带附件的用户消息 mu、审批 ap、
/// 亲手执行卡片 ua、档位切换的当次说明 note:<seq>。
fn rich() -> Store {
    let mut s = store_with_open();
    s.set_connection(Connection::Live);
    s.apply(persisted(
        5,
        &[CONV],
        E::TurnQueued(pb::TurnQueued {
            turn: Some(pb::Turn {
                id: "t1".into(),
                conversation_id: CID.into(),
                state: pb::TurnState::Running as i32,
                ..Default::default()
            }),
        }),
    ));
    let mut m = assistant_msg("m1", 6, pb::MessageStatus::Completed, "好的");
    m.turn_id = Some("t1".into());
    s.apply(persisted(6, &[CONV], appended(m)));
    let mut u = assistant_msg("mu", 7, pb::MessageStatus::Completed, "看看这个文件");
    u.author = Some(pb::Author {
        kind: Some(pb::author::Kind::User(pb::UserAuthor {})),
    });
    u.attachment_ids = vec!["att1".into()];
    s.apply(persisted(7, &[CONV], appended(u)));
    s.apply(persisted(
        8,
        &[CONV],
        E::TurnFinished(pb::TurnFinished {
            turn_id: "t1".into(),
            state: pb::TurnState::Failed as i32,
            ..Default::default()
        }),
    ));
    s.apply(persisted(
        9,
        &[CONV],
        E::ApprovalRequested(pb::ApprovalRequested {
            approval: Some(pb::Approval {
                id: "ap".into(),
                conversation_id: CID.into(),
                state: pb::ApprovalState::Pending as i32,
                created_seq: 9,
                ..Default::default()
            }),
        }),
    ));
    s.apply(persisted(
        10,
        &[CONV],
        E::UserActionProposed(pb::UserActionProposed {
            card: Some(pb::UserActionCard {
                id: "ua".into(),
                conversation_id: CID.into(),
                state: pb::UserActionState::Unknown as i32,
                created_seq: 10,
                ..Default::default()
            }),
        }),
    ));
    s.apply(persisted(
        11,
        &[CONV],
        E::ApprovalModeChanged(pb::ApprovalModeChanged {
            target: Some(pb::ApprovalModeTarget {
                kind: Some(pb::approval_mode_target::Kind::ConversationId(CID.into())),
            }),
            mode: Some(pb::ApprovalMode::Auto as i32),
            ..Default::default()
        }),
    ));
    let rows = row_ids(&s);
    for want in ["m1", "mu", "end:t1", "ap", "ua", "note:11"] {
        assert!(
            rows.iter().any(|r| r == want),
            "准备：时间线里有 {want}（实际 {rows:?}）"
        );
    }
    s.take_dirty();
    s
}

#[test]
fn turn_state_marks_messages_of_that_turn_only() {
    let mut s = rich();
    s.apply(persisted(
        20,
        &[CONV],
        E::TurnStateChanged(pb::TurnStateChanged {
            turn_id: "t1".into(),
            from: pb::TurnState::Running as i32,
            to: pb::TurnState::Failed as i32,
            reason: Some("模型暂时不可用".into()),
            ..Default::default()
        }),
    ));
    assert_eq!(
        dirty_rows(&mut s),
        ["m1"],
        "表情与失败原因读回合状态；用户消息、卡片不读"
    );
}

#[test]
fn message_status_marks_its_turn_end_row() {
    let mut s = rich();
    s.apply(persisted(
        20,
        &[CONV],
        E::MessageCompleted(pb::MessageCompleted {
            conversation_id: CID.into(),
            message_id: "m1".into(),
            body: None,
            status: pb::MessageStatus::Failed as i32,
        }),
    ));
    assert_eq!(
        dirty_rows(&mut s),
        ["end:t1", "m1"],
        "回合结束行读同回合消息的 status"
    );
}

#[test]
fn deleting_marks_user_action_cards_only() {
    let mut s = rich();
    s.apply(persisted(
        20,
        &[CONV],
        E::ConversationDeleting(pb::ConversationDeleting {
            conversation_id: CID.into(),
        }),
    ));
    assert_eq!(
        dirty_rows(&mut s),
        ["ua"],
        "卡片说明读「删除中」；别的行不读"
    );
}

fn projects(round: u64, name: &str) -> Item {
    Item::ViewResult {
        purpose: QueryPurpose::Summary {
            round,
            table: SummaryTable::Projects,
        },
        result: Box::new(pb::QueryResult {
            seq: 30 + round,
            kind: Some(pb::query_result::Kind::ListProjects(
                pb::ListProjectsResult {
                    projects: vec![pb::Project {
                        id: PID.into(),
                        name: name.into(),
                        ..Default::default()
                    }],
                },
            )),
        }),
    }
}

#[test]
fn project_table_change_marks_notes_and_dividers_only_when_changed() {
    let mut s = rich();
    s.apply(projects(1, "demo"));
    assert_eq!(dirty_rows(&mut s), ["note:11"], "项目名进了说明行与分隔线");
    s.apply(projects(2, "demo"));
    assert!(dirty_rows(&mut s).is_empty(), "对照：表没变就不标");
    s.apply(projects(3, "改名了"));
    assert_eq!(dirty_rows(&mut s), ["note:11"]);
}

#[test]
fn attachment_marks_messages_that_mention_it() {
    let mut s = rich();
    s.apply(persisted(
        20,
        &[CONV],
        E::AttachmentStored(pb::AttachmentStored {
            attachment: Some(pb::Attachment {
                id: "att1".into(),
                conversation_id: CID.into(),
                name: "a.csv".into(),
                ..Default::default()
            }),
        }),
    ));
    assert_eq!(dirty_rows(&mut s), ["mu"], "附件进了提到它的消息");
}

#[test]
fn delta_appends_to_its_own_row_only() {
    let mut s = rich();
    let mut m = assistant_msg("m2", 21, pb::MessageStatus::Streaming, "");
    m.turn_id = Some("t1".into());
    s.apply(persisted(21, &[CONV], appended(m)));
    s.take_dirty();
    s.apply(delta(&[CONV], "m2", 0, "第一段"));
    let d = s.take_dirty().conversation;
    assert!(d.rows.is_empty(), "流式追加不整行重发别的行");
    assert_eq!(d.appended.keys().collect::<Vec<_>>(), ["m2"]);
}
