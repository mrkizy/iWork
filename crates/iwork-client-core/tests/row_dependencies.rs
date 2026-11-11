//! 行的视图模型读了行外的状态：那份状态变了，读它的行要被标脏，没读它的行不标（两个方向都判）。
//! 回合状态 → 消息表情由场景对拍 `scenario_fold` 覆盖；这里是场景触发不到的连接状态与摘要表。

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
    s.apply(assistants(1, "#111111"));
    assert_eq!(
        dirty_rows(&mut s),
        ["m1"],
        "头像进了消息行；审批卡片不读助手表"
    );
    // 对照：重查回来的表没变，不标
    s.apply(assistants(2, "#111111"));
    assert!(dirty_rows(&mut s).is_empty());
    s.apply(assistants(3, "#222222"));
    assert_eq!(dirty_rows(&mut s), ["m1"]);
}
