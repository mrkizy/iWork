//! 第 2 轮审查 B 侧的确定性交错（2/F-1～2/F-4）：计划、提交、内存应用与 decide 在同一串行边界里；
//! 已提交的状态不因请求取消或审计失败而丢。

use super::*;
use iwork_domain::id::CommandId;
use iwork_ports::store::{CommitRequest, StoragePort};

async fn commit_raw(b: &Bench, n: u128, events: Vec<DomainEvent>) -> Seq {
    b.store
        .commit(CommitRequest {
            command_id: CommandId::from_raw(RawUlid::from_u128(0x9000 + n)),
            actor: user(),
            events,
            outbox: vec![],
        })
        .await
        .unwrap()
        .last_seq
}

fn count(b: &Bench, f: impl Fn(&DomainEvent) -> bool) -> usize {
    b.store.all().iter().filter(|e| f(e)).count()
}

/// 2/F-1：作废计划已交出、编排已写入回合状态与 ApprovalVoided、committed 回调还没来：此时 decide(Allow, Project)
/// 先追平，读到作废，不能批准、不能生成记住规则；同一回合也不能新登记等待项。
/// 对照：计划交出了但没落盘，下一个决定先过屏障（一笔空提交）再读，审批照常可以批准。
#[tokio::test]
async fn round2_void_committed_before_callback_cannot_be_approved() {
    let b = Bench::new(Some(Manual)).await;
    let d = b.decider();
    let h = b.ask(&d, turn(200), write_req("src/a.rs"));
    let id = b.approval(0).await;
    let plan = d
        .void_pending(turn(200), VoidReason::TurnCancelled)
        .await
        .unwrap()
        .0;
    assert!(!plan.is_empty());
    let mut with_turn = vec![state_changed(
        turn(200),
        TurnState::WaitingApproval,
        TurnState::Cancelling,
    )];
    with_turn.extend(plan.iter().cloned());
    commit_raw(&b, 1, with_turn).await;
    assert_eq!(
        b.engine
            .decide(
                id,
                allow(RememberScope::Project),
                user(),
                crate::support::command()
            )
            .await,
        Err(DecideError::NotPending(id))
    );
    assert_eq!(
        count(&b, |e| matches!(e, DomainEvent::RuleRemembered(_))),
        0
    );
    assert_eq!(
        count(&b, |e| matches!(e, DomainEvent::ApprovalDecided(_))),
        0
    );
    // 同一回合在作废中：新请求直接按取消拒绝，不登记等待项
    assert!(is_deny(
        &d.decide(turn(200), &write_req("src/b.rs")).await.decision,
        MSG_TURN_CANCELLED
    ));
    assert_eq!(
        count(&b, |e| matches!(e, DomainEvent::ApprovalRequested(_))),
        1,
        "回合不在等审批：不建审批"
    );
    d.committed(&plan).await;
    assert!(b.pending(id).await.is_none());
    assert!(is_deny(&settle(h).await, MSG_TURN_CANCELLED));

    // 对照：外部提交失败 → 撤销预留，等待项照常可以批准
    let h = b.ask(&d, turn(201), write_req("src/c.rs"));
    let id = b.approval(1).await;
    let plan = d
        .void_pending(turn(201), VoidReason::TurnCancelled)
        .await
        .unwrap()
        .0;
    assert!(!plan.is_empty());
    let commits = b.store.commits.load(Ordering::SeqCst);
    b.engine
        .decide(
            id,
            allow(RememberScope::Once),
            user(),
            crate::support::command(),
        )
        .await
        .unwrap();
    assert_eq!(
        b.store.commits.load(Ordering::SeqCst),
        commits + 2,
        "屏障的空提交 + 决定那一笔"
    );
    assert_eq!(settle(h).await, PermissionDecision::Allow);
}

/// 2/F-1 同类：对话删除走 void_for_event，确认前同样不能批准。
#[tokio::test]
async fn round2_void_for_event_blocks_decide_until_confirmed() {
    let b = Bench::new(Some(Manual)).await;
    let h = b.ask(&b.decider(), turn(202), write_req("src/a.rs"));
    let id = b.approval(0).await;
    let deleting: DomainEvent = ConversationDeleting {
        conversation_id: conversation(),
    }
    .into();
    let plan = b
        .engine
        .commit_with(
            Scope::ConversationDeleting(conversation()),
            command(vec![deleting]),
        )
        .await
        .unwrap()
        .attached;
    assert!(!plan.is_empty());
    assert_eq!(
        b.engine
            .decide(
                id,
                allow(RememberScope::Project),
                user(),
                crate::support::command()
            )
            .await,
        Err(DecideError::NotPending(id))
    );
    assert!(is_deny(&settle(h).await, MSG_TURN_CANCELLED));
    assert_eq!(
        count(&b, |e| matches!(e, DomainEvent::RuleRemembered(_))),
        0
    );
}

