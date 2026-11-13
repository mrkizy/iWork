//! 提交时校验的判断函数（T02 方案 2.6「状态前提」、T07 方案 6.10.7）。期望值手写，不从转换表推。

use iwork_domain::approval::{
    ApprovalState, ConversationPresence, Decider, OwnerFacts, OwnerGone, Presence, ReviewState,
    VoidReason, check_committed, owner_alive, requires_owner, void_reason_for,
};
use iwork_domain::event::{
    ApprovalDecided, ApprovalReviewUpdated, ApprovalVoided, DomainEvent, UserActionStateChanged,
};
use iwork_domain::id::{ApprovalId, ConversationId, RawUlid, ReviewId, TurnId, UserActionId};
use iwork_domain::user_action::{self, ExitStatus, UserActionState};

fn aid() -> ApprovalId {
    ApprovalId::from_raw(RawUlid::from_u128(1))
}
fn rid(n: u128) -> ReviewId {
    ReviewId(TurnId::from_raw(RawUlid::from_u128(n)))
}

/// 这组用例只看前提校验，事件归哪个对话不影响判断。
fn conv() -> ConversationId {
    ConversationId::from_raw(RawUlid::from_u128(9))
}

fn decided(state: ApprovalState) -> DomainEvent {
    ApprovalDecided {
        conversation_id: Some(conv()),
        approval_id: aid(),
        state,
        remember: None,
    }
    .into()
}

fn review(r: ReviewState) -> DomainEvent {
    ApprovalReviewUpdated {
        conversation_id: Some(conv()),
        approval_id: aid(),
        review: r,
    }
    .into()
}

#[test]
fn approval_main_state_must_be_pending() {
    let approved = ApprovalState::Approved {
        decider: Decider::User,
    };
    let voided = ApprovalState::Voided {
        reason: VoidReason::TurnCancelled,
    };
    let void_event: DomainEvent = ApprovalVoided {
        conversation_id: Some(conv()),
        approval_id: aid(),
        reason: VoidReason::TurnCancelled,
    }
    .into();
    let nr = ReviewState::NotReviewed;
    // Pending 能接批准、拒绝、作废
    for e in [
        decided(approved),
        decided(ApprovalState::Denied),
        void_event.clone(),
    ] {
        assert!(
            check_committed(ApprovalState::Pending, &nr, &e).is_ok(),
            "{e:?}"
        );
    }
    // 终态之后什么都接不了
    for from in [approved, ApprovalState::Denied, voided] {
        for e in [
            decided(approved),
            decided(ApprovalState::Denied),
            void_event.clone(),
        ] {
            assert!(check_committed(from, &nr, &e).is_err(), "{from:?} ← {e:?}");
        }
    }
    // ApprovalDecided 不能用来记 Pending / Voided
    assert!(
        check_committed(
            ApprovalState::Pending,
            &nr,
            &decided(ApprovalState::Pending)
        )
        .is_err()
    );
    assert!(check_committed(ApprovalState::Pending, &nr, &decided(voided)).is_err());
}

#[test]
fn review_edges_and_review_id() {
    let p = ApprovalState::Pending;
    let running = ReviewState::InProgress { review_id: rid(7) };
    // 开始：NotReviewed → InProgress
    assert!(check_committed(p, &ReviewState::NotReviewed, &review(running.clone())).is_ok());
    // 同一次审查出结论
    let passed = ReviewState::Passed {
        review_id: rid(7),
        reason: "ok".into(),
    };
    assert!(check_committed(p, &running, &review(passed.clone())).is_ok());
    // review_id 对不上
    let other = ReviewState::Failed {
        review_id: rid(8),
        reason: "x".into(),
    };
    assert!(check_committed(p, &running, &review(other)).is_err());
    // 表外的边：已出结论再出结论、没开始就出结论、再开始一次
    assert!(
        check_committed(
            p,
            &passed,
            &review(ReviewState::Errored { review_id: rid(7) })
        )
        .is_err()
    );
    assert!(
        check_committed(
            p,
            &ReviewState::NotReviewed,
            &review(ReviewState::TimedOut { review_id: rid(7) })
        )
        .is_err()
    );
    assert!(check_committed(p, &running, &review(running.clone())).is_err());
    // 主状态不是 Pending 时审查更新也不行
    assert!(
        check_committed(
            ApprovalState::Denied,
            &ReviewState::NotReviewed,
            &review(running)
        )
        .is_err()
    );
}

fn card_event(state: UserActionState) -> DomainEvent {
    UserActionStateChanged {
        conversation_id: Some(conv()),
        card_id: UserActionId::from_raw(RawUlid::from_u128(2)),
        state,
        output_artifact: None,
    }
    .into()
}

