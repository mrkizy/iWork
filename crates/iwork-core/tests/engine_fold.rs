//! 审批引擎的折叠（T07 方案 6.10.2 折叠表、6.10.6 第 8 条、6.10.8 的 R1–R2）。直接喂事件，期望值手写。

#[path = "support/fold_fx.rs"]
mod fx;
#[path = "../../iwork-ports/tests/reference_projection/mod.rs"]
mod reference_projection;

use fx::*;
use iwork_core::approval::denial::DenialPhase;
use iwork_core::approval::engine::fold::{Effect, FoldError, Folded, VoidTarget};
use iwork_core::approval::engine::{ConversationModes, MSG_TURN_CANCELLED, MSG_USER_DENIED};
use iwork_domain::approval::{
    ApprovalState, ConversationPresence, Decider, OwnerFacts, Presence, ReviewState, VoidReason,
    owner_alive, void_reason_for,
};
use iwork_domain::approval_mode::{ApprovalMode, ProjectDefaultMode};
use iwork_domain::event::{
    ApprovalModeTarget, DmProjectSwitched, DomainEvent, ProjectSettingsUpdated, RuleRemembered,
    RuleRevoked, RulesDeleted, SecurityListEntryAdded,
};
use iwork_domain::id::{ReviewId, Seq};
use iwork_domain::project::ProjectSettings;
use iwork_domain::rule::{DenyReason, Verdict};
use iwork_domain::turn::TurnState;
use iwork_domain::user_action::{ExitStatus, UserActionState};
use iwork_ports::agent::PermissionDecision;
use iwork_ports::store::StoredEvent;

/// 按 1, 2, 3… 的序号依次应用，返回折叠与每条事件的效果。
fn fold(events: &[DomainEvent]) -> (Folded, Vec<Vec<Effect>>) {
    let mut f = Folded::default();
    let effects = events
        .iter()
        .enumerate()
        .map(|(i, e)| f.apply(Seq(i as u64 + 1), e).expect("合法的序列"))
        .collect();
    (f, effects)
}

fn more(f: &mut Folded, events: &[DomainEvent]) -> Vec<Vec<Effect>> {
    events
        .iter()
        .map(|e| {
            let seq = Seq(f.cursor().0 + 1);
            f.apply(seq, e).expect("合法的序列")
        })
        .collect()
}

fn deny(msg: &str) -> PermissionDecision {
    PermissionDecision::Deny {
        message: msg.into(),
    }
}

/// 项目 1（手动）、助手 1、项目对话 10、回合 20 在等审批。
fn base() -> Vec<DomainEvent> {
    vec![
        e_project(1, ProjectDefaultMode::Manual),
        e_assistant(1),
        e_conversation(project_conversation(10, 1)),
        e_turn(20, 10, 1),
        e_waiting(20),
    ]
}

fn with(extra: &[DomainEvent]) -> Vec<DomainEvent> {
    let mut v = base();
    v.extend_from_slice(extra);
    v
}

// ---------- 折叠表逐行 ----------

#[test]
fn project_default_feeds_new_conversations() {
    let (f, _) = fold(&[
        e_project(1, ProjectDefaultMode::Auto),
        e_conversation(project_conversation(10, 1)),
        ProjectSettingsUpdated {
            project_id: pid(1),
            previous: Some(ProjectSettings::default()),
            settings: ProjectSettings::default(),
        }
        .into(),
        e_conversation(project_conversation(11, 1)),
        e_mode(
            ApprovalModeTarget::Project(pid(1)),
            Some(ApprovalMode::Auto),
        ),
        e_conversation(project_conversation(12, 1)),
        // 没登记过的项目：手动
        e_conversation(project_conversation(13, 9)),
    ]);
    let mode = |c| f.conversation(cid(c)).unwrap().effective_mode(aid(1));
    assert_eq!(
        [mode(10), mode(11), mode(12), mode(13)],
        [
            ApprovalMode::Auto,
            ApprovalMode::Manual,
            ApprovalMode::Auto,
            ApprovalMode::Manual
        ],
        "项目默认只在建对话那一刻取值"
    );
}

#[test]
fn snapshot_inputs_seq_tracks_lists_projects_assistants() {
    let (f, _) = fold(&[e_project(1, ProjectDefaultMode::Manual)]);
    assert_eq!(f.snapshot_inputs_seq(), Seq(1));
    let (f, _) = fold(&[
        e_project(1, ProjectDefaultMode::Manual),
        e_assistant(1),
        SecurityListEntryAdded { rule: rule(1) }.into(),
        // 改项目设置不让快照过期
        ProjectSettingsUpdated {
            project_id: pid(1),
            previous: Some(ProjectSettings::default()),
            settings: ProjectSettings::default(),
        }
        .into(),
    ]);
    assert_eq!(f.snapshot_inputs_seq(), Seq(3));
    let (f, _) = fold(&[e_assistant(1), e_assistant_deleted(1)]);
    assert_eq!(f.snapshot_inputs_seq(), Seq(2));
}