/// 2/F-2：拒绝已落盘、等回执时 decide 被取消：订阅重投 ApprovalDecided 后拒绝记忆照样生效，
/// 切自动档也不放行原调用。取消等回执不是回滚。
#[tokio::test]
async fn round2_committed_denial_survives_cancelled_receipt() {
    let b = Bench::new(Some(Manual)).await;
    let d = b.decider();
    let h = b.ask(&d, turn(203), write_req("src/a.rs"));
    let id = b.approval(0).await;
    let gate = Arc::new(tokio::sync::Notify::new());
    *b.store.receipt_gate.lock().unwrap() = Some(gate.clone());
    let engine = b.engine.clone();
    let decision = tokio::spawn(async move {
        engine
            .decide(id, UserDecision::Deny, user(), crate::support::command())
            .await
    });
    let stored = loop {
        if let Some(e) = b
            .store
            .stored()
            .into_iter()
            .find(|e| matches!(e.event, DomainEvent::ApprovalDecided(_)))
        {
            break e;
        }
        tokio::task::yield_now().await;
    };
    decision.abort();
    let _ = decision.await;
    // 下一次追平折到已落盘的拒绝
    assert!(matches!(stored.event, DomainEvent::ApprovalDecided(_)));
    b.engine.sync().await.unwrap();
    assert!(is_deny(&settle(h).await, MSG_USER_DENIED));
    assert!(b.pending(id).await.is_none());
    b.engine
        .switch_mode(
            ApprovalModeTarget::Conversation(conversation()),
            Some(Auto),
            user(),
            crate::support::command(),
        )
        .await
        .unwrap();
    let again = b.call(&d, turn(204), write_req("src/a.rs")).await;
    assert!(is_deny(&again, MSG_USER_DENIED), "{again:?}");
    // 再追平一次也一样（每条事件只应用一次）
    b.engine.sync().await.unwrap();
    assert!(is_deny(
        &b.call(&d, turn(205), write_req("src/a.rs")).await,
        MSG_USER_DENIED
    ));
}

/// 2/F-2 同类：切档已落盘、等回执时被取消，订阅重投档位变化后档位与放行都补上。
#[tokio::test]
async fn round2_committed_mode_switch_survives_cancelled_receipt() {
    let b = Bench::new(Some(Manual)).await;
    let d = b.decider();
    let h = b.ask(&d, turn(206), write_req("src/a.rs"));
    let _ = b.approval(0).await;
    let gate = Arc::new(tokio::sync::Notify::new());
    *b.store.receipt_gate.lock().unwrap() = Some(gate.clone());
    let engine = b.engine.clone();
    let switching = tokio::spawn(async move {
        engine
            .switch_mode(
                ApprovalModeTarget::Conversation(conversation()),
                Some(Auto),
                user(),
                crate::support::command(),
            )
            .await
    });
    let stored = loop {
        let s = b.store.stored();
        if s.iter()
            .any(|e| matches!(e.event, DomainEvent::ApprovalDecided(_)))
        {
            break s;
        }
        tokio::task::yield_now().await;
    };
    switching.abort();
    let _ = switching.await;
    assert!(
        stored
            .iter()
            .any(|e| matches!(e.event, DomainEvent::ApprovalModeChanged(_)))
    );
    b.engine.sync().await.unwrap();
    assert_eq!(b.mode(conversation(), assistant()).await, Auto);
    assert_eq!(settle(h).await, PermissionDecision::Allow);
}

