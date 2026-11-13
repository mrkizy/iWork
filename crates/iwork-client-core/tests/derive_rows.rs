//! 每种行每种状态一条（方案第 7 节「derive 每种行每种状态一条（两个方向都判）」）：文字与可否点击来自 derive。

mod support;

use iwork_client_core::derive::row::row;
use iwork_client_core::store::{Connection, Row, Store};
use iwork_proto::pb;
use pb::domain_event::Event as E;
use pb::row_view::Row as V;
use support::*;

fn live_store() -> Store {
    let mut s = store_with_open();
    s.set_connection(Connection::Live);
    s
}

fn view(s: &Store, id: &str) -> V {
    let tl = s.open.as_ref().unwrap();
    let key = tl.key_of(id).unwrap();
    row(s, tl, key, id, tl.row(id).unwrap()).row.unwrap()
}

fn request(s: &mut Store, seq: u64, a: pb::Approval) {
    s.apply(persisted(
        seq,
        &[CONV],
        E::ApprovalRequested(pb::ApprovalRequested { approval: Some(a) }),
    ));
}

fn approval(offered: Vec<pb::RememberScope>) -> pb::Approval {
    pb::Approval {
        id: "ap".into(),
        conversation_id: CID.into(),
        kind: pb::ApprovalKind::Ask as i32,
        state: pb::ApprovalState::Pending as i32,
        action: Some(pb::ActionSummary {
            raw_summary: "npm test".into(),
            out_of_bounds: true,
            ..Default::default()
        }),
        offered_remember: offered.into_iter().map(|s| s as i32).collect(),
        remember_label: "本项目内 `npm test …` 不再问".into(),
        ..Default::default()
    }
}

fn approval_row(s: &Store) -> pb::ApprovalRow {
    match view(s, "ap") {
        V::Approval(a) => a,
        other => panic!("{other:?}"),
    }
}

#[test]
fn approval_pending_can_decide_only_when_live() {
    let mut s = live_store();
    request(
        &mut s,
        10,
        approval(vec![pb::RememberScope::Once, pb::RememberScope::Project]),
    );
    let a = approval_row(&s);
    assert!(a.can_decide);
    assert_eq!(a.tags, ["越界"]);
    assert_eq!(
        a.remember_options
            .iter()
            .map(|o| o.label.as_str())
            .collect::<Vec<_>>(),
        ["仅这一次", "本项目"]
    );
    assert_eq!(a.state_label, "");
    s.set_connection(Connection::Reconnecting);
    assert!(!approval_row(&s).can_decide, "断线时不可点");
}

#[test]
fn approval_must_ask_has_no_remember() {
    let mut s = live_store();
    request(&mut s, 10, approval(vec![]));
    let a = approval_row(&s);
    assert!(a.remember_options.is_empty());
    assert!(a.can_decide);
}

#[test]
fn approval_terminal_states() {
    let cases: Vec<(E, &str, bool, bool)> = vec![
        (
            E::ApprovalDecided(pb::ApprovalDecided {
                conversation_id: CID.into(),
                approval_id: "ap".into(),
                state: pb::ApprovalState::Approved as i32,
                decider: pb::ApprovalDecider::User as i32,
                remember: None,
            }),
            "已允许",
            false,
            false,
        ),
        (
            E::ApprovalDecided(pb::ApprovalDecided {
                conversation_id: CID.into(),
                approval_id: "ap".into(),
                state: pb::ApprovalState::Approved as i32,
                decider: pb::ApprovalDecider::ModeSwitch as i32,
                remember: None,
            }),
            "切换档位后已放行",
            false,
            true,
        ),
        (
            E::ApprovalDecided(pb::ApprovalDecided {
                conversation_id: CID.into(),
                approval_id: "ap".into(),
                state: pb::ApprovalState::Denied as i32,
                decider: pb::ApprovalDecider::User as i32,
                remember: None,
            }),
            "已拒绝",
            false,
            false,
        ),
        (
            E::ApprovalVoided(pb::ApprovalVoided {
                conversation_id: CID.into(),
                approval_id: "ap".into(),
                reason: pb::VoidReason::TurnCancelled as i32,
            }),
            "已作废：回合已取消",
            true,
            false,
        ),
    ];
    for (event, label, muted, folded) in cases {
        let mut s = live_store();
        request(&mut s, 10, approval(vec![pb::RememberScope::Once]));
        s.apply(persisted(11, &[CONV], event));
        let a = approval_row(&s);
        assert_eq!(a.state_label, label);
        assert!(!a.can_decide, "{label}：终态不可点");
        assert!(a.remember_options.is_empty(), "{label}");
        assert_eq!((a.muted, a.folded), (muted, folded), "{label}");
    }
}