#[test]
fn conversation_modes_from_events() {
    let mut dm_auto = dm(11, 1, Some(1));
    dm_auto.approval_mode = None;
    let (mut f, _) = fold(&[
        e_project(1, ProjectDefaultMode::Auto),
        e_project(2, ProjectDefaultMode::Manual),
        e_assistant(1),
        e_assistant(2),
        e_conversation(project_conversation(10, 1)),
        e_conversation(dm_auto),
    ]);
    assert_eq!(
        f.conversation(cid(11)).unwrap().modes,
        ConversationModes::Dm {
            own_setting: None,
            selected_project_default: Some(ProjectDefaultMode::Auto)
        }
    );
    more(
        &mut f,
        &[
            e_mode(
                ApprovalModeTarget::Member {
                    conversation_id: cid(10),
                    assistant_id: aid(2),
                },
                Some(ApprovalMode::FullAccess),
            ),
            e_mode(
                ApprovalModeTarget::Conversation(cid(10)),
                Some(ApprovalMode::Manual),
            ),
            DmProjectSwitched {
                conversation_id: cid(11),
                project_id: Some(pid(2)),
            }
            .into(),
        ],
    );
    let c10 = f.conversation(cid(10)).unwrap();
    assert_eq!(c10.effective_mode(aid(1)), ApprovalMode::Manual);
    assert_eq!(c10.effective_mode(aid(2)), ApprovalMode::FullAccess);
    assert_eq!(
        f.conversation(cid(11)).unwrap().effective_mode(aid(1)),
        ApprovalMode::Manual,
        "切到手动项目，私聊自己的设置清空"
    );
}

#[test]
fn approval_requested_detached_by_turn_state() {
    let (f, _) = fold(&with(&[
        e_requested(approval(1, 20, 10, "a")),
        e_turn(21, 10, 1),
        e_requested(approval(2, 21, 10, "b")),
    ]));
    assert!(!f.pending_approval(apid(1)).unwrap().detached);
    assert!(
        f.pending_approval(apid(2)).unwrap().detached,
        "回合不在等审批时建的审批：编排不会采纳它的结果"
    );
    assert_eq!(f.approval_conversation(apid(1)), Some(cid(10)));
}

#[test]
fn leaving_waiting_marks_detached() {
    let (mut f, _) = fold(&with(&[e_requested(approval(1, 20, 10, "a"))]));
    more(&mut f, &[e_running(20)]);
    assert!(f.pending_approval(apid(1)).unwrap().detached);
    // 只进不出：再回到等审批也还是 detached
    more(&mut f, &[e_waiting(20)]);
    assert!(f.pending_approval(apid(1)).unwrap().detached);
}

#[test]
fn review_updated_stops_review_when_not_in_progress() {
    let rid = ReviewId(tid(99));
    let (_, effects) = fold(&with(&[
        e_requested(approval(1, 20, 10, "a")),
        e_review(1, ReviewState::InProgress { review_id: rid }),
        e_review(1, ReviewState::TimedOut { review_id: rid }),
    ]));
    let n = effects.len();
    assert_eq!(effects[n - 2], vec![], "开始审查没有效果");
    assert_eq!(
        effects[n - 1],
        vec![Effect::StopReview { approval: apid(1) }]
    );
}

#[test]
fn denied_records_denial_and_answers() {
    let (f, effects) = fold(&with(&[e_requested(approval(1, 20, 10, "a")), e_denied(1)]));
    assert_eq!(
        effects.last().unwrap(),
        &vec![Effect::Answer {
            approval: apid(1),
            decision: deny(MSG_USER_DENIED)
        }]
    );
    let conv = f.conversation(cid(10)).unwrap();
    assert_eq!(
        conv.denial.get(&exec("a").fingerprint()),
        Some(DenialPhase::AutoDeny)
    );
    assert!(f.pending_approval(apid(1)).is_none());
    assert_eq!(f.approval_conversation(apid(1)), None);
}

#[test]
fn user_message_flips_denial_and_user_allow_clears() {
    let (mut f, _) = fold(&with(&[
        e_requested(approval(1, 20, 10, "a")),
        e_denied(1),
        e_user_message(1, 10),
    ]));
    let fp = exec("a").fingerprint();
    assert_eq!(
        f.conversation(cid(10)).unwrap().denial.get(&fp),
        Some(DenialPhase::ForceAskOnce)
    );
    // AI 审查放行不清强制询问；用户本人允许才清
    more(
        &mut f,
        &[
            e_requested(approval(2, 20, 10, "a")),
            e_decided(
                2,
                ApprovalState::Approved {
                    decider: Decider::AiReview,
                },
            ),
        ],
    );
    assert_eq!(
        f.conversation(cid(10)).unwrap().denial.get(&fp),
        Some(DenialPhase::ForceAskOnce)
    );
    more(
        &mut f,
        &[e_requested(approval(3, 20, 10, "a")), e_allowed(3)],
    );
    assert_eq!(f.conversation(cid(10)).unwrap().denial.get(&fp), None);
}

