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
        older(&s),
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
    let older_msgs: Vec<pb::Message> = (0..TIMELINE_CAP as u64)
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
    look_above_oldest(&mut s);
    s.apply(view_result(older(&s), page(9001, older_msgs, true)));
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
    let older_msgs: Vec<pb::Message> = (0..TIMELINE_CAP as u64 + 10)
        .map(|i| assistant_msg(&format!("o{i}"), 1 + i, S::Completed, "x"))
        .collect();
    look_above_oldest(&mut s);
    s.apply(view_result(older(&s), page(9001, older_msgs, true)));
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
            conversation_id: CID.into(),
            turn_id: "other".into(),
            state: pb::TurnState::Completed as i32,
            ..Default::default()
        }),
    ));
    s.apply(persisted(
        12,
        &[PENDING],
        E::ApprovalDecided(pb::ApprovalDecided {
            conversation_id: CID.into(),
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
            conversation_id: CID.into(),
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
            released_approvals: None,
        }),
    ));
    assert_eq!(row_ids(&s), ["note:10"]);
    s.apply(snapshot(CONV, 20, vec![page(20, vec![], false)]));
    assert!(row_ids(&s).is_empty());
}

/// 审查附注：「回到最新」（重建）先到、更早发出的翻页结果后到时，翻页结果不接上去，时间线中间不留空洞。
#[test]
fn stale_older_page_is_dropped_after_rebuild() {
    let mut s = store_with_open();
    s.apply(snapshot(
        CONV,
        10,
        vec![page(
            10,
            vec![assistant_msg("a", 10, S::Completed, "x")],
            true,
        )],
    ));
    let stale = older(&s); // before = "a"
    s.apply(snapshot(
        CONV,
        50,
        vec![page(
            50,
            vec![assistant_msg("z", 50, S::Completed, "x")],
            true,
        )],
    ));
    s.apply(view_result(
        stale,
        page(51, vec![assistant_msg("old", 5, S::Completed, "x")], false),
    ));
    assert_eq!(
        row_ids(&s),
        ["z"],
        "翻页结果的 before 已不是最旧的一行：丢掉"
    );
    // 对照：before 对得上就接上。
    let fresh = older(&s);
    s.apply(view_result(
        fresh,
        page(52, vec![assistant_msg("y", 40, S::Completed, "x")], false),
    ));
    assert_eq!(row_ids(&s), ["y", "z"]);
}

/// 审查附注：时间线不接着最新时，新回合不插模型切换行。
#[test]
fn detached_timeline_gets_no_model_switch_row() {
    let mut s = store_with_open();
    let turn = |id: &str, model: &str| {
        E::TurnQueued(pb::TurnQueued {
            turn: Some(pb::Turn {
                id: id.into(),
                conversation_id: CID.into(),
                model: Some(pb::ModelSelection {
                    model_id: model.into(),
                    effort: None,
                }),
                state: pb::TurnState::Queued as i32,
                ..Default::default()
            }),
        })
    };
    s.apply(snapshot(
        CONV,
        9000,
        vec![page(
            9000,
            vec![assistant_msg("tail", 9000, S::Completed, "t")],
            true,
        )],
    ));
    s.apply(persisted(9001, &[CONV], turn("t1", "a")));
    let older_msgs: Vec<pb::Message> = (0..TIMELINE_CAP as u64 + 1)
        .map(|i| assistant_msg(&format!("o{i}"), 1 + i, S::Completed, "x"))
        .collect();
    look_above_oldest(&mut s);
    s.apply(view_result(older(&s), page(9002, older_msgs, true)));
    assert!(!s.open.as_ref().unwrap().at_tail);
    s.apply(persisted(9003, &[CONV], turn("t2", "b")));
    assert!(s.open.as_ref().unwrap().row("model:t2").is_none());
}

/// 审查 1/F-8：一次重连的四张摘要表分在几个 Subscribed 里，收齐且期间没有命中摘要的事件才清标记。
#[test]
fn snapshots_across_subscribed_clear_stale_only_without_events_between() {
    use iwork_client_core::store::SummaryTable;
    fn results(tables: &[SummaryTable]) -> Vec<pb::QueryResult> {
        use pb::query_result::Kind as K;
        tables
            .iter()
            .map(|t| pb::QueryResult {
                seq: 20,
                kind: Some(match t {
                    SummaryTable::Projects => K::ListProjects(Default::default()),
                    SummaryTable::Conversations => K::ListConversations(Default::default()),
                    SummaryTable::Assistants => K::ListAssistants(Default::default()),
                    SummaryTable::Pending => K::ListPendingItems(Default::default()),
                    SummaryTable::List | SummaryTable::Anchors => unreachable!(),
                }),
            })
            .collect()
    }
    let side = [
        SummaryTable::Projects,
        SummaryTable::Conversations,
        SummaryTable::Assistants,
    ];
    for event_between in [false, true] {
        let mut s = store_with_open();
        s.apply(persisted(
            5,
            &[SIDEBAR],
            E::ConversationRead(pb::ConversationRead::default()),
        ));
        assert!(s.refresh.is_stale());
        s.resubscribing();
        s.apply(snapshot(SIDEBAR, 20, results(&side)));
        if event_between {
            s.apply(persisted(
                21,
                &[PENDING],
                E::ConversationRead(pb::ConversationRead::default()),
            ));
        }
        s.apply(snapshot(PENDING, 21, results(&[SummaryTable::Pending])));
        assert_eq!(
            s.refresh.is_stale(),
            event_between,
            "期间有事件：{event_between}"
        );
    }
}

