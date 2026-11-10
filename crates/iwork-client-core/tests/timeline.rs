//! 时间线（方案 2.2、第 7 节「时间线（F-5）」）：不连后台，按调用方（会话层）的方式喂 `Store::apply`。

mod support;

use iwork_client_core::store::timeline::TIMELINE_CAP;
use iwork_client_core::store::{FollowUp, QueryPurpose};
use iwork_proto::pb;
use pb::MessageStatus as S;
use pb::domain_event::Event as E;
use support::*;

fn text_of(s: &iwork_client_core::store::Store, id: &str) -> String {
    s.open
        .as_ref()
        .unwrap()
        .text(id)
        .unwrap_or_default()
        .to_owned()
}

/// S4：断线期间一条已滚出最新页的旧消息完成了；重连走快照只留这一页，那条旧消息不再显示成流式中。
#[test]
fn snapshot_drops_rows_outside_page() {
    let mut s = store_with_open();
    s.apply(persisted(
        10,
        &[CONV],
        appended(assistant_msg("old", 0, S::Streaming, "")),
    ));
    s.apply(persisted(
        11,
        &[CONV],
        appended(assistant_msg("m2", 0, S::Completed, "hi")),
    ));
    assert_eq!(row_ids(&s), ["old", "m2"]);
    // 快照只带最新页（m2），old 在页外：整个重建，old 丢掉，往上翻时重新查到它的当前状态。
    s.apply(snapshot(
        CONV,
        50,
        vec![page(
            50,
            vec![assistant_msg("m2", 11, S::Completed, "hi")],
            true,
        )],
    ));
    assert_eq!(row_ids(&s), ["m2"]);
    let tl = s.open.as_ref().unwrap();
    assert!(tl.has_older);
    // 往上翻：拿到的是已完成的 old。
    s.apply(view_result(
        QueryPurpose::OlderPage {
            conversation_id: CID.into(),
        },
        page(
            51,
            vec![assistant_msg("old", 10, S::Completed, "done")],
            false,
        ),
    ));
    assert_eq!(row_ids(&s), ["old", "m2"]);
    assert_eq!(text_of(&s, "old"), "done");
    match s.open.as_ref().unwrap().row("old") {
        Some(iwork_client_core::store::Row::Message(m)) => {
            assert_eq!(m.status, S::Completed as i32)
        }
        other => panic!("{other:?}"),
    }
}

#[test]
fn live_page_merges_when_at_tail() {
    let mut s = store_with_open();
    s.apply(persisted(
        10,
        &[CONV],
        appended(assistant_msg("m1", 0, S::Completed, "a")),
    ));
    s.apply(persisted(
        11,
        &[CONV],
        appended(assistant_msg("m2", 0, S::Streaming, "")),
    ));
    // 连接没断时的最新页只含 m2：按身份合并，m1 留着。
    s.apply(view_result(
        QueryPurpose::LatestPage {
            conversation_id: CID.into(),
        },
        page(
            12,
            vec![assistant_msg("m2", 11, S::Streaming, "partial")],
            true,
        ),
    ));
    assert_eq!(row_ids(&s), ["m1", "m2"]);
    assert_eq!(text_of(&s, "m2"), "partial");
}

#[test]
fn detached_tail_rebuilds() {
    let mut s = store_with_open();
    s.apply(snapshot(
        CONV,
        1,
        vec![page(
            1,
            vec![assistant_msg("tail", 1, S::Completed, "t")],
            true,
        )],
    ));
    // 往上翻到超限：从新的一端丢，at_tail 变假。
    let older: Vec<pb::Message> = (0..TIMELINE_CAP as u64)
        .map(|i| assistant_msg(&format!("o{i}"), 1000 + i, S::Completed, "x"))
        .collect();
    // 让较旧的页 seq 比 tail 小：重新造 tail 的 seq 更大。
    s.apply(snapshot(
        CONV,
        9000,
        vec![page(
            9000,
            vec![assistant_msg("tail", 9000, S::Completed, "t")],
            true,
        )],
    ));
    s.apply(view_result(
        QueryPurpose::OlderPage {
            conversation_id: CID.into(),
        },
        page(9001, older, true),
    ));
    let tl = s.open.as_ref().unwrap();
    assert_eq!(tl.len(), TIMELINE_CAP);
    assert!(!tl.at_tail);
    assert!(tl.row("tail").is_none(), "从新的一端丢");
    // at_tail 为假时的最新页：重建，不合并。
    s.apply(view_result(
        QueryPurpose::LatestPage {
            conversation_id: CID.into(),
        },
        page(
            9002,
            vec![assistant_msg("tail", 9000, S::Completed, "t")],
            true,
        ),
    ));
    assert_eq!(row_ids(&s), ["tail"]);
    assert!(s.open.as_ref().unwrap().at_tail);
}