#[test]
fn approved_goes_to_awaiting_delivery_or_grants() {
    let (f, effects) = fold(&with(&[
        e_requested(approval(1, 20, 10, "a")),
        e_allowed(1),
    ]));
    assert_eq!(
        effects.last().unwrap(),
        &vec![Effect::Answer {
            approval: apid(1),
            decision: PermissionDecision::Allow
        }]
    );
    assert!(f.awaiting_delivery(apid(1)).is_some());
    assert!(f.grant(apid(1)).is_none());

    // detached 的审批被批准：直接是一次性放行，挂着的 hook（万一还有）收到「回合已取消」
    let (f, effects) = fold(&with(&[
        e_requested(approval(1, 20, 10, "a")),
        e_interrupted(20),
        e_allowed(1),
    ]));
    assert_eq!(
        effects.last().unwrap(),
        &vec![Effect::Answer {
            approval: apid(1),
            decision: deny(MSG_TURN_CANCELLED)
        }]
    );
    assert_eq!(f.grant(apid(1)).unwrap().record, record("a"));
    assert!(f.awaiting_delivery(apid(1)).is_none());
}

#[test]
fn voided_answers_and_stops_review() {
    let (f, effects) = fold(&with(&[
        e_requested(approval(1, 20, 10, "a")),
        e_voided(1, VoidReason::TurnCancelled),
    ]));
    assert_eq!(
        effects.last().unwrap(),
        &vec![
            Effect::Answer {
                approval: apid(1),
                decision: deny(MSG_TURN_CANCELLED)
            },
            Effect::StopReview { approval: apid(1) }
        ]
    );
    assert!(f.pending_approval(apid(1)).is_none());
}

#[test]
fn delivered_removes_awaiting_and_ignores_others() {
    let (f, _) = fold(&with(&[
        e_requested(approval(1, 20, 10, "a")),
        e_requested(approval(2, 20, 10, "b")),
        e_allowed(1),
        e_denied(2),
        e_delivered(1, 20),
        // 被拒绝的审批交付：什么都不做
        e_delivered(2, 20),
    ]));
    assert!(f.awaiting_delivery(apid(1)).is_none());
    assert!(f.grant(apid(1)).is_none());
}

/// 9/F-6：批准 → 中断（挪进放行）→ 交付同一条：交付过就是用掉了，放行不留。
#[test]
fn delivered_after_interrupt_drops_grant() {
    let (f, _) = fold(&with(&[
        e_requested(approval(1, 20, 10, "a")),
        e_allowed(1),
        e_interrupted(20),
    ]));
    assert!(f.grant(apid(1)).is_some(), "对照：中断时挪进了放行");
    let (f, _) = fold(&with(&[
        e_requested(approval(1, 20, 10, "a")),
        e_allowed(1),
        e_interrupted(20),
        e_delivered(1, 20),
    ]));
    assert!(f.grant(apid(1)).is_none());
    assert!(f.awaiting_delivery(apid(1)).is_none());
    assert!(
        f.turns_to_resume()
            .into_iter()
            .map(|(t, _)| t)
            .collect::<Vec<_>>()
            .is_empty()
    );
}

#[test]
fn turn_finished_closes_and_keeps_owner() {
    let (f, effects) = fold(&with(&[
        e_requested(approval(1, 20, 10, "a")),
        e_allowed(1),
        e_turn_finished(20, TurnState::Completed),
    ]));
    assert_eq!(
        effects.last().unwrap(),
        &vec![Effect::TurnClosed { turn: tid(20) }]
    );
    assert!(f.awaiting_delivery(apid(1)).is_none());
    assert!(!f.is_waiting(tid(20)));
    assert_eq!(
        f.owner_facts(tid(20)).turn,
        Presence::Present,
        "回合结束不等于行没了"
    );
}

#[test]
fn conversation_deleted_drops_everything() {
    let (f, effects) = fold(&with(&[
        e_requested(approval(1, 20, 10, "a")),
        e_card(card(5, 20, 10)),
        e_deleting(10),
        e_deleted(10),
    ]));
    assert_eq!(
        effects.last().unwrap(),
        &vec![
            Effect::Answer {
                approval: apid(1),
                decision: deny(MSG_TURN_CANCELLED)
            },
            Effect::CardSettled {
                card: uaid(5),
                message: MSG_TURN_CANCELLED.into()
            },
            Effect::ConversationGone {
                conversation: cid(10)
            },
        ]
    );
    assert!(f.conversation(cid(10)).is_none());
    assert_eq!(f.approval_conversation(apid(1)), None);
    assert_eq!(f.card_conversation(uaid(5)), None);
    assert_eq!(f.turn_owner(tid(20)), None);
    assert!(!f.is_waiting(tid(20)));
}

