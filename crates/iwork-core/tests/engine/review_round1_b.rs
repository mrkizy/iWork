use super::*;

#[tokio::test]
async fn failed_mode_commit_keeps_previous_mode() {
    let b = Bench::new(Some(Manual)).await;
    b.store.fail_next.store(true, Ordering::SeqCst);
    assert!(
        b.engine
            .switch_mode(
                ApprovalModeTarget::Conversation(conversation()),
                Some(FullAccess),
                user()
            )
            .await
            .is_err()
    );
    assert_eq!(
        b.engine.effective_mode(conversation(), assistant()).await,
        Manual
    );
}

#[tokio::test]
async fn failed_denial_can_be_retried_and_is_recorded() {
    let b = Bench::new(Some(Manual)).await;
    let h = b.ask(&b.decider(), turn(100), write_req("src/a.rs"));
    let id = b.approval(0).await;
    b.store.fail_next.store(true, Ordering::SeqCst);
    assert!(
        b.engine
            .decide(id, UserDecision::Deny, user())
            .await
            .is_err()
    );
    assert_eq!(
        b.engine.pending_approval(id).await.unwrap().state,
        ApprovalState::Pending
    );
    assert!(!h.is_finished());
    b.engine
        .decide(id, UserDecision::Deny, user())
        .await
        .unwrap();
    assert_eq!(
        h.await.unwrap(),
        PermissionDecision::Deny {
            message: MSG_USER_DENIED.into()
        }
    );
    assert_eq!(b.count(|e| matches!(e, DomainEvent::ApprovalDecided(_))), 1);
}

#[tokio::test]
async fn legacy_approval_resumes_without_grant_once() {
    let b = Bench::new(Some(Manual)).await;
    let h = b.ask(&b.decider(), turn(101), write_req("src/a.rs"));
    let id = b.approval(0).await;
    h.abort();
    let mut events = b.store.stored();
    for e in &mut events {
        if let DomainEvent::ApprovalRequested(e) = &mut e.event {
            e.approval.call = None;
        }
    }
    let recovered = Bench::new(Some(Manual)).await;
    recovered.engine.recover(&events, |_| Some(keys())).await;
    let result = recovered
        .engine
        .decide(
            id,
            UserDecision::Allow {
                remember: RememberScope::Once,
            },
            user(),
        )
        .await
        .unwrap();
    assert_eq!(result.resume_turn, Some(turn(101)));
    assert!(recovered.engine.grant(conversation(), id).await.is_none());
}

#[tokio::test]
async fn remembered_rule_receipt_race_keeps_deny_rules() {
    use iwork_domain::action::ActionKind;
    use iwork_domain::event::RuleRemembered;
    use iwork_domain::rule::{RuleLayer, RuleMatcher, RuleScope};
    let b = Bench::new(Some(Manual)).await;
    let rule = live_rule(
        0x333,
        RuleLayer::UserDeny,
        kind_and(
            ActionKind::Write,
            RuleMatcher::PathIs(format!("{WS}/src/blocked.rs").into()),
        ),
        RuleScope::Global,
    );
    b.engine
        .on_event(Seq(1), &RuleRemembered { rule }.into())
        .await;
    let h = b.ask(&b.decider(), turn(102), bash_req("npm test"));
    let id = b.approval(0).await;
    let gate = Arc::new(tokio::sync::Notify::new());
    *b.store.receipt_gate.lock().unwrap() = Some(gate.clone());
    let engine = b.engine.clone();
    let decision = tokio::spawn(async move {
        engine
            .decide(id, allow(RememberScope::Conversation), user())
            .await
    });
    let stored = loop {
        if let Some(e) = b
            .store
            .stored()
            .into_iter()
            .find(|e| matches!(e.event, DomainEvent::RuleRemembered(_)))
        {
            break e;
        }
        tokio::task::yield_now().await;
    };
    b.engine.on_event(stored.seq, &stored.event).await;
    gate.notify_one();
    decision.await.unwrap().unwrap();
    assert_eq!(h.await.unwrap(), PermissionDecision::Allow);
    b.engine
        .switch_mode(
            ApprovalModeTarget::Conversation(conversation()),
            Some(Auto),
            user(),
        )
        .await
        .unwrap();
    let result = b
        .call(&b.decider(), turn(103), write_req("src/blocked.rs"))
        .await;
    assert!(
        matches!(result, PermissionDecision::Deny { .. }),
        "{result:?}"
    );
}

#[tokio::test]
async fn void_plan_does_not_change_waiting_state_before_commit() {
    let b = Bench::new(Some(Manual)).await;
    let d = b.decider();
    let h = b.ask(&d, turn(104), write_req("src/a.rs"));
    let id = b.approval(0).await;
    let plan = d.void_pending(turn(104), VoidReason::TurnCancelled).await;
    b.store.fail_next.store(true, Ordering::SeqCst);
    use iwork_domain::id::CommandId;
    use iwork_ports::store::{CommitRequest, StoragePort};
    assert!(
        b.store
            .commit(CommitRequest {
                command_id: CommandId::from_raw(RawUlid::from_u128(999)),
                actor: user(),
                events: plan.clone(),
                outbox: vec![]
            })
            .await
            .is_err()
    );
    assert_eq!(
        b.engine.pending_approval(id).await.unwrap().state,
        ApprovalState::Pending
    );
    assert!(!h.is_finished());
    let retry = d.void_pending(turn(104), VoidReason::TurnCancelled).await;
    assert_eq!(retry, plan);
    d.committed(&retry).await;
    assert!(b.engine.pending_approval(id).await.is_none());
    assert!(is_deny(&h.await.unwrap(), MSG_TURN_CANCELLED));
}
