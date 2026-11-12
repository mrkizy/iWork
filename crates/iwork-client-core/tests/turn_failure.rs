//! 回合失败的显示（ui-m1.md 2a 第 10 条，Q14）：一开始就失败 → 不画空气泡、一行「这一轮没能开始：…」；
//! 输出一部分后失败 → 照常显示消息并加「这一轮中途失败：…」；原因挂在消息上时回合结束行不重复。

mod support;

use iwork_client_core::derive::row::row;
use iwork_client_core::store::Store;
use iwork_proto::pb;
use pb::domain_event::Event as E;
use pb::row_view::Row as V;
use support::*;

fn failed_turn(text: &str, status: pb::MessageStatus) -> Store {
    let mut s = store_with_open();
    s.apply(persisted(
        5,
        &[CONV],
        E::TurnQueued(pb::TurnQueued {
            turn: Some(pb::Turn {
                id: "t1".into(),
                conversation_id: CID.into(),
                state: pb::TurnState::Queued as i32,
                ..Default::default()
            }),
        }),
    ));
    let mut m = assistant_msg("m1", 6, pb::MessageStatus::Streaming, "");
    m.turn_id = Some("t1".into());
    s.apply(persisted(6, &[CONV], appended(m)));
    s.apply(persisted(
        7,
        &[CONV],
        E::TurnStateChanged(pb::TurnStateChanged {
            turn_id: "t1".into(),
            from: pb::TurnState::Running as i32,
            to: pb::TurnState::Failed as i32,
            reason: Some("Claude Code 启动后立刻退出了".into()),
            ..Default::default()
        }),
    ));
    s.apply(persisted(
        8,
        &[CONV],
        E::MessageCompleted(pb::MessageCompleted {
            conversation_id: CID.into(),
            message_id: "m1".into(),
            body: (!text.is_empty()).then(|| pb::MessageBody {
                content: Some(pb::message_body::Content::InlineText(text.into())),
            }),
            status: status as i32,
        }),
    ));
    s.apply(persisted(
        9,
        &[CONV],
        E::TurnFinished(pb::TurnFinished {
            turn_id: "t1".into(),
            state: pb::TurnState::Failed as i32,
            ..Default::default()
        }),
    ));
    s
}

fn view(s: &Store, id: &str) -> V {
    let tl = s.open.as_ref().unwrap();
    row(s, tl, tl.key_of(id).unwrap(), id, tl.row(id).unwrap())
        .row
        .unwrap()
}

fn message(s: &Store) -> pb::MessageRow {
    match view(s, "m1") {
        V::Message(m) => m,
        other => panic!("{other:?}"),
    }
}

fn turn_end(s: &Store) -> pb::TurnEndRow {
    match view(s, "end:t1") {
        V::TurnEnd(t) => t,
        other => panic!("{other:?}"),
    }
}

#[test]
fn failed_before_any_output_hides_bubble() {
    let s = failed_turn("", pb::MessageStatus::Failed);
    let m = message(&s);
    assert!(m.bubble_hidden);
    let f = m.failure.unwrap();
    assert_eq!(f.text, "这一轮没能开始：Claude Code 启动后立刻退出了");
    assert!(f.detail.contains("t1"), "详情带回合 id：{}", f.detail);
    assert_eq!(m.status_label, "", "不另挂「失败」标签");
    assert_eq!(
        turn_end(&s).outcome_label,
        "",
        "原因已在消息上，回合结束行不重复"
    );
}

#[test]
fn failed_midway_keeps_bubble_and_adds_line() {
    let s = failed_turn("我先看一下日志", pb::MessageStatus::Failed);
    let m = message(&s);
    assert!(!m.bubble_hidden);
    assert_eq!(m.text, "我先看一下日志");
    assert_eq!(
        m.failure.unwrap().text,
        "这一轮中途失败：Claude Code 启动后立刻退出了"
    );
}

#[test]
fn completed_message_in_failed_turn_keeps_reason_on_turn_end() {
    // 对照：消息本身正常结束、回合失败（turn-end 场景的形状）——消息上没有说明行，原因留在回合结束行
    let s = failed_turn("开始跑……", pb::MessageStatus::Completed);
    let m = message(&s);
    assert_eq!(m.failure, None);
    assert!(!m.bubble_hidden);
    assert_eq!(
        turn_end(&s).outcome_label,
        "失败：Claude Code 启动后立刻退出了"
    );
}