#[test]
fn assistant_deleted_drops_its_dm_only() {
    let (f, effects) = fold(&with(&[
        e_conversation(dm(11, 1, None)),
        e_turn(30, 11, 1),
        e_waiting(30),
        e_requested(approval(1, 20, 10, "a")),
        e_requested(approval(2, 30, 11, "b")),
        e_assistant_deleted(1),
    ]));
    assert!(!f.has_assistant(aid(1)));
    assert!(f.conversation(cid(11)).is_none(), "私聊整条消失");
    assert!(effects.last().unwrap().contains(&Effect::ConversationGone {
        conversation: cid(11)
    }));
    assert!(
        f.pending_approval(apid(1)).is_some(),
        "项目对话里的待审批等作废事件"
    );
    assert_eq!(f.turn_owner(tid(30)), None);
}

#[test]
fn cards_settle_once_and_leave_at_terminal() {
    let (mut f, effects) = fold(&with(&[
        e_card(card(5, 20, 10)),
        e_card_state(5, UserActionState::Starting),
        e_card_state(5, UserActionState::Executing),
        e_card_state(
            5,
            UserActionState::Unknown {
                error: "后台服务中断".into(),
            },
        ),
    ]));
    let n = effects.len();
    assert_eq!(effects[n - 3], vec![]);
    assert_eq!(effects[n - 2], vec![]);
    assert_eq!(
        effects[n - 1],
        vec![Effect::CardSettled {
            card: uaid(5),
            message: "用户已执行，结果未知".into()
        }]
    );
    assert_eq!(
        f.card_conversation(uaid(5)),
        Some(cid(10)),
        "Unknown 不是终态"
    );
    let ended = more(&mut f, &[e_card_state(5, UserActionState::Ended)]);
    assert_eq!(ended, vec![vec![]], "已经带过话，不再重复");
    assert_eq!(f.card_conversation(uaid(5)), None);

    let (f, effects) = fold(&with(&[
        e_card(card(6, 20, 10)),
        e_card_state(6, UserActionState::Starting),
        e_card_state(6, UserActionState::Executing),
        e_card_state(
            6,
            UserActionState::Done {
                exit: ExitStatus::Code(3),
            },
        ),
    ]));
    assert_eq!(
        effects.last().unwrap(),
        &vec![Effect::CardSettled {
            card: uaid(6),
            message: "用户已执行，退出码 3".into()
        }]
    );
    assert_eq!(f.card_conversation(uaid(6)), None);
}

#[test]
fn live_rules_follow_rule_events() {
    let (f, _) = fold(&[
        RuleRemembered { rule: rule(1) }.into(),
        RuleRemembered { rule: rule(2) }.into(),
        RuleRemembered { rule: rule(3) }.into(),
        RuleRevoked { rule_id: rid(1) }.into(),
        RulesDeleted {
            rule_ids: vec![rid(3)],
            cause: iwork_domain::event::RulesDeletedCause::ConversationDeleted,
        }
        .into(),
    ]);
    let ids: Vec<_> = f.live_rules().iter().map(|r| r.id).collect();
    assert_eq!(ids, vec![rid(2)]);
    assert_eq!(f.cursor(), Seq(5));
}

// ---------- 一次性放行的消费：审计事件带的 grant_approval_id ----------

/// W1 留下放行 A（审批 1）→ 续接、回到等审批。返回折叠与此刻的游标。
fn grant_left() -> (Folded, Seq) {
    let (mut f, _) = fold(&with(&[
        e_requested(approval(1, 20, 10, "a")),
        e_allowed(1),
        e_interrupted(20),
    ]));
    assert!(f.grant(apid(1)).is_some());
    more(&mut f, &[e_resumed(20), e_waiting(20)]);
    let c = f.cursor();
    (f, c)
}

#[test]
fn grant_audit_allow_moves_to_awaiting() {
    let (mut f, c) = grant_left();
    more(&mut f, &[e_grant_audit(20, 1, Verdict::Allow, c)]);
    assert!(f.grant(apid(1)).is_none(), "同一回合紧跟着的第二次找不到它");
    assert!(f.awaiting_delivery(apid(1)).is_some());
    more(&mut f, &[e_delivered(1, 20), e_interrupted(20)]);
    assert!(f.grant(apid(1)).is_none(), "交付过的不回来");
    assert!(
        f.turns_to_resume()
            .into_iter()
            .map(|(t, _)| t)
            .collect::<Vec<_>>()
            .is_empty()
    );
}

#[test]
fn grant_audit_not_allow_voids_grant() {
    let (mut f, c) = grant_left();
    more(
        &mut f,
        &[e_grant_audit(
            20,
            1,
            Verdict::Deny(DenyReason::DenialMemory(exec("a").fingerprint())),
            c,
        )],
    );
    assert!(f.grant(apid(1)).is_none());
    assert!(f.awaiting_delivery(apid(1)).is_none());
}