fn card(state: pb::UserActionState) -> pb::UserActionCard {
    pb::UserActionCard {
        id: "card".into(),
        conversation_id: CID.into(),
        content: Some(pb::user_action_card::Content::Command(
            pb::CommandProposal {
                command: "ssh-add -l".into(),
                cwd: "/".into(),
            },
        )),
        reason: "列出密钥".into(),
        state: state as i32,
        ..Default::default()
    }
}

#[test]
fn user_action_states() {
    use pb::UserActionState as S;
    // (状态, 文字, 可执行, 可忽略, 可停, 可核对, 有说明)
    let cases = [
        (S::Pending, "", true, true, false, false, false),
        (S::Executing, "执行中", false, false, true, false, false),
        (S::Dismissed, "已忽略", false, false, false, false, false),
        (S::Unknown, "执行结果未知", false, false, false, true, true),
        (S::Ended, "你已核对", false, false, false, false, false),
    ];
    for (state, label, exec, dismiss, stop, confirm, notice) in cases {
        let mut s = live_store();
        s.apply(persisted(
            10,
            &[CONV],
            E::UserActionProposed(pb::UserActionProposed {
                card: Some(card(state)),
            }),
        ));
        let V::UserAction(c) = view(&s, "card") else {
            panic!()
        };
        assert_eq!(c.state_label, label);
        assert_eq!(
            (
                c.can_execute,
                c.can_dismiss,
                c.can_stop,
                c.can_confirm_ended,
                c.notice.is_some()
            ),
            (exec, dismiss, stop, confirm, notice),
            "{state:?}"
        );
        assert!(c.can_copy);
    }
    // 执行完：退出码。
    let mut s = live_store();
    s.apply(persisted(
        10,
        &[CONV],
        E::UserActionProposed(pb::UserActionProposed {
            card: Some(card(S::Pending)),
        }),
    ));
    s.apply(persisted(
        11,
        &[CONV],
        E::UserActionStateChanged(pb::UserActionStateChanged {
            card_id: "card".into(),
            state: S::Done as i32,
            exit: Some(pb::ExitStatus {
                kind: Some(pb::exit_status::Kind::Code(2)),
            }),
            ..Default::default()
        }),
    ));
    let V::UserAction(c) = view(&s, "card") else {
        panic!()
    };
    assert_eq!(c.exit_label.as_deref(), Some("退出码 2"));
    assert_eq!(c.state_label, "你已执行");
}

/// 对话停在「删除中」时，结果未知的卡片提示「确认后继续删除」。
#[test]
fn unknown_card_while_deleting() {
    let mut s = live_store();
    s.apply(snapshot(CONV, 5, vec![page(5, vec![], false)]));
    s.apply(persisted(
        10,
        &[CONV],
        E::UserActionProposed(pb::UserActionProposed {
            card: Some(card(pb::UserActionState::Unknown)),
        }),
    ));
    s.apply(persisted(
        11,
        &[CONV],
        E::ConversationDeleting(pb::ConversationDeleting {
            conversation_id: CID.into(),
        }),
    ));
    let V::UserAction(c) = view(&s, "card") else {
        panic!()
    };
    assert_eq!(
        c.notice.as_deref(),
        Some("有一张卡片的执行结果无法核对，请确认后继续删除")
    );
}

fn queue_turn(s: &mut Store, seq: u64, id: &str, model: &str) {
    s.apply(persisted(
        seq,
        &[CONV],
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
        }),
    ));
}

#[test]
fn turn_end_celebrate_failed_cancelled() {
    let cases = [
        (
            pb::TurnState::Completed,
            true,
            Some(2),
            "改了 2 个文件 +5 −1",
            "",
        ),
        (pb::TurnState::Completed, false, None, "", ""),
        (pb::TurnState::Failed, false, None, "", "失败"),
        (pb::TurnState::Cancelled, false, None, "", "已取消"),
    ];
    for (state, celebrate, files, summary, outcome) in cases {
        let mut s = live_store();
        queue_turn(&mut s, 10, "t1", "opus");
        s.apply(persisted(
            20,
            &[CONV],
            E::TurnFinished(pb::TurnFinished {
                conversation_id: CID.into(),
                turn_id: "t1".into(),
                state: state as i32,
                usage: Some(pb::Usage {
                    input_tokens: 10,
                    output_tokens: 5,
                    ..Default::default()
                }),
                celebrate,
                change_summary: files.map(|n| pb::ChangeSummary {
                    files_changed: n,
                    insertions: 5,
                    deletions: 1,
                    paths: vec![],
                }),
            }),
        ));
        let V::TurnEnd(t) = view(&s, "end:t1") else {
            panic!()
        };
        assert_eq!(
            (
                t.celebrate,
                t.summary_line.as_str(),
                t.outcome_label.as_str()
            ),
            (celebrate, summary, outcome),
            "{state:?}"
        );
        assert_eq!(t.failed, state == pb::TurnState::Failed);
    }
}