#[test]
fn timeline_cap_detaches_tail() {
    let mut s = store_with_open();
    s.apply(snapshot(
        CONV,
        9000,
        vec![page(
            9000,
            vec![assistant_msg("tail", 9000, S::Completed, "t")],
            true,
        )],
    ));
    let older: Vec<pb::Message> = (0..TIMELINE_CAP as u64 + 10)
        .map(|i| assistant_msg(&format!("o{i}"), 1 + i, S::Completed, "x"))
        .collect();
    s.apply(view_result(
        QueryPurpose::OlderPage {
            conversation_id: CID.into(),
        },
        page(9001, older, true),
    ));
    assert!(!s.open.as_ref().unwrap().at_tail);
    // 之后的新消息不进时间线，只计数。
    s.apply(persisted(
        9002,
        &[CONV],
        appended(assistant_msg("new", 0, S::Completed, "n")),
    ));
    let tl = s.open.as_ref().unwrap();
    assert!(tl.row("new").is_none());
    assert_eq!(tl.new_below, 1);
    assert_eq!(tl.len(), TIMELINE_CAP);
}

#[test]
fn delta_gap_requests_checkpoint() {
    let mut s = store_with_open();
    s.apply(persisted(
        10,
        &[CONV],
        appended(assistant_msg("m", 0, S::Streaming, "")),
    ));
    assert!(s.apply(delta(&[CONV], "m", 0, "ab")).follow_ups.is_empty());
    // 重叠部分丢掉。
    s.apply(delta(&[CONV], "m", 1, "bc"));
    assert_eq!(text_of(&s, "m"), "abc");
    // 缺了一段：停止追加，要一次检查点；缺口在等时不重复要。
    let a = s.apply(delta(&[CONV], "m", 10, "zz"));
    assert_eq!(
        a.follow_ups,
        [FollowUp::Checkpoint {
            conversation_id: CID.into()
        }]
    );
    assert_eq!(text_of(&s, "m"), "abc");
    assert!(s.apply(delta(&[CONV], "m", 12, "yy")).follow_ups.is_empty());
    // 检查点回来：正文对齐，之后的增量按新长度接上。
    s.apply(view_result(
        QueryPurpose::LatestPage {
            conversation_id: CID.into(),
        },
        page(
            11,
            vec![assistant_msg("m", 10, S::Streaming, "abcdefghij")],
            false,
        ),
    ));
    s.apply(delta(&[CONV], "m", 10, "k"));
    assert_eq!(text_of(&s, "m"), "abcdefghijk");
}

#[test]
fn unknown_target_event_is_dropped() {
    let mut s = store_with_open();
    s.apply(persisted(
        10,
        &[CONV],
        appended(assistant_msg("m", 0, S::Completed, "x")),
    ));
    let before = row_ids(&s);
    // 别的对话的回合事件（待处理范围订了整类回合流）：找不到目标，丢掉。
    s.apply(persisted(
        11,
        &[PENDING],
        E::TurnFinished(pb::TurnFinished {
            turn_id: "other".into(),
            state: pb::TurnState::Completed as i32,
            ..Default::default()
        }),
    ));
    s.apply(persisted(
        12,
        &[PENDING],
        E::ApprovalDecided(pb::ApprovalDecided {
            approval_id: "nope".into(),
            state: pb::ApprovalState::Approved as i32,
            ..Default::default()
        }),
    ));
    assert_eq!(row_ids(&s), before);
    assert_eq!(s.cursor(), 12, "丢掉的事件照样推进游标");
}