#[test]
fn stale_grant_audit_after_interrupt_keeps_grant() {
    // 求值时游标 c；审计落盘之前回合中断（seq b > c）
    let (mut f, c) = grant_left();
    more(&mut f, &[e_interrupted(20)]);
    more(&mut f, &[e_grant_audit(20, 1, Verdict::Allow, c)]);
    assert!(f.grant(apid(1)).is_some(), "过期的消费不算数");
    assert!(f.awaiting_delivery(apid(1)).is_none());
    assert_eq!(
        f.turns_to_resume()
            .into_iter()
            .map(|(t, _)| t)
            .collect::<Vec<_>>(),
        vec![tid(20)]
    );
    // 对照：审计先落盘、中断在后 → 先挪进等交付、中断再挪回来
    let (mut f, c) = grant_left();
    more(&mut f, &[e_grant_audit(20, 1, Verdict::Allow, c)]);
    assert!(f.awaiting_delivery(apid(1)).is_some());
    more(&mut f, &[e_interrupted(20)]);
    assert!(f.grant(apid(1)).is_some());
    assert!(f.awaiting_delivery(apid(1)).is_none());
}

#[test]
fn grant_audit_for_unknown_grant_does_nothing() {
    let (mut f, c) = grant_left();
    let before = f.clone();
    more(&mut f, &[e_grant_audit(20, 9, Verdict::Allow, c)]);
    assert_eq!(f.grant(apid(1)), before.grant(apid(1)));
    assert_eq!(f.cursor(), Seq(before.cursor().0 + 1));
}

// ---------- T06 的 R1–R2（事件直接提交，扮演编排） ----------

#[test]
fn r1_pending_at_interrupt_then_allow_grants() {
    let (f, _) = fold(&with(&[
        e_requested(approval(1, 20, 10, "a")),
        e_interrupted(20),
        e_allowed(1),
    ]));
    assert_eq!(f.grant(apid(1)).unwrap().turn, tid(20));
    assert_eq!(
        f.turns_to_resume()
            .into_iter()
            .map(|(t, _)| t)
            .collect::<Vec<_>>(),
        vec![tid(20)]
    );
}

#[test]
fn r1_approved_undelivered_then_interrupted_grants() {
    let (f, effects) = fold(&with(&[
        e_requested(approval(1, 20, 10, "a")),
        e_allowed(1),
        e_interrupted(20),
    ]));
    let n = effects.len();
    assert_eq!(
        effects[n - 2],
        vec![Effect::Answer {
            approval: apid(1),
            decision: PermissionDecision::Allow
        }],
        "等待方在允许那一刻收到过 Allow"
    );
    assert_eq!(f.grant(apid(1)).unwrap().record, record("a"));
    assert_eq!(
        f.turns_to_resume()
            .into_iter()
            .map(|(t, _)| t)
            .collect::<Vec<_>>(),
        vec![tid(20)]
    );
}

#[test]
fn r1_delivered_no_grant() {
    let (f, _) = fold(&with(&[
        e_requested(approval(1, 20, 10, "a")),
        e_allowed(1),
        e_delivered(1, 20),
        e_running(20),
    ]));
    assert!(f.grant(apid(1)).is_none());
    assert!(f.awaiting_delivery(apid(1)).is_none());
    let (f, _) = fold(&with(&[
        e_requested(approval(1, 20, 10, "a")),
        e_allowed(1),
        e_delivered(1, 20),
        e_running(20),
        e_waiting(20),
        e_interrupted(20),
    ]));
    assert!(f.grant(apid(1)).is_none(), "之后回合中断也没有");
    assert!(
        f.turns_to_resume()
            .into_iter()
            .map(|(t, _)| t)
            .collect::<Vec<_>>()
            .is_empty()
    );
}

#[test]
fn r1_delivered_then_interrupted_no_grant() {
    let (f, _) = fold(&with(&[
        e_requested(approval(1, 20, 10, "a")),
        e_requested(approval(2, 20, 10, "b")),
        e_allowed(1),
        e_delivered(1, 20),
        e_interrupted(20),
    ]));
    assert!(f.grant(apid(1)).is_none());
    let b = f.pending_approval(apid(2)).unwrap();
    assert!(b.detached);
    assert_eq!(b.approval.state, ApprovalState::Pending);
}

#[test]
fn r2_terminal_drops_grants() {
    let (f, _) = fold(&with(&[
        e_requested(approval(1, 20, 10, "a")),
        e_allowed(1),
        e_interrupted(20),
        e_turn_state(
            20,
            TurnState::Interrupted {
                reason: iwork_domain::turn::InterruptReason::ApprovalWaitLimit,
            },
            TurnState::Cancelled,
        ),
        e_turn_finished(20, TurnState::Cancelled),
    ]));
    assert!(f.grant(apid(1)).is_none());
    assert!(
        f.turns_to_resume()
            .into_iter()
            .map(|(t, _)| t)
            .collect::<Vec<_>>()
            .is_empty()
    );
}