/// 2/F-3：撤销已落盘但订阅还没消费，另一条记住规则的本地回执先回来：缓存先从日志补齐撤销，
/// 被撤销的规则不再放行；审计的 live_rules_seq 按日志重放得到同一结论。
#[tokio::test]
async fn round2_live_cursor_must_describe_contiguous_rule_history() {
    use iwork_core::approval::evaluate::strictness::strictness;
    use iwork_core::approval::evaluate::{EvalContext, evaluate};
    use iwork_core::approval::rules::build_ruleset;
    let b = Bench::new(Some(Manual)).await;
    let d = b.decider();
    // ① 记住「本项目允许写 src/a.rs」，订阅按序投递
    let h = b.ask(&d, turn(210), write_req("src/a.rs"));
    let id = b.approval(0).await;
    b.engine
        .decide(
            id,
            allow(RememberScope::Project),
            user(),
            crate::support::command(),
        )
        .await
        .unwrap();
    settle(h).await;
    b.engine.sync().await.unwrap();
    assert_eq!(
        b.call(&d, turn(211), write_req("src/a.rs")).await,
        PermissionDecision::Allow
    );
    // ② 撤销落盘，订阅还没消费
    let rule_id = b
        .store
        .all()
        .into_iter()
        .find_map(|e| match e {
            DomainEvent::RuleRemembered(r) => Some(r.rule.id),
            _ => None,
        })
        .unwrap();
    commit_raw(
        &b,
        3,
        vec![iwork_domain::event::RuleRevoked { rule_id }.into()],
    )
    .await;
    // ③ 另一条记住规则的本地回执先回来
    let h = b.ask(&d, turn(212), write_req("docs/other.md"));
    let id = b.approval(1).await;
    b.engine
        .decide(
            id,
            allow(RememberScope::Project),
            user(),
            crate::support::command(),
        )
        .await
        .unwrap();
    settle(h).await;
    // ④ 被撤销的规则不再放行
    let h = b.ask(&d, turn(213), write_req("src/a.rs"));
    let id = b.approval(2).await;
    b.engine
        .decide(id, UserDecision::Deny, user(), crate::support::command())
        .await
        .unwrap();
    settle(h).await;
    // 每条审计按自己的游标从日志重建规则集，结论一致
    let stored = b.store.stored();
    let k = keys();
    let mut checked = 0;
    for ev in &stored {
        let DomainEvent::ApprovalEvaluated(a) = &ev.event else {
            continue;
        };
        let mut live: Vec<RuleEntry> = Vec::new();
        for e in stored.iter().filter(|e| e.seq <= a.live_rules_seq) {
            match &e.event {
                DomainEvent::RuleRemembered(r) => live.push(r.rule.clone()),
                DomainEvent::RuleRevoked(r) => live.retain(|x| x.id != r.rule_id),
                _ => {}
            }
        }
        let rules = build_ruleset(&k.snapshot.rules, &live, &k.scope()).unwrap();
        let denial = iwork_core::approval::denial::DenialMemory::default();
        let ctx = EvalContext {
            mode: Manual,
            turn_kind: TurnKind::Assistant,
            tool_set: &k.tool_set,
            disabled_groups: &k.disabled_groups,
            denial: &denial,
        };
        let shape = iwork_core::approval::action::CallShape::Tool;
        let worst = a
            .sub_actions
            .iter()
            .map(|x| evaluate(x, &shape, &rules, &ctx, &fs()).0.verdict)
            .max_by_key(strictness)
            .unwrap();
        assert_eq!(
            strictness(&worst),
            strictness(&a.verdict),
            "审计 {:?} 的游标 {:?}",
            a.verdict,
            a.live_rules_seq
        );
        checked += 1;
    }
    assert!(checked >= 4, "对照：审计条数 {checked}");
    // 最后一次写 src/a.rs 是询问（被拒），不是放行
    let last = stored
        .iter()
        .rev()
        .find_map(|e| match &e.event {
            DomainEvent::ApprovalEvaluated(a) => Some(a.clone()),
            _ => None,
        })
        .unwrap();
    assert!(
        matches!(last.verdict, Verdict::Ask(_)),
        "{:?}",
        last.verdict
    );
}

/// 2/F-4：一次性放行命中后审计提交失败：授权不丢，重试同一调用照样命中；成功后才消费。
#[tokio::test]
async fn round2_failed_audit_must_not_consume_grant_once() {
    let b = Bench::new(None).await;
    let d = b.decider();
    let h = b.ask(&d, turn(220), bash_req("rm a.txt && touch a.txt"));
    let id = b.approval(0).await;
    h.abort();
    let _ = h.await;
    b.interrupt(turn(220)).await;
    b.engine
        .decide(
            id,
            allow(RememberScope::Once),
            user(),
            crate::support::command(),
        )
        .await
        .unwrap();
    assert!(b.grant(id).await.is_some());
    // 续接（编排提交回到等审批），然后让审计那一笔失败
    b.waiting(turn(220), assistant());
    b.store.fail_next.store(true, Ordering::SeqCst);
    let first = d
        .decide(turn(220), &bash_req("rm a.txt && touch a.txt"))
        .await;
    assert!(matches!(first.decision, PermissionDecision::Deny { .. }));
    assert!(b.grant(id).await.is_some(), "审计失败不消费");
    assert_eq!(
        b.call(&d, turn(220), bash_req("rm a.txt && touch a.txt"))
            .await,
        PermissionDecision::Allow
    );
    assert!(b.grant(id).await.is_none());
}
