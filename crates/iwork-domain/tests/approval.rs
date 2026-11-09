//! 审批状态机与 AI 审查子状态（方案 4.3「审批 ApprovalState × ReviewState」）。

mod support;

use iwork_domain::action::{Action, ActionFlags, ActionKind, ActionTarget, ToolRef};
use iwork_domain::approval::{
    self, APPROVAL_MACHINE, APPROVAL_TRANSITIONS, Applied, Approval, ApprovalApplyError,
    ApprovalContext, ApprovalKind, ApprovalState, ApprovalTrigger, ApprovalTriggerKind, Decider,
    REVIEW_MACHINE, REVIEW_TRANSITIONS, ReviewPhase, ReviewState, ReviewTrigger, StaleReason,
    VoidReason,
};
use iwork_domain::approval_mode::{ApprovalMode, RememberScope};
use iwork_domain::common::Timestamp;
use iwork_domain::id::{ApprovalId, ConversationId, RawUlid, ReviewId, TurnId};
use iwork_domain::machine::{Finite, IllegalTransition};
use iwork_domain::rule::RuleSetVersion;
use proptest::prelude::*;
use support::{check_forward, check_reverse, verify_machine};

use ApprovalState as A;
use ApprovalTrigger as Tr;

fn review_id(n: u128) -> ReviewId {
    ReviewId(TurnId::from_raw(RawUlid::from_u128(100 + n)))
}

fn pending() -> Approval {
    Approval {
        id: ApprovalId::from_raw(RawUlid::from_u128(1)),
        turn_id: TurnId::from_raw(RawUlid::from_u128(2)),
        conversation_id: ConversationId::from_raw(RawUlid::from_u128(3)),
        kind: ApprovalKind::Ask,
        action: Action {
            kind: ActionKind::Execute,
            target: ActionTarget::Command {
                program: "npm".into(),
                subcommand: Some("test".into()),
                argv: vec![],
            },
            tool: ToolRef("Bash".into()),
            raw_summary: "npm test".into(),
            flags: ActionFlags::default(),
        },
        matched_rules: vec![],
        ruleset_version: RuleSetVersion("1".into()),
        state: A::Pending,
        review: ReviewState::NotReviewed,
        offered_remember: vec![RememberScope::Once],
        remember_label: String::new(),
        requested_at: Timestamp(0),
    }
}

const AUTO: ApprovalContext = ApprovalContext {
    mode_now: ApprovalMode::Auto,
};
const MANUAL: ApprovalContext = ApprovalContext {
    mode_now: ApprovalMode::Manual,
};

/// 处于 `InProgress{r1}` 的待审批。
fn reviewing(r: ReviewId) -> Approval {
    let mut a = pending();
    a.apply(Tr::AiReviewStarted { review_id: r }, &AUTO)
        .unwrap();
    assert_eq!(a.review, ReviewState::InProgress { review_id: r });
    a
}

fn stale(trigger: ApprovalTriggerKind, r: ReviewId, why: StaleReason) -> ApprovalApplyError {
    ApprovalApplyError::Stale {
        trigger,
        review_id: r,
        why,
    }
}

#[test]
fn approval_table_forward() {
    assert_eq!(APPROVAL_TRANSITIONS.len(), 14, "对照：表行数");
    assert_eq!(check_forward(approval::next, APPROVAL_TRANSITIONS), vec![]);
    assert_eq!(REVIEW_TRANSITIONS.len(), 6, "对照：表行数");
    assert_eq!(
        check_forward(approval::next_review, REVIEW_TRANSITIONS),
        vec![]
    );
}

#[test]
fn approval_table_reverse() {
    assert_eq!(
        check_reverse(APPROVAL_MACHINE, approval::next, APPROVAL_TRANSITIONS),
        vec![]
    );
    assert_eq!(
        check_reverse(REVIEW_MACHINE, approval::next_review, REVIEW_TRANSITIONS),
        vec![]
    );
    assert_eq!(
        verify_machine(APPROVAL_MACHINE, approval::next, APPROVAL_TRANSITIONS),
        Ok(())
    );
    assert_eq!(
        verify_machine(REVIEW_MACHINE, approval::next_review, REVIEW_TRANSITIONS),
        Ok(())
    );
}