// ---------- 归属事实与孤儿 ----------

#[test]
fn orphans_follow_owner_facts() {
    let (mut f, _) = fold(&with(&[
        e_requested(approval(1, 20, 10, "a")),
        e_card(card(5, 20, 10)),
        e_turn_finished(20, TurnState::Completed),
    ]));
    assert!(f.orphans().is_empty(), "回合结束、助手还在：归属成立");
    more(&mut f, &[e_assistant_deleted(1)]);
    assert_eq!(
        f.owner_facts(tid(20)),
        OwnerFacts {
            turn: Presence::Present,
            conversation: ConversationPresence::Present,
            assistant: Presence::Gone
        }
    );
    let gone = owner_alive(&f.owner_facts(tid(20))).unwrap_err();
    assert_eq!(void_reason_for(gone), VoidReason::AssistantDeleted);
    assert_eq!(
        f.orphans().get(&cid(10)).cloned(),
        Some(vec![
            VoidTarget::Approval {
                id: apid(1),
                reason: VoidReason::AssistantDeleted
            },
            VoidTarget::Card {
                id: uaid(5),
                reason: VoidReason::AssistantDeleted
            },
        ])
    );

    let (f, _) = fold(&with(&[
        e_requested(approval(1, 20, 10, "a")),
        e_deleting(10),
    ]));
    assert_eq!(
        f.orphans().get(&cid(10)).cloned(),
        Some(vec![VoidTarget::Approval {
            id: apid(1),
            reason: VoidReason::ConversationDeleted
        }])
    );
}

// ---------- 出错：点名 seq，什么都不改 ----------

#[test]
fn fold_error_names_seq_and_changes_nothing() {
    let (mut f, _) = fold(&base());
    let before = f.clone();
    let err = f.apply(Seq(9), &e_allowed(7)).unwrap_err();
    assert_eq!(
        err,
        FoldError {
            seq: Seq(9),
            kind: "approval_decided",
            why: format!("审批 {} 不在待审批里", apid(7)),
        }
    );
    assert_eq!(f, before, "出错不改任何东西");
    let err = f.apply(Seq(5), &e_user_message(1, 10)).unwrap_err();
    assert_eq!(err.seq, Seq(5), "序号不大于游标");
    assert_eq!(f, before);
}

#[test]
fn replay_skips_streams_the_engine_does_not_read() {
    let events = with(&[
        iwork_domain::event::RecoveryCompleted {
            interrupted_turn_ids: vec![],
        }
        .into(),
        e_requested(approval(1, 20, 10, "a")),
    ]);
    let stored: Vec<StoredEvent> = events
        .iter()
        .enumerate()
        .map(|(i, e)| StoredEvent {
            seq: Seq(i as u64 + 1),
            at: iwork_domain::common::Timestamp(0),
            actor: iwork_domain::common::Actor("t".into()),
            event: e.clone(),
        })
        .collect();
    let f = Folded::replay(stored).unwrap();
    // 对照：逐条 apply 引擎流里的事件得到同一个结果
    let mut g = Folded::default();
    for (i, e) in events.iter().enumerate() {
        if iwork_core::approval::engine::fold::in_engine_streams(e) {
            g.apply(Seq(i as u64 + 1), e).unwrap();
        }
    }
    assert_eq!(f, g);
    assert_eq!(f.cursor(), Seq(7), "游标停在引擎流的最后一条");
}

// ---------- 性质：存储肯收的序列，折叠都认；两边的归属事实相等 ----------

mod props {
    use super::reference_projection::ReferenceProjection;
    use super::*;
    use proptest::prelude::*;

    /// 生成器的一步。具体事件由驱动按参照投影当时的状态补全（审批的对话取自它的回合）。
    #[derive(Debug, Clone)]
    enum Op {
        Assistant(u8),
        AssistantDeleted(u8),
        ProjectConversation(u8),
        Dm(u8, u8),
        Deleting(u8),
        Deleted(u8),
        Turn(u8, u8, u8),
        TurnTo(u8, u8),
        TurnFinished(u8),
        Request(u8, u8),
        Review(u8, u8),
        Decide(u8, u8),
        Void(u8),
        Deliver(u8),
        GrantAudit(u8, bool, u8),
        Card(u8, u8),
        CardTo(u8, u8),
        UserMessage(u8),
        Rule(u8, bool),
    }