/// 审查 2 附注：截掉一端时，回合、附件、工具调用索引这些旁表跟着变小，不随对话一直涨。
#[test]
fn trimming_also_trims_side_tables() {
    use iwork_client_core::store::timeline::Timeline;
    let mut s = store_with_open();
    s.open = Some(Timeline::with_cap(CID, 20));
    for i in 0..100u64 {
        let seq = 10 + i * 5;
        let turn = format!("t{i}");
        s.apply(persisted(
            seq,
            &[CONV],
            E::TurnQueued(pb::TurnQueued {
                turn: Some(pb::Turn {
                    id: turn.clone(),
                    conversation_id: CID.into(),
                    ..Default::default()
                }),
            }),
        ));
        s.apply(persisted(
            seq + 1,
            &[CONV],
            E::ToolCallStarted(pb::ToolCallStarted {
                turn_id: turn.clone(),
                tool_call_id: format!("tc{i}"),
                ..Default::default()
            }),
        ));
        s.apply(persisted(
            seq + 2,
            &[CONV],
            E::AttachmentStored(pb::AttachmentStored {
                attachment: Some(pb::Attachment {
                    id: format!("a{i}"),
                    conversation_id: CID.into(),
                    ..Default::default()
                }),
            }),
        ));
        let mut m = assistant_msg(&format!("m{i}"), 0, S::Completed, "x");
        m.attachment_ids = vec![format!("a{i}")];
        s.apply(persisted(seq + 3, &[CONV], appended(m)));
        s.apply(persisted(
            seq + 4,
            &[CONV],
            E::TurnFinished(pb::TurnFinished {
                conversation_id: CID.into(),
                turn_id: turn,
                state: pb::TurnState::Completed as i32,
                ..Default::default()
            }),
        ));
    }
    let tl = s.open.as_ref().unwrap();
    assert!(tl.len() <= 20);
    let (turns, attachments, tools) = tl.side_table_sizes();
    assert!(
        turns <= 20 && attachments <= 20 && tools <= 20,
        "旁表 {turns}/{attachments}/{tools} 没跟着截"
    );
}

/// 审查 3/F-1：往上翻超过上限、从新的一端截掉回合结束行时，回合条目留着：这个回合更早的行还在，
/// 前面的模型切换行也还在。
#[test]
fn trimming_newest_end_keeps_turn_entries() {
    let turn = |id: &str, model: &str, created: u64, ended: u64| pb::Turn {
        id: id.into(),
        conversation_id: CID.into(),
        model: Some(pb::ModelSelection {
            model_id: model.into(),
            effort: None,
        }),
        state: pb::TurnState::Completed as i32,
        created_seq: created,
        ended_seq: ended,
        ..Default::default()
    };
    let mut m2 = assistant_msg("m2", 8995, S::Completed, "x");
    m2.turn_id = Some("t2".into());
    let mut s = store_with_open();
    s.apply(snapshot(
        CONV,
        9001,
        vec![pb::QueryResult {
            seq: 9001,
            kind: Some(pb::query_result::Kind::GetConversation(
                pb::GetConversationResult {
                    conversation: Some(conversation(CID, "", 0)),
                    messages: vec![assistant_msg("m1", 8980, S::Completed, "x"), m2],
                    turns: vec![turn("t1", "a", 8970, 8975), turn("t2", "b", 8990, 9000)],
                    has_more: true,
                    ..Default::default()
                },
            )),
        }],
    ));
    // 行：end:t1、m1、model:t2、m2、end:t2 共 5 行。往前翻一页，正好多出一行：从新的一端截掉 end:t2。
    let before = row_ids(&s).len();
    assert_eq!(before, 5);
    let older_msgs: Vec<pb::Message> = (0..(TIMELINE_CAP - before + 1) as u64)
        .map(|i| assistant_msg(&format!("o{i}"), 1 + i, S::Completed, "x"))
        .collect();
    look_above_oldest(&mut s);
    s.apply(view_result(older(&s), page(9002, older_msgs, true)));
    let tl = s.open.as_ref().unwrap();
    assert!(tl.row("end:t2").is_none(), "对照：结束行确实被截掉了");
    assert!(tl.row("m2").is_some());
    assert!(tl.turns.contains_key("t2"), "回合条目还在");
    assert!(tl.row("model:t2").is_some(), "模型切换行还在");
}

/// 网页往上翻时声明的窗口：顶在已加载的最旧一行之上（往前翻回来的行都落在窗口里）。方案 2.2 第 4 条「丢哪一端」
/// 按窗口定：窗口在旧的一侧，往前翻超上限时丢的是新的一端。
fn look_above_oldest(s: &mut iwork_client_core::store::Store) {
    let first = s.open.as_ref().unwrap().identities()[0].0.ordinal();
    s.set_window(CID, i64::MIN, first - 1, false);
}