/// S11：查询结果的排序字段为 0：不拿 0 排序，整条时间线标「数据不完整」并点名实体。
#[test]
fn zero_seq_is_protocol_error() {
    let mut s = store_with_open();
    s.apply(snapshot(
        CONV,
        5,
        vec![page(
            5,
            vec![assistant_msg("bad", 0, S::Completed, "x")],
            false,
        )],
    ));
    let tl = s.open.as_ref().unwrap();
    let bad = tl.incomplete.as_ref().expect("标出数据不完整");
    assert_eq!((bad.entity, bad.id.as_str()), ("Message", "bad"));
    assert!(tl.row("bad").is_none(), "不以 0 排到最前");
    // 对照：排序字段齐全时没有这个标记。
    let mut ok = store_with_open();
    ok.apply(snapshot(
        CONV,
        5,
        vec![page(
            5,
            vec![assistant_msg("good", 3, S::Completed, "x")],
            false,
        )],
    ));
    assert!(ok.open.as_ref().unwrap().incomplete.is_none());
}

/// 事件里只出现在这条时间线以外对话的消息不进来；退订后排着的事件（没有活动范围对应）整条丢掉。
#[test]
fn events_for_other_conversations_or_no_scope_are_ignored() {
    let mut s = store_with_open();
    let mut m = assistant_msg("x", 0, S::Completed, "x");
    m.conversation_id = "c2".into();
    s.apply(persisted(10, &[SIDEBAR], appended(m)));
    assert!(row_ids(&s).is_empty());
    s.apply(persisted(
        11,
        &[99],
        appended(assistant_msg("y", 0, S::Completed, "y")),
    ));
    assert!(row_ids(&s).is_empty());
    assert_eq!(s.cursor(), 10, "没有活动范围对应的事件不推进游标");
}

/// 审批终态（方案 G3）：实时事件按终态契约改行。
#[test]
fn approval_events_follow_terminal_contract() {
    let mut s = store_with_open();
    s.apply(persisted(
        10,
        &[CONV],
        E::ApprovalRequested(pb::ApprovalRequested {
            approval: Some(pb::Approval {
                id: "ap".into(),
                conversation_id: CID.into(),
                state: pb::ApprovalState::Pending as i32,
                ..Default::default()
            }),
        }),
    ));
    s.apply(persisted(
        11,
        &[CONV],
        E::ApprovalVoided(pb::ApprovalVoided {
            approval_id: "ap".into(),
            reason: pb::VoidReason::TurnCancelled as i32,
        }),
    ));
    match s.open.as_ref().unwrap().row("ap") {
        Some(iwork_client_core::store::Row::Approval(a)) => {
            assert_eq!(a.state, pb::ApprovalState::Voided as i32);
            assert_eq!(a.void_reason, Some(pb::VoidReason::TurnCancelled as i32));
            assert_eq!(a.decider, None);
            assert_eq!(a.created_seq, 10);
        }
        other => panic!("{other:?}"),
    }
}

/// 当次说明（档位切换）进时间线；时间线重建时丢弃（方案 D5）。
#[test]
fn mode_change_note_is_live_only() {
    let mut s = store_with_open();
    s.apply(persisted(
        10,
        &[CONV],
        E::ApprovalModeChanged(pb::ApprovalModeChanged {
            target: Some(pb::ApprovalModeTarget {
                kind: Some(pb::approval_mode_target::Kind::ConversationId(CID.into())),
            }),
            mode: Some(pb::ApprovalMode::Auto as i32),
            cause: pb::ApprovalModeChangeCause::User as i32,
        }),
    ));
    assert_eq!(row_ids(&s), ["note:10"]);
    s.apply(snapshot(CONV, 20, vec![page(20, vec![], false)]));
    assert!(row_ids(&s).is_empty());
}