    fn op() -> impl Strategy<Value = Op> {
        let s = || 0u8..4;
        prop_oneof![
            1 => s().prop_map(Op::Assistant),
            1 => s().prop_map(Op::AssistantDeleted),
            1 => s().prop_map(Op::ProjectConversation),
            1 => (s(), s()).prop_map(|(c, a)| Op::Dm(c, a)),
            1 => s().prop_map(Op::Deleting),
            1 => s().prop_map(Op::Deleted),
            1 => (s(), s(), s()).prop_map(|(t, c, a)| Op::Turn(t, c, a)),
            6 => (s(), 0u8..6).prop_map(|(t, to)| Op::TurnTo(t, to)),
            1 => s().prop_map(Op::TurnFinished),
            6 => (0u8..6, s()).prop_map(|(a, t)| Op::Request(a, t)),
            3 => (0u8..6, 0u8..3).prop_map(|(a, r)| Op::Review(a, r)),
            5 => (0u8..6, 0u8..3).prop_map(|(a, d)| Op::Decide(a, d)),
            2 => (0u8..6).prop_map(Op::Void),
            4 => (0u8..6).prop_map(Op::Deliver),
            4 => (0u8..6, any::<bool>(), 0u8..8).prop_map(|(a, ok, c)| Op::GrantAudit(a, ok, c)),
            3 => (0u8..4, s()).prop_map(|(c, t)| Op::Card(c, t)),
            3 => (0u8..4, 0u8..5).prop_map(|(c, s)| Op::CardTo(c, s)),
            2 => s().prop_map(Op::UserMessage),
            2 => (s(), any::<bool>()).prop_map(|(r, add)| Op::Rule(r, add)),
        ]
    }

    /// 每例先铺的开场：两个助手、两个项目对话、一个私聊，各有回合；生成器在这之上随机操作。
    fn prelude() -> Vec<Op> {
        vec![
            Op::Assistant(0),
            Op::Assistant(1),
            Op::ProjectConversation(0),
            Op::ProjectConversation(1),
            Op::Dm(2, 1),
            Op::Turn(0, 0, 0),
            Op::Turn(1, 0, 1),
            Op::Turn(2, 1, 0),
            Op::Turn(3, 2, 1),
        ]
    }

    const TURN_STATES: [TurnState; 6] = [
        TurnState::Running,
        TurnState::WaitingApproval,
        TurnState::Interrupted {
            reason: iwork_domain::turn::InterruptReason::ApprovalWaitLimit,
        },
        TurnState::Cancelled,
        TurnState::Completed,
        TurnState::Failed,
    ];

    fn card_state(n: u8) -> UserActionState {
        match n {
            0 => UserActionState::Starting,
            1 => UserActionState::Executing,
            2 => UserActionState::Unknown { error: "u".into() },
            3 => UserActionState::Ended,
            _ => UserActionState::Dismissed,
        }
    }