#[test]
fn card_edges_hand_written() {
    use UserActionState as S;
    let all = || {
        vec![
            S::Pending,
            S::Starting,
            S::Executing,
            S::Stopping,
            S::Done {
                exit: ExitStatus::Code(0),
            },
            S::Failed { error: "e".into() },
            S::Unknown { error: "u".into() },
            S::Ended,
            S::Dismissed,
            S::Voided {
                reason: VoidReason::TurnCancelled,
            },
        ]
    };
    // 手写的允许对（现在 → 新值）
    let allowed: &[(&str, &str)] = &[
        ("Pending", "Starting"),
        ("Pending", "Dismissed"),
        ("Pending", "Voided"),
        ("Starting", "Executing"),
        ("Starting", "Failed"),
        ("Starting", "Unknown"),
        ("Executing", "Done"),
        ("Executing", "Stopping"),
        ("Executing", "Unknown"),
        ("Stopping", "Done"),
        ("Stopping", "Unknown"),
        ("Unknown", "Ended"),
    ];
    let name = |s: &S| format!("{:?}", s.kind());
    let mut ok = 0;
    for from in all() {
        for to in all() {
            let want = allowed.contains(&(name(&from).as_str(), name(&to).as_str()));
            let got = user_action::check_committed(&from, &card_event(to.clone())).is_ok();
            assert_eq!(got, want, "{} → {}", name(&from), name(&to));
            ok += usize::from(got);
        }
    }
    assert_eq!(ok, allowed.len(), "对照：允许的对恰好是手写的那些");
}

#[test]
fn owner_alive_order_and_void_reason() {
    use ConversationPresence as C;
    use Presence::{Gone, Present};
    let f = |turn, conversation, assistant| OwnerFacts {
        turn,
        conversation,
        assistant,
    };
    assert_eq!(owner_alive(&f(Present, C::Present, Present)), Ok(()));
    // 回合没了：别的不看
    assert_eq!(
        owner_alive(&f(Gone, C::Present, Present)),
        Err(OwnerGone::Turn)
    );
    assert_eq!(owner_alive(&f(Gone, C::Gone, Gone)), Err(OwnerGone::Turn));
    assert_eq!(
        owner_alive(&f(Present, C::Gone, Gone)),
        Err(OwnerGone::Conversation)
    );
    assert_eq!(
        owner_alive(&f(Present, C::Deleting, Gone)),
        Err(OwnerGone::ConversationDeleting)
    );
    assert_eq!(
        owner_alive(&f(Present, C::Present, Gone)),
        Err(OwnerGone::Assistant)
    );
    assert_eq!(
        void_reason_for(OwnerGone::Assistant),
        VoidReason::AssistantDeleted
    );
    for g in [
        OwnerGone::Turn,
        OwnerGone::Conversation,
        OwnerGone::ConversationDeleting,
    ] {
        assert_eq!(void_reason_for(g), VoidReason::ConversationDeleted);
    }
}

#[test]
fn requires_owner_only_for_build_and_approve() {
    assert!(requires_owner(&decided(ApprovalState::Approved {
        decider: Decider::AiReview
    })));
    assert!(!requires_owner(&decided(ApprovalState::Denied)));
    assert!(requires_owner(&card_event(UserActionState::Starting)));
    assert!(!requires_owner(&card_event(UserActionState::Dismissed)));
    assert!(!requires_owner(&card_event(UserActionState::Voided {
        reason: VoidReason::AssistantDeleted
    })));
    // 排新回合要查（T10a A3）；回合别的事件不查
    let turn = iwork_domain::turn::Turn {
        id: iwork_domain::id::TurnId::from_raw(iwork_domain::id::RawUlid::from_u128(1)),
        conversation_id: iwork_domain::id::ConversationId::from_raw(
            iwork_domain::id::RawUlid::from_u128(2),
        ),
        assistant_id: iwork_domain::id::AssistantId::from_raw(
            iwork_domain::id::RawUlid::from_u128(3),
        ),
        assistant_version: 1,
        runtime_id: iwork_domain::id::RuntimeId::from_raw(iwork_domain::id::RawUlid::from_u128(4)),
        model: iwork_domain::common::ModelSelection {
            model_id: iwork_domain::common::ModelId("m".into()),
            effort: None,
        },
        runtime_session_id: None,
        kind: iwork_domain::turn::TurnKind::Assistant,
        state: iwork_domain::turn::TurnState::Queued,
        started_at: iwork_domain::common::Timestamp(0),
        ended_at: None,
    };
    let id = turn.id;
    let conversation = turn.conversation_id;
    assert!(requires_owner(
        &iwork_domain::event::TurnQueued { turn }.into()
    ));
    assert!(!requires_owner(
        &iwork_domain::event::TurnIntentRecorded {
            conversation_id: Some(conversation),
            turn_id: id,
            intent: iwork_domain::turn::TurnIntentKind::Cancel,
        }
        .into()
    ));
}