#[test]
fn approval_scenarios() {
    let (r1, r2) = (review_id(1), review_id(2));

    // 1. 超时转交用户后，迟到的「通过」作废；用户随后允许才放行
    let mut a = reviewing(r1);
    a.apply(Tr::AiReviewTimedOut { review_id: r1 }, &AUTO)
        .unwrap();
    assert_eq!(a.review, ReviewState::TimedOut { review_id: r1 });
    assert!(a.awaits_user());
    let before = a.clone();
    assert_eq!(
        a.apply(Tr::AiReviewPassed { review_id: r1 }, &AUTO),
        Err(stale(
            ApprovalTriggerKind::AiReviewPassed,
            r1,
            StaleReason::NotInProgress
        ))
    );
    assert_eq!(a, before);
    a.apply(
        Tr::UserAllow {
            remember: RememberScope::Once,
        },
        &AUTO,
    )
    .unwrap();
    assert_eq!(
        a.state,
        A::Approved {
            decider: Decider::User
        }
    );

    // 2. 切回手动：进行中的审查作废、转问用户；之后的「通过」在手动档下作废
    let mut a = reviewing(r1);
    let applied = a.apply(Tr::ModeChangedToManual, &MANUAL).unwrap();
    assert_eq!(
        applied,
        Applied {
            state_changed: false,
            review_changed: true
        }
    );
    assert_eq!(a.review, ReviewState::Abandoned { review_id: r1 });
    assert!(a.awaits_user());
    let before = a.clone();
    assert!(matches!(
        a.apply(Tr::AiReviewPassed { review_id: r1 }, &MANUAL),
        Err(ApprovalApplyError::Stale { .. })
    ));
    assert_eq!((a.state, &a), (A::Pending, &before));

    // 3. 用户在超时前直接拒了：迟到的「通过」作废，仍是 Denied
    let mut a = reviewing(r1);
    a.apply(Tr::UserDeny, &AUTO).unwrap();
    assert_eq!(
        a.apply(Tr::AiReviewPassed { review_id: r1 }, &AUTO),
        Err(stale(
            ApprovalTriggerKind::AiReviewPassed,
            r1,
            StaleReason::NotInProgress
        ))
    );
    assert_eq!(a.state, A::Denied);

    // 4. 对照：本次、进行中、自动档，三条都满足才放行
    let mut a = reviewing(r1);
    let applied = a
        .apply(Tr::AiReviewPassed { review_id: r1 }, &AUTO)
        .unwrap();
    assert_eq!(
        applied,
        Applied {
            state_changed: true,
            review_changed: true
        }
    );
    assert_eq!(
        a.state,
        A::Approved {
            decider: Decider::AiReview
        }
    );
    assert_eq!(a.review, ReviewState::Passed { review_id: r1 });

    // 5. 错 id：当前是 r2，r1 的结果作废
    let mut a = reviewing(r2);
    let before = a.clone();
    assert_eq!(
        a.apply(Tr::AiReviewPassed { review_id: r1 }, &AUTO),
        Err(stale(
            ApprovalTriggerKind::AiReviewPassed,
            r1,
            StaleReason::ReviewIdMismatch
        ))
    );
    assert_eq!(a, before);
}

#[test]
fn approval_scenarios_more() {
    let r1 = review_id(1);

    // 档位已不是自动（条件③）：进行中、对号的「不通过」也作废
    let mut a = reviewing(r1);
    let ctx = ApprovalContext {
        mode_now: ApprovalMode::FullAccess,
    };
    assert_eq!(
        a.apply(
            Tr::AiReviewFailed {
                review_id: r1,
                reason: "x".into()
            },
            &ctx
        ),
        Err(stale(
            ApprovalTriggerKind::AiReviewFailed,
            r1,
            StaleReason::ModeNotAuto
        ))
    );

    // 审查进行中切到全开：模式切换放行，子状态保持进行中，之后的结果都作废
    let mut a = reviewing(r1);
    a.apply(Tr::ModeSwitchAllows, &ctx).unwrap();
    assert_eq!(
        a.state,
        A::Approved {
            decider: Decider::ModeSwitch
        }
    );
    assert_eq!(a.review, ReviewState::InProgress { review_id: r1 });
    assert!(!a.awaits_user());
    assert!(matches!(
        a.apply(Tr::AiReviewTimedOut { review_id: r1 }, &AUTO),
        Err(ApprovalApplyError::Stale {
            why: StaleReason::NotInProgress,
            ..
        })
    ));

    // 不通过带理由，转问用户
    let mut a = reviewing(r1);
    assert!(!a.awaits_user(), "审查中用户还没被问到，不计入待你处理");
    a.apply(
        Tr::AiReviewFailed {
            review_id: r1,
            reason: "越界写".into(),
        },
        &AUTO,
    )
    .unwrap();
    assert_eq!(
        a.review,
        ReviewState::Failed {
            review_id: r1,
            reason: "越界写".into()
        }
    );
    assert!(a.awaits_user());

    // 作废：回合取消
    let mut a = pending();
    a.apply(
        Tr::Void {
            reason: VoidReason::TurnCancelled,
        },
        &MANUAL,
    )
    .unwrap();
    assert_eq!(
        a.state,
        A::Voided {
            reason: VoidReason::TurnCancelled
        }
    );

    // 重复开始审查是缺陷，不是乱序
    let mut a = reviewing(r1);
    assert_eq!(
        a.apply(
            Tr::AiReviewStarted {
                review_id: review_id(2)
            },
            &AUTO
        ),
        Err(ApprovalApplyError::IllegalReviewTransition(
            IllegalTransition {
                machine: REVIEW_MACHINE,
                state: ReviewPhase::InProgress,
                trigger: ReviewTrigger::Started,
            }
        ))
    );

    // 已决定的审批不能再决定
    let mut a = pending();
    a.apply(Tr::UserDeny, &MANUAL).unwrap();
    assert_eq!(
        a.apply(
            Tr::UserAllow {
                remember: RememberScope::Forever
            },
            &MANUAL
        ),
        Err(ApprovalApplyError::IllegalTransition(IllegalTransition {
            machine: APPROVAL_MACHINE,
            state: A::Denied,
            trigger: ApprovalTriggerKind::UserAllow,
        }))
    );
}