    /// 驱动：把一步变成一条事件。`used` 挡住重复建同一个实体（存储有主键，不会收）。
    fn event_for(
        op: &Op,
        r: &ReferenceProjection,
        used: &mut std::collections::BTreeSet<String>,
    ) -> Option<DomainEvent> {
        let n = |x: u8| u128::from(x) + 1;
        let mut once = |key: String| used.insert(key);
        Some(match *op {
            Op::Assistant(a) => {
                if !once(format!("a{a}")) {
                    return None;
                }
                e_assistant(n(a))
            }
            Op::AssistantDeleted(a) => {
                if !r.assistants.contains(&aid(n(a))) {
                    return None;
                }
                e_assistant_deleted(n(a))
            }
            Op::ProjectConversation(c) => {
                if !once(format!("c{c}")) {
                    return None;
                }
                e_conversation(project_conversation(n(c), 1))
            }
            Op::Dm(c, a) => {
                if !once(format!("c{c}")) || !r.assistants.contains(&aid(n(a))) {
                    return None;
                }
                e_conversation(dm(n(c), n(a), None))
            }
            Op::Deleting(c) => {
                if !r.conversations.contains_key(&cid(n(c))) {
                    return None;
                }
                e_deleting(n(c))
            }
            Op::Deleted(c) => {
                if !r.conversations.contains_key(&cid(n(c))) {
                    return None;
                }
                e_deleted(n(c))
            }
            Op::Turn(t, c, a) => {
                if !once(format!("t{t}")) || !r.conversations.contains_key(&cid(n(c))) {
                    return None;
                }
                e_turn(n(t), n(c), n(a))
            }
            Op::TurnTo(t, to) => {
                if !r.turns.contains_key(&tid(n(t))) {
                    return None;
                }
                e_turn_state(n(t), TurnState::Running, TURN_STATES[usize::from(to)])
            }
            Op::TurnFinished(t) => {
                if !r.turns.contains_key(&tid(n(t))) {
                    return None;
                }
                e_turn_finished(n(t), TurnState::Completed)
            }
            Op::Request(a, t) => {
                let (conv, _) = *r.turns.get(&tid(n(t)))?;
                if !once(format!("p{a}")) {
                    return None;
                }
                let mut ap = approval(n(a), n(t), 0, &format!("x{}", a % 3));
                ap.conversation_id = conv;
                e_requested(ap)
            }
            Op::Review(a, k) => {
                let rid = ReviewId(tid(50));
                e_review(
                    n(a),
                    match k {
                        0 => ReviewState::InProgress { review_id: rid },
                        1 => ReviewState::Passed {
                            review_id: rid,
                            reason: "ok".into(),
                        },
                        _ => ReviewState::Errored { review_id: rid },
                    },
                )
            }
            Op::Decide(a, d) => match d {
                0 => e_denied(n(a)),
                1 => e_allowed(n(a)),
                _ => e_decided(
                    n(a),
                    ApprovalState::Approved {
                        decider: Decider::AiReview,
                    },
                ),
            },
            Op::Void(a) => e_voided(n(a), VoidReason::TurnCancelled),
            Op::Deliver(a) => {
                let (_, _, turn, _) = r.approvals.get(&apid(n(a)))?;
                ApprovalDeliveredFor(*turn, n(a)).into()
            }
            Op::GrantAudit(a, ok, c) => {
                let (_, _, turn, _) = r.approvals.get(&apid(n(a)))?;
                let verdict = if ok {
                    Verdict::Allow
                } else {
                    Verdict::Deny(DenyReason::ToolDisabled)
                };
                let mut e = e_grant_audit(0, n(a), verdict, Seq(u64::from(c) * 3));
                if let DomainEvent::ApprovalEvaluated(ev) = &mut e {
                    ev.turn_id = *turn;
                }
                e
            }
            Op::Card(c, t) => {
                let (conv, _) = *r.turns.get(&tid(n(t)))?;
                if !once(format!("k{c}")) {
                    return None;
                }
                let mut k = card(n(c), n(t), 0);
                k.conversation_id = conv;
                e_card(k)
            }
            Op::CardTo(c, s) => e_card_state(n(c), card_state(s)),
            Op::UserMessage(c) => e_user_message(u128::from(c) + 100, n(c)),
            Op::Rule(x, add) => {
                if add {
                    RuleRemembered { rule: rule(n(x)) }.into()
                } else {
                    RuleRevoked { rule_id: rid(n(x)) }.into()
                }
            }
        })
    }

    /// 交付事件的回合取自审批（驱动用）。
    struct ApprovalDeliveredFor(iwork_domain::id::TurnId, u128);
    impl From<ApprovalDeliveredFor> for DomainEvent {
        fn from(d: ApprovalDeliveredFor) -> Self {
            iwork_domain::event::ApprovalDelivered {
                // 引擎按自己折叠的表认对话，不读事件上的所属对话（G5 的路由不在这组用例里）
                conversation_id: None,
                approval_id: apid(d.1),
                turn_id: d.0,
            }
            .into()
        }
    }

    proptest! {
        /// 方案 6.10.13：凡是通过提交时校验落了盘的前缀，折叠都不报错；
        /// 每个前缀上，投影里每条待审批与卡片的回合，两边的归属事实相等（owner_facts_agree_between_fold_and_store）。
        #[test]
        fn fold_accepts_whatever_store_accepts(ops in prop::collection::vec(op(), 1..80)) {
            let mut store = ReferenceProjection::default();
            store.apply(&e_project(1, ProjectDefaultMode::Manual)).unwrap();
            let mut folded = Folded::default();
            folded.apply(Seq(1), &e_project(1, ProjectDefaultMode::Manual)).unwrap();
            let mut used = std::collections::BTreeSet::new();
            let mut accepted = 1u64;
            for op in prelude().iter().chain(&ops) {
                let Some(event) = event_for(op, &store, &mut used) else { continue };
                if store.apply(&event).is_err() {
                    continue; // 存储拒收：整笔回滚，不进日志
                }
                accepted += 1;
                folded.apply(Seq(accepted), &event).map_err(|e| TestCaseError::fail(e.to_string()))?;
                for (_, _, turn, _) in store.approvals.values() {
                    prop_assert_eq!(store.owner_facts(*turn), folded.owner_facts(*turn));
                }
                for (_, turn, _) in store.cards.values() {
                    prop_assert_eq!(store.owner_facts(*turn), folded.owner_facts(*turn));
                }
                // 折叠里的待审批 = 参照投影里 Pending 的审批
                let pending_store: Vec<_> = store
                    .approvals
                    .iter()
                    .filter(|(_, (s, _, _, _))| *s == ApprovalState::Pending)
                    .map(|(id, _)| *id)
                    .collect();
                let pending_fold: Vec<_> = folded
                    .conversations()
                    .flat_map(|(_, c)| c.pending.keys().copied())
                    .collect::<std::collections::BTreeSet<_>>()
                    .into_iter()
                    .collect();
                prop_assert_eq!(pending_store, pending_fold);
            }
        }
    }
}