/// R9：印章只有回合结束行带；审批、亲手执行、继续卡片的行类型里没有这个字段。
#[test]
fn only_turn_end_row_has_stamp_field() {
    let pool = <pb::RowView as prost::Name>::full_name();
    assert_eq!(pool, "iwork.v1.RowView");
    let fds = prost_types::FileDescriptorSet::decode(iwork_proto::FILE_DESCRIPTOR_SET).unwrap();
    let msgs: Vec<_> = fds
        .file
        .iter()
        .flat_map(|f| f.message_type.iter())
        .collect();
    for name in ["ApprovalRow", "UserActionRow", "ResumeRow"] {
        let m = msgs.iter().find(|m| m.name() == name).unwrap();
        assert!(m.field.iter().all(|f| f.name() != "celebrate"), "{name}");
    }
    let end = msgs.iter().find(|m| m.name() == "TurnEndRow").unwrap();
    assert!(
        end.field.iter().any(|f| f.name() == "celebrate"),
        "对照：回合结束行有印章"
    );
}

#[test]
fn model_switch_between_turns_and_note_suppresses_it() {
    let mut s = live_store();
    queue_turn(&mut s, 10, "t1", "opus");
    queue_turn(&mut s, 20, "t2", "sonnet");
    assert!(
        s.open.as_ref().unwrap().row("model:t2").is_some(),
        "相邻回合模型不同：贴说明"
    );
    // 关掉重开（走快照）后时间线按查询结果重建，说明行仍在、位置相同。
    let mut s2 = live_store();
    queue_turn(&mut s2, 10, "t1", "opus");
    s2.apply(persisted(
        15,
        &[CONV],
        E::ConversationModelChanged(pb::ConversationModelChanged {
            conversation_id: CID.into(),
            model: Some(pb::ModelSelection {
                model_id: "sonnet".into(),
                effort: None,
            }),
        }),
    ));
    queue_turn(&mut s2, 20, "t2", "sonnet");
    let tl = s2.open.as_ref().unwrap();
    assert!(
        tl.row("model:t2").is_none(),
        "当次说明已说过，不再贴重建得出的那一行"
    );
    assert!(matches!(tl.row("note:15"), Some(Row::Note { .. })));
}

/// 同一件事一个样子（审查 2a 2/F-4）：切了项目、切了模型，不管是当次说明（事件带来的）还是重建得出的那一行，
/// 网页拿到的视图种类都一样（分隔线、模型切换行）；只有档位切换是普通说明行。
#[test]
fn switch_notes_render_like_their_rebuilt_rows() {
    let mut s = live_store();
    queue_turn(&mut s, 10, "t1", "opus");
    s.apply(persisted(
        15,
        &[CONV],
        E::ConversationModelChanged(pb::ConversationModelChanged {
            conversation_id: CID.into(),
            model: Some(pb::ModelSelection {
                model_id: "sonnet".into(),
                effort: None,
            }),
        }),
    ));
    s.apply(persisted(
        16,
        &[CONV],
        E::DmProjectSwitched(pb::DmProjectSwitched {
            conversation_id: CID.into(),
            project_id: Some(PID.into()),
        }),
    ));
    s.apply(persisted(
        17,
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
    assert!(matches!(view(&s, "note:15"), V::ModelSwitch(_)), "切了模型");
    assert!(matches!(view(&s, "note:16"), V::Divider(_)), "切了项目");
    assert!(
        matches!(view(&s, "note:17"), V::Note(_)),
        "对照：切档位是说明行"
    );
}

#[test]
fn resume_row_from_pending_table() {
    let mut s = live_store();
    s.apply(snapshot(
        PENDING,
        30,
        vec![pb::QueryResult {
            seq: 30,
            kind: Some(pb::query_result::Kind::ListPendingItems(
                pb::ListPendingItemsResult {
                    items: vec![pb::PendingItem {
                        item: Some(pb::pending_item::Item::Resume(pb::ResumeCard {
                            turn_id: "t1".into(),
                            conversation_id: CID.into(),
                            reason: pb::InterruptReason::Crash as i32,
                            created_seq: 25,
                        })),
                    }],
                },
            )),
        }],
    ));
    let V::Resume(r) = view(&s, "resume:t1") else {
        panic!()
    };
    assert!(r.can_resume && r.can_abandon);
    assert_eq!(r.reason, "后台服务意外中断，回合停在这里");
}

use prost::Message as _;