fn any_review_id() -> impl Strategy<Value = ReviewId> {
    // 只取三个值，让「对号」与「不对号」都常出现
    (0u128..3).prop_map(review_id)
}

fn any_trigger() -> impl Strategy<Value = ApprovalTrigger> {
    prop_oneof![
        prop::sample::select(RememberScope::all()).prop_map(|remember| Tr::UserAllow { remember }),
        Just(Tr::UserDeny),
        Just(Tr::ModeSwitchAllows),
        prop::sample::select(VoidReason::all()).prop_map(|reason| Tr::Void { reason }),
        any_review_id().prop_map(|review_id| Tr::AiReviewStarted { review_id }),
        any_review_id().prop_map(|review_id| Tr::AiReviewPassed { review_id }),
        any_review_id().prop_map(|review_id| Tr::AiReviewFailed {
            review_id,
            reason: "r".into()
        }),
        any_review_id().prop_map(|review_id| Tr::AiReviewErrored { review_id }),
        any_review_id().prop_map(|review_id| Tr::AiReviewTimedOut { review_id }),
        Just(Tr::ModeChangedToManual),
    ]
}

fn any_step() -> impl Strategy<Value = (ApprovalTrigger, ApprovalContext)> {
    (
        any_trigger(),
        prop::sample::select(ApprovalMode::all()).prop_map(|mode_now| ApprovalContext { mode_now }),
    )
}

proptest! {
    /// Denied 永不变 Approved（卡验收 3）。
    #[test]
    fn approval_denied_never_approved(steps in prop::collection::vec(any_step(), 0..40)) {
        let mut a = pending();
        let mut denied = false;
        for (trigger, ctx) in steps {
            let _ = a.apply(trigger, &ctx);
            if denied {
                prop_assert_eq!(a.state, A::Denied);
            }
            denied |= a.state == A::Denied;
        }
    }

    /// 三个终态不再转换：进入终态后主状态不再变化。
    #[test]
    fn approval_terminal_is_final(steps in prop::collection::vec(any_step(), 0..40)) {
        let mut a = pending();
        let mut terminal = None;
        for (trigger, ctx) in steps {
            let result = a.apply(trigger, &ctx);
            if let Some(t) = terminal {
                prop_assert!(result.is_err());
                prop_assert_eq!(a.state, t);
            } else if a.state.is_terminal() {
                terminal = Some(a.state);
            }
        }
    }

    /// 任何失败（Stale 或表外转换）都不改审批的任何字段。
    #[test]
    fn approval_stale_never_changes_state(steps in prop::collection::vec(any_step(), 0..40)) {
        let mut a = pending();
        for (trigger, ctx) in steps {
            let before = a.clone();
            if a.apply(trigger, &ctx).is_err() {
                prop_assert_eq!(&a, &before);
            }
        }
    }
}

#[test]
fn approval_terminal_states_reject_every_trigger() {
    let terminal: Vec<_> = ApprovalState::all()
        .into_iter()
        .filter(|s| s.is_terminal())
        .collect();
    // Approved×3 决定者 + Denied + Voided×5 原因
    assert_eq!(terminal.len(), 9);
    for state in terminal {
        for trigger in ApprovalTriggerKind::all() {
            assert!(
                approval::next(state, trigger).is_err(),
                "{state:?} × {trigger:?}"
            );
        }
    }
}

/// 作废只能由五个原因之一触发，且每个原因只产生同名的作废状态。
#[test]
fn approval_void_reasons_exhaustive() {
    assert_eq!(
        VoidReason::all(),
        vec![
            VoidReason::TurnCancelled,
            VoidReason::ConversationDeleted,
            VoidReason::TurnAbandonedAfterInterrupt,
            VoidReason::AssistantDeleted,
            VoidReason::TurnFailed,
        ]
    );
    let voiding: Vec<_> = APPROVAL_TRANSITIONS
        .iter()
        .filter(|(_, _, to)| matches!(to, A::Voided { .. }))
        .copied()
        .collect();
    let expected: Vec<_> = VoidReason::all()
        .into_iter()
        .map(|reason| {
            (
                A::Pending,
                ApprovalTriggerKind::Void { reason },
                A::Voided { reason },
            )
        })
        .collect();
    assert_eq!(voiding, expected);
}
