//! 引擎重做后的确定性交错（T07 方案 6.10.9 B 侧反例逐条、6.10.13 测试清单）。
//!
//! 先后靠假存储的三个门固定：入队门（还没入队）、处理门（入队了、排在它后面的都得等）、回执门（处理完、
//! 回执没到）。存储只按入队顺序处理，调用方被丢弃后已入队的那笔照样落盘（和生产写线程一样）。
//! 每条都带对照，证明比的是先后。

use super::*;
use iwork_core::approval::engine::fold::Folded;
use iwork_core::turn::Decided;
use iwork_domain::event::{
    ApprovalDecided, ApprovalDelivered, ApprovalVoided, ConversationDeleted, RuleRemembered,
    TurnFinished,
};
use iwork_domain::id::UserActionId;

fn notify() -> Arc<tokio::sync::Notify> {
    Arc::new(tokio::sync::Notify::new())
}

/// 等条件成立（最多 2 秒），期间让出执行权。
async fn until(what: &str, cond: impl Fn() -> bool) {
    tokio::time::timeout(Duration::from_secs(2), async {
        while !cond() {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap_or_else(|_| panic!("等不到：{what}"));
}

/// 多让几轮，给别的任务机会跑到它们会停下的地方。
async fn settle_tasks() {
    for _ in 0..50 {
        tokio::task::yield_now().await;
    }
}

fn voided(id: ApprovalId, reason: VoidReason) -> DomainEvent {
    ApprovalVoided {
        approval_id: id,
        reason,
    }
    .into()
}

fn approved_by_user(id: ApprovalId) -> DomainEvent {
    ApprovalDecided {
        approval_id: id,
        state: ApprovalState::Approved {
            decider: Decider::User,
        },
        remember: None,
    }
    .into()
}

fn delivered(id: ApprovalId, t: TurnId) -> DomainEvent {
    ApprovalDelivered {
        approval_id: id,
        turn_id: t,
    }
    .into()
}

fn no_approved(b: &Bench, id: ApprovalId) -> bool {
    b.count(|e| matches!(e, DomainEvent::ApprovalDecided(x) if x.approval_id == id && matches!(x.state, ApprovalState::Approved { .. })))
        == 0
}

fn rules_remembered(b: &Bench) -> usize {
    b.count(|e| matches!(e, DomainEvent::RuleRemembered(_)))
}

/// 引擎内存等于对日志的重放（方向 1）。
async fn fold_matches_log(b: &Bench) {
    b.engine.catch_up().await.unwrap();
    assert_eq!(b.engine.folded(), Folded::replay(b.store.stored()).unwrap());
}

fn spawn_decide(
    b: &Bench,
    id: ApprovalId,
    decision: UserDecision,
) -> tokio::task::JoinHandle<Result<iwork_core::approval::engine::DecideOutcome, DecideError>> {
    let engine = b.engine.clone();
    tokio::spawn(async move { engine.decide(id, decision, user()).await })
}

/// 等第 `n` 张卡片。
async fn card_n(b: &Bench, n: usize) -> UserActionId {
    for _ in 0..400 {
        let cards: Vec<_> = b
            .store
            .all()
            .into_iter()
            .filter_map(|e| match e {
                DomainEvent::UserActionProposed(p) => Some(p.card.id),
                _ => None,
            })
            .collect();
        if let Some(c) = cards.get(n) {
            return *c;
        }
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    panic!("第 {n} 张卡片没有出现");
}

// ---------- 6.10.9 B 侧反例 ----------

/// 1/F-13：提交回 Busy，折叠不动；重试的拒绝只留一条事件。
#[tokio::test]
async fn t_failed_commit_leaves_fold_unchanged() {
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
    assert_eq!(b.mode(conversation(), assistant()).await, Manual);
    let h = b.ask(&b.decider(), turn(1), write_req("src/a.rs"));
    let id = b.approval(0).await;
    let fps = b.pending(id).await.unwrap().fingerprints();
    b.store.fail_next.store(true, Ordering::SeqCst);
    assert!(
        b.engine
            .decide(id, UserDecision::Deny, user())
            .await
            .is_err()
    );
    for fp in &fps {
        assert_eq!(
            b.engine.denial_phase(conversation(), fp),
            None,
            "没落盘不记"
        );
    }
    b.engine
        .decide(id, UserDecision::Deny, user())
        .await
        .unwrap();
    assert_eq!(b.count(|e| matches!(e, DomainEvent::ApprovalDecided(_))), 1);
    for fp in &fps {
        assert_eq!(
            b.engine.denial_phase(conversation(), fp),
            Some(iwork_core::approval::denial::DenialPhase::AutoDeny)
        );
    }
    settle(h).await;
    fold_matches_log(&b).await;
}

/// 2/F-1 (a)：作废已落盘、编排还没丢句柄：decide 在令牌上等，拿到后追平，`NotPending`。
#[tokio::test]
async fn t_void_then_decide_cannot_approve_while_hold_held() {
    let b = Bench::new(Some(Manual)).await;
    let h = b.ask(&b.decider(), turn(1), write_req("src/a.rs"));
    let id = b.approval(0).await;
    let (plan, hold) = b
        .engine
        .void_plan(conversation(), turn(1), VoidReason::TurnCancelled)
        .await
        .unwrap();
    let decide = spawn_decide(&b, id, allow(RememberScope::Project));
    settle_tasks().await;
    assert!(!decide.is_finished(), "句柄占着令牌，decide 在等");
    // 编排那笔落盘了，句柄还没丢
    let mut events = vec![state_changed(
        turn(1),
        TurnState::WaitingApproval,
        TurnState::Cancelling,
    )];
    events.extend(plan);
    b.store.seed(events);
    settle_tasks().await;
    assert!(!decide.is_finished());
    drop(hold);
    assert_eq!(decide.await.unwrap(), Err(DecideError::NotPending(id)));
    assert!(no_approved(&b, id));
    assert_eq!(rules_remembered(&b), 0);
    assert!(is_deny(&settle(h).await, MSG_TURN_CANCELLED));
}

/// 2/F-1 (b)：编排在提交途中被丢弃、作废还在写队列里：decide 先过屏障，等那笔落盘再读。
#[tokio::test]
async fn t_void_then_decide_cannot_approve_when_void_still_queued() {
    let b = Bench::new(Some(Manual)).await;
    let h = b.ask(&b.decider(), turn(1), write_req("src/a.rs"));
    let id = b.approval(0).await;
    let (plan, hold) = b
        .engine
        .void_plan(conversation(), turn(1), VoidReason::TurnCancelled)
        .await
        .unwrap();
    let gate = notify();
    *b.store.before_commit.lock().unwrap() = Some(gate.clone());
    let store = b.store.clone();
    let mut events = vec![state_changed(
        turn(1),
        TurnState::WaitingApproval,
        TurnState::Cancelling,
    )];
    events.extend(plan);
    let orchestrator = tokio::spawn(async move {
        use iwork_ports::store::StoragePort;
        store.commit(command(events)).await
    });
    until("作废入队", || {
        b.store.commit_entered.load(Ordering::SeqCst)
    })
    .await;
    orchestrator.abort();
    drop(hold);
    let empties = b.store.empty_commits.load(Ordering::SeqCst);
    let decide = spawn_decide(&b, id, allow(RememberScope::Project));
    settle_tasks().await;
    assert!(!decide.is_finished(), "屏障排在作废后面");
    gate.notify_one();
    assert_eq!(decide.await.unwrap(), Err(DecideError::NotPending(id)));
    assert_eq!(b.store.empty_commits.load(Ordering::SeqCst), empties + 1);
    assert!(no_approved(&b, id));
    assert_eq!(rules_remembered(&b), 0);
    assert!(is_deny(&settle(h).await, MSG_TURN_CANCELLED));
}

/// 2/F-1 (c)：不拿令牌的写者直接提交作废，decide 那笔排在它后面：整笔被拒（记住规则一起回滚），重算后 `NotPending`。
#[tokio::test]
async fn t_void_then_decide_cannot_approve_without_token() {
    let b = Bench::new(Some(Manual)).await;
    let h = b.ask(&b.decider(), turn(1), write_req("src/a.rs"));
    let id = b.approval(0).await;
    let gate = notify();
    *b.store.enqueue_gate.lock().unwrap() = Some(gate.clone());
    let decide = spawn_decide(&b, id, allow(RememberScope::Project));
    until("decide 停在入队门口", || {
        b.store.at_enqueue_gate.load(Ordering::SeqCst)
    })
    .await;
    b.store
        .seed(vec![voided(id, VoidReason::ConversationDeleted)]);
    let commits = b.store.commits.load(Ordering::SeqCst);
    gate.notify_one();
    assert_eq!(decide.await.unwrap(), Err(DecideError::NotPending(id)));
    assert_eq!(
        b.store.commits.load(Ordering::SeqCst),
        commits + 1,
        "被拒 1 次后收敛"
    );
    assert!(no_approved(&b, id));
    assert_eq!(rules_remembered(&b), 0);
    assert!(is_deny(&settle(h).await, MSG_TURN_CANCELLED));
}

/// 2/F-2：拒绝已落盘、回执没到时丢弃 decide：切自动后同一调用被拒绝记忆拒；折叠的拒绝记忆与重放一致。
#[tokio::test]
async fn t_committed_denial_survives_cancelled_receipt() {
    let b = Bench::new(Some(Manual)).await;
    let d = b.decider();
    let h = b.ask(&d, turn(1), write_req("src/a.rs"));
    let id = b.approval(0).await;
    *b.store.receipt_gate.lock().unwrap() = Some(notify());
    let decide = spawn_decide(&b, id, UserDecision::Deny);
    until("拒绝落盘", || {
        b.count(|e| matches!(e, DomainEvent::ApprovalDecided(_))) == 1
    })
    .await;
    decide.abort();
    let _ = decide.await;
    b.engine
        .switch_mode(
            ApprovalModeTarget::Conversation(conversation()),
            Some(Auto),
            user(),
        )
        .await
        .unwrap();
    assert!(is_deny(&settle(h).await, MSG_USER_DENIED));
    let again = b.call(&d, turn(2), write_req("src/a.rs")).await;
    assert!(is_deny(&again, MSG_USER_DENIED), "{again:?}");
    fold_matches_log(&b).await;
}

/// 3/F-3 ①：拒绝 A 的 decide 被丢弃，(a) 已处理 / (b) 还在队列；随后切自动，同指纹的 B 不被放行。
#[tokio::test]
async fn t_mode_switch_after_cancelled_denial() {
    for queued in [false, true] {
        let b = Bench::new(Some(Manual)).await;
        let d = b.decider();
        let ha = b.ask(&d, turn(1), write_req("src/a.rs"));
        let a = b.approval(0).await;
        let hb = b.ask(&d, turn(1), write_req("src/a.rs"));
        let bid = b.approval(1).await;
        let gate = notify();
        if queued {
            *b.store.before_commit.lock().unwrap() = Some(gate.clone());
        } else {
            *b.store.receipt_gate.lock().unwrap() = Some(gate.clone());
        }
        let decide = spawn_decide(&b, a, UserDecision::Deny);
        if queued {
            until("拒绝入队", || {
                b.store.commit_entered.load(Ordering::SeqCst)
            })
            .await;
        } else {
            until("拒绝落盘", || {
                b.count(|e| matches!(e, DomainEvent::ApprovalDecided(_))) == 1
            })
            .await;
        }
        decide.abort();
        let _ = decide.await;
        let empties = b.store.empty_commits.load(Ordering::SeqCst);
        let engine = b.engine.clone();
        let switch = tokio::spawn(async move {
            engine
                .switch_mode(
                    ApprovalModeTarget::Conversation(conversation()),
                    Some(Auto),
                    user(),
                )
                .await
        });
        if queued {
            settle_tasks().await;
            assert!(!switch.is_finished(), "屏障等队列里的拒绝");
            gate.notify_one();
        }
        switch.await.unwrap().unwrap();
        assert!(no_approved(&b, bid), "queued={queued}");
        assert!(b.pending(bid).await.is_some());
        assert_eq!(
            b.store.empty_commits.load(Ordering::SeqCst),
            empties + 1,
            "被丢弃的决定之后，下一位过一次屏障"
        );
        settle(ha).await;
        b.engine
            .decide(bid, UserDecision::Deny, user())
            .await
            .unwrap();
        settle(hb).await;
    }
}

/// 3/F-3 ②：两次切档按 seq 各应用一次，并发追平不把档位改回去。
#[tokio::test]
async fn t_fold_never_goes_backwards() {
    let b = Bench::new(Some(Manual)).await;
    let target = ApprovalModeTarget::Conversation(conversation());
    b.engine
        .switch_mode(target, Some(Auto), user())
        .await
        .unwrap();
    let (x, y, z) = tokio::join!(
        b.engine.switch_mode(target, Some(Manual), user()),
        b.engine.sync(),
        b.engine.catch_up()
    );
    x.unwrap();
    y.unwrap();
    z.unwrap();
    b.engine.sync().await.unwrap();
    assert_eq!(b.mode(conversation(), assistant()).await, Manual);
    fold_matches_log(&b).await;
}

/// 3/F-4：审查在跑；作废计划交出；审查出结论（在令牌上排队）；作废没落盘、丢句柄：结论照常生效。
#[tokio::test]
async fn t_review_result_during_failed_void_settles() {
    let b = Bench::new(Some(Auto)).await;
    let gate = notify();
    b.reviewer.push(ReviewScript::SaysAfter(
        "{\"pass\": true, \"reason\": \"ok\"}".into(),
        gate.clone(),
    ));
    let h = b.ask(
        &b.decider(),
        turn(1),
        read_req("/home/u/Documents/notes/x.txt"),
    );
    let id = b.approval(0).await;
    assert!(matches!(
        b.pending(id).await.unwrap().review,
        ReviewState::InProgress { .. }
    ));
    let (_plan, hold) = b
        .engine
        .void_plan(conversation(), turn(1), VoidReason::TurnCancelled)
        .await
        .unwrap();
    gate.notify_one();
    settle_tasks().await;
    assert!(!h.is_finished(), "结论在令牌上排队");
    drop(hold); // 作废那笔没提交
    assert_eq!(settle(h).await, PermissionDecision::Allow);
    assert!(!no_approved(&b, id));
}

/// 3/F-5：对话删除 / 助手删除 / 回合取消落盘后，同回合的新请求在求值之前被拒，不建审批。
#[tokio::test]
async fn t_deleting_conversation_blocks_new_requests() {
    let b = Bench::new(Some(Manual)).await;
    let d = b.decider();
    let h = b.ask(&d, turn(1), write_req("src/a.rs"));
    let _ = b.approval(0).await;
    let gate = notify();
    *b.store.before_commit.lock().unwrap() = Some(gate.clone());
    let engine = b.engine.clone();
    let deleting = tokio::spawn(async move {
        engine
            .commit_with(
                Scope::ConversationDeleting(conversation()),
                command(vec![
                    ConversationDeleting {
                        conversation_id: conversation(),
                    }
                    .into(),
                ]),
            )
            .await
    });
    until("删除入队", || {
        b.store.commit_entered.load(Ordering::SeqCst)
    })
    .await;
    // 落盘前：新请求在令牌上等
    let d2 = d.clone();
    let before =
        tokio::spawn(async move { d2.decide(turn(1), &write_req("src/b.rs")).await.decision });
    settle_tasks().await;
    assert!(!before.is_finished());
    gate.notify_one();
    deleting.await.unwrap().unwrap();
    assert!(is_deny(&before.await.unwrap(), MSG_TURN_CANCELLED));
    // 落盘后
    assert!(is_deny(
        &d.decide(turn(1), &write_req("src/c.rs")).await.decision,
        MSG_TURN_CANCELLED
    ));
    assert_eq!(
        b.count(|e| matches!(e, DomainEvent::ApprovalRequested(_))),
        1
    );
    settle(h).await;
}

#[tokio::test]
async fn t_deleted_assistant_blocks_new_requests() {
    let b = Bench::new(Some(Manual)).await;
    let d = b.decider();
    b.waiting(turn(1), assistant());
    b.engine
        .commit_with(
            Scope::AssistantDeleted(assistant()),
            command(vec![
                AssistantDeleted {
                    assistant_id: assistant(),
                    name_snapshot: "a".into(),
                }
                .into(),
            ]),
        )
        .await
        .unwrap();
    assert!(is_deny(
        &d.decide(turn(1), &write_req("src/a.rs")).await.decision,
        MSG_TURN_CANCELLED
    ));
    assert_eq!(
        b.count(|e| matches!(e, DomainEvent::ApprovalRequested(_))),
        0
    );
    // 对照：别的助手照常问
    let other = Arc::new(b.engine.decider_for(keys_with(&[], other_assistant())));
    let h = b.ask(&other, turn(2), write_req("src/a.rs"));
    let id = b.approval(0).await;
    b.engine
        .decide(id, UserDecision::Deny, user())
        .await
        .unwrap();
    settle(h).await;
}

#[tokio::test]
async fn t_cancelled_turn_blocks_new_requests() {
    let b = Bench::new(Some(Manual)).await;
    let d = b.decider();
    let h = b.ask(&d, turn(1), write_req("src/a.rs"));
    let _ = b.approval(0).await;
    b.void_turn(&d, turn(1), VoidReason::TurnCancelled).await;
    assert!(is_deny(&settle(h).await, MSG_TURN_CANCELLED));
    assert!(is_deny(
        &d.decide(turn(1), &write_req("src/b.rs")).await.decision,
        MSG_TURN_CANCELLED
    ));
    assert_eq!(
        b.count(|e| matches!(e, DomainEvent::ApprovalRequested(_))),
        1
    );
}

/// 3/F-6：卡片启动先落盘，旧的作废计划后落盘：作废那笔整条被拒；日志、投影、折叠一致是 Starting。
#[tokio::test]
async fn t_card_start_beats_stale_void() {
    let b = Bench::new(Some(Manual)).await;
    let h = b.ask(&b.decider(), turn(1), bash_req("cat ~/.ssh/id_rsa"));
    let card = card_n(&b, 0).await;
    let (plan, hold) = b
        .engine
        .void_plan(conversation(), turn(1), VoidReason::TurnCancelled)
        .await
        .unwrap();
    assert_eq!(plan.len(), 1);
    // T10 的执行器不经引擎写 Starting
    b.store
        .seed(vec![card_state(card, UserActionState::Starting)]);
    {
        use iwork_ports::store::StoragePort;
        let rejected = b.store.commit(command(plan)).await;
        assert!(matches!(
            rejected,
            Err(iwork_ports::store::StorageError::Projection { .. })
        ));
    }
    drop(hold);
    b.engine.sync().await.unwrap();
    let folded = b.engine.folded();
    let conv = folded.conversation(conversation()).unwrap();
    assert_eq!(conv.cards[&card].state, UserActionState::Starting);
    assert!(matches!(
        b.store.projection.lock().unwrap().cards[&card].0,
        UserActionState::Starting
    ));
    assert!(
        b.engine
            .void_plan(conversation(), turn(1), VoidReason::TurnCancelled)
            .await
            .unwrap()
            .0
            .is_empty(),
        "不再是 Pending，不进计划"
    );
    assert!(!h.is_finished(), "卡片还没结果");
    h.abort();
}

/// 3/F-6 反过来：作废先落盘，启动那笔被拒；挂着的 hook 收到「回合已取消」。
#[tokio::test]
async fn t_card_void_beats_start() {
    let b = Bench::new(Some(Manual)).await;
    let d = b.decider();
    let h = b.ask(&d, turn(1), bash_req("cat ~/.ssh/id_rsa"));
    let card = card_n(&b, 0).await;
    b.void_turn(&d, turn(1), VoidReason::TurnCancelled).await;
    let start = b
        .engine
        .commit_with(
            Scope::Card(conversation()),
            command(vec![card_state(card, UserActionState::Starting)]),
        )
        .await;
    assert!(matches!(
        start,
        Err(iwork_core::approval::engine::EngineError::Storage(
            iwork_ports::store::StorageError::Projection { .. }
        ))
    ));
    assert!(is_deny(&settle(h).await, MSG_TURN_CANCELLED));
}

/// 3/F-7：回合取消的计划没落盘，对话删除的计划随后提交：删除落盘前 decide 在等；之后 `NotPending`。
#[tokio::test]
async fn t_overlapping_void_plans_one_fails() {
    let b = Bench::new(Some(Manual)).await;
    let h = b.ask(&b.decider(), turn(1), write_req("src/a.rs"));
    let id = b.approval(0).await;
    let (_plan, hold) = b
        .engine
        .void_plan(conversation(), turn(1), VoidReason::TurnCancelled)
        .await
        .unwrap();
    let engine = b.engine.clone();
    let deleting = tokio::spawn(async move {
        engine
            .commit_with(
                Scope::ConversationDeleting(conversation()),
                command(vec![
                    ConversationDeleting {
                        conversation_id: conversation(),
                    }
                    .into(),
                ]),
            )
            .await
    });
    settle_tasks().await;
    let decide = spawn_decide(&b, id, allow(RememberScope::Project));
    settle_tasks().await;
    assert!(!deleting.is_finished() && !decide.is_finished());
    drop(hold); // 回合取消那笔失败了
    let attached = deleting.await.unwrap().unwrap().attached;
    assert!(
        attached
            .iter()
            .any(|e| matches!(e, DomainEvent::ApprovalVoided(v) if v.approval_id == id))
    );
    assert_eq!(decide.await.unwrap(), Err(DecideError::NotPending(id)));
    assert_eq!(rules_remembered(&b), 0);
    settle(h).await;
}

/// 3/F-8 (a)：请求在入队之前被丢弃：折叠、等待登记里都没有它，作废计划为空。
#[tokio::test]
async fn t_cancelled_uncommitted_request_leaves_nothing() {
    let b = Bench::new(Some(Manual)).await;
    let gate = notify();
    *b.store.enqueue_gate.lock().unwrap() = Some(gate.clone());
    let h = b.ask(&b.decider(), turn(1), write_req("src/a.rs"));
    until("请求停在入队门口", || {
        b.store.at_enqueue_gate.load(Ordering::SeqCst)
    })
    .await;
    h.abort();
    let _ = h.await;
    gate.notify_one();
    settle_tasks().await;
    assert_eq!(
        b.count(|e| matches!(e, DomainEvent::ApprovalRequested(_))),
        0
    );
    let (plan, _) = b
        .engine
        .void_plan(conversation(), turn(1), VoidReason::TurnCancelled)
        .await
        .unwrap();
    assert!(plan.is_empty());
    fold_matches_log(&b).await;
}

/// 3/F-8 (b)：处理了、回执没到时丢弃：折叠里是一条正常的待审批（没有等待方），照常能处理。
#[tokio::test]
async fn t_cancelled_committed_request_is_pending_detached() {
    let b = Bench::new(Some(Manual)).await;
    *b.store.receipt_gate.lock().unwrap() = Some(notify());
    let h = b.ask(&b.decider(), turn(1), write_req("src/a.rs"));
    let id = b.approval(0).await;
    h.abort();
    let _ = h.await;
    assert_eq!(b.pending(id).await.unwrap().state, ApprovalState::Pending);
    b.engine
        .decide(id, UserDecision::Deny, user())
        .await
        .unwrap();
    assert!(b.pending(id).await.is_none());
}

// ---------- T06 的 R1–R3 在引擎上 ----------

/// W1 之后丢掉引擎、`start`、T04 提交 `Interrupted{Crash}`：一份放行，记录与原审批相等。
#[tokio::test]
async fn r1_crash_before_delivery_grants_after_restart() {
    let b = Bench::new(Some(Manual)).await;
    let h = b.ask(&b.decider(), turn(1), bash_req("rm a.txt && touch a.txt"));
    let id = b.approval(0).await;
    let record = b.pending(id).await.unwrap().call.unwrap();
    b.engine
        .decide(id, allow(RememberScope::Once), user())
        .await
        .unwrap();
    assert_eq!(settle(h).await, PermissionDecision::Allow);
    let (fresh, _) = b.restart(|_, _| Some(keys())).await;
    assert!(fresh.grant(id).await.is_none(), "还在等交付");
    fresh
        .external(vec![state_changed(
            turn(1),
            TurnState::WaitingApproval,
            TurnState::Interrupted {
                reason: InterruptReason::Crash,
            },
        )])
        .await;
    assert_eq!(fresh.grant(id).await.unwrap().record, *record);
    assert_eq!(fresh.engine.turns_to_resume(), vec![turn(1)]);
}

/// W1 留下放行 A 并续接回到等审批。返回 A。
async fn w1_grant(b: &Bench, d: &Arc<Decider_>, t: TurnId, cmd: &str) -> ApprovalId {
    let n = b.count(|e| matches!(e, DomainEvent::ApprovalRequested(_)));
    let h = b.ask(d, t, bash_req(cmd));
    let id = b.approval(n).await;
    b.engine
        .decide(id, allow(RememberScope::Once), user())
        .await
        .unwrap();
    assert_eq!(settle(h).await, PermissionDecision::Allow);
    b.interrupt(t).await;
    assert!(b.grant(id).await.is_some());
    b.waiting(t, d.keys().assistant);
    id
}

#[tokio::test]
async fn r3_grant_hit_returns_approval_and_is_single_use() {
    let b = Bench::new(Some(Manual)).await;
    let d = b.decider();
    let a = w1_grant(&b, &d, turn(1), "rm a.txt && touch a.txt").await;
    let req = bash_req("rm a.txt && touch a.txt");
    let first = b.engine.on_permission(d.keys(), turn(1), &req).await;
    assert_eq!(
        first,
        Decided {
            decision: PermissionDecision::Allow,
            approval: Some(a)
        }
    );
    // 第二次排在第一次的审计之后、交付之前：找不到放行，照常问
    let n = b.count(|e| matches!(e, DomainEvent::ApprovalRequested(_)));
    let h = b.ask(&d, turn(1), req.clone());
    let again = b.approval(n).await;
    assert_ne!(again, a);
    b.external(vec![delivered(a, turn(1))]).await;
    assert!(b.grant(a).await.is_none());
    b.engine
        .decide(again, UserDecision::Deny, user())
        .await
        .unwrap();
    settle(h).await;
}

#[tokio::test]
async fn r3_grant_hit_then_interrupted_keeps_grant() {
    let b = Bench::new(Some(Manual)).await;
    let d = b.decider();
    let a = w1_grant(&b, &d, turn(1), "rm a.txt && touch a.txt").await;
    let req = bash_req("rm a.txt && touch a.txt");
    assert_eq!(
        b.engine
            .on_permission(d.keys(), turn(1), &req)
            .await
            .approval,
        Some(a)
    );
    assert!(b.grant(a).await.is_none(), "挪去等交付");
    b.interrupt(turn(1)).await;
    assert!(b.grant(a).await.is_some(), "没交付又中断：放行还在");
    b.waiting(turn(1), assistant());
    assert_eq!(
        b.engine.on_permission(d.keys(), turn(1), &req).await,
        Decided {
            decision: PermissionDecision::Allow,
            approval: Some(a)
        }
    );
}

/// `Decided.approval`：规则放行 / 规则拒绝 / 用户允许 / 用户拒绝 / 卡片 → None、None、Some、Some、None。
#[tokio::test]
async fn decided_carries_approval_id() {
    let b = Bench::new(Some(Manual)).await;
    let d = b.decider();
    let k = d.keys().clone();
    b.waiting(turn(1), assistant());
    let allow_rule = b.engine.on_permission(&k, turn(1), &bash_req("ls")).await;
    assert_eq!(allow_rule.decision, PermissionDecision::Allow);
    assert_eq!(allow_rule.approval, None);
    let deny_rule = b
        .engine
        .on_permission(&k, turn(1), &write_req(&format!("{DATA}/x")))
        .await;
    assert!(matches!(
        deny_rule.decision,
        PermissionDecision::Deny { .. }
    ));
    assert_eq!(deny_rule.approval, None);
    for (n, decision) in [(0, allow(RememberScope::Once)), (1, UserDecision::Deny)] {
        let engine = b.engine.clone();
        let k2 = k.clone();
        let h = tokio::spawn(async move {
            engine
                .on_permission(&k2, turn(1), &write_req("src/a.rs"))
                .await
        });
        let id = b.approval(n).await;
        b.engine.decide(id, decision, user()).await.unwrap();
        assert_eq!(h.await.unwrap().approval, Some(id));
    }
    let engine = b.engine.clone();
    let k2 = k.clone();
    let h = tokio::spawn(async move {
        engine
            .on_permission(&k2, turn(1), &bash_req("cat ~/.ssh/id_rsa"))
            .await
    });
    let card = card_n(&b, 0).await;
    b.external(vec![card_state(card, UserActionState::Dismissed)])
        .await;
    assert_eq!(h.await.unwrap().approval, None);
}

// ---------- 方案审查第 1 轮 ----------

/// 1/F-1：同一调用的请求已取完视图、命中 A，审计还没入队；编排先落 `Interrupted`。
/// (a) 请求活着；(b) 审计入队后丢掉请求。两种都判过期的消费，A 留在放行里。
#[tokio::test]
async fn f1_interrupt_before_grant_audit_keeps_grant() {
    for drop_request in [false, true] {
        let b = Bench::new(Some(Manual)).await;
        let d = b.decider();
        let a = w1_grant(&b, &d, turn(1), "rm a.txt && touch a.txt").await;
        let gate = notify();
        *b.store.enqueue_gate.lock().unwrap() = Some(gate.clone());
        let engine = b.engine.clone();
        let k = d.keys().clone();
        let request = tokio::spawn(async move {
            engine
                .on_permission(&k, turn(1), &bash_req("rm a.txt && touch a.txt"))
                .await
        });
        until("审计停在入队门口", || {
            b.store.at_enqueue_gate.load(Ordering::SeqCst)
        })
        .await;
        b.interrupt(turn(1)).await;
        let interrupted_at = b.store.stored().last().unwrap().seq;
        gate.notify_one();
        if drop_request {
            until("审计入队", || b.store.queued() > 0 || b.count(|e| matches!(e, DomainEvent::ApprovalEvaluated(x) if x.grant_approval_id == Some(a))) == 1).await;
            request.abort();
            let _ = request.await;
            until("审计落盘", || {
                b.count(|e| matches!(e, DomainEvent::ApprovalEvaluated(x) if x.grant_approval_id == Some(a))) == 1
            })
            .await;
        } else {
            assert_eq!(request.await.unwrap().decision, PermissionDecision::Allow);
        }
        let audit = b
            .store
            .stored()
            .into_iter()
            .find_map(|e| match e.event {
                DomainEvent::ApprovalEvaluated(x) if x.grant_approval_id == Some(a) => {
                    Some((e.seq, x))
                }
                _ => None,
            })
            .unwrap();
        assert!(audit.0 > interrupted_at, "审计落在中断之后");
        assert!(audit.1.live_rules_seq < interrupted_at, "求值在中断之前");
        assert!(b.grant(a).await.is_some(), "过期的消费不挪动放行");
        assert_eq!(b.engine.turns_to_resume(), vec![turn(1)]);
        // 再续接：同一调用命中一次并交付，之后没有放行
        b.waiting(turn(1), assistant());
        assert_eq!(
            b.engine
                .on_permission(d.keys(), turn(1), &bash_req("rm a.txt && touch a.txt"))
                .await
                .approval,
            Some(a)
        );
        b.external(vec![delivered(a, turn(1))]).await;
        assert!(b.grant(a).await.is_none());
        fold_matches_log(&b).await;
    }
}

/// 1/F-1 再反向一层：旧审计落在回合又进入等审批之后，仍不挪动 A；新一代的请求命中 A。
#[tokio::test]
async fn f1_next_generation_consumes_after_stale_audit() {
    let b = Bench::new(Some(Manual)).await;
    let d = b.decider();
    let a = w1_grant(&b, &d, turn(1), "rm a.txt && touch a.txt").await;
    let gate = notify();
    *b.store.enqueue_gate.lock().unwrap() = Some(gate.clone());
    let engine = b.engine.clone();
    let k = d.keys().clone();
    let old = tokio::spawn(async move {
        engine
            .on_permission(&k, turn(1), &bash_req("rm a.txt && touch a.txt"))
            .await
    });
    until("旧审计停在入队门口", || {
        b.store.at_enqueue_gate.load(Ordering::SeqCst)
    })
    .await;
    b.interrupt(turn(1)).await;
    b.waiting(turn(1), assistant()); // 续接并再次等审批
    gate.notify_one();
    let _ = old.await.unwrap();
    assert!(b.grant(a).await.is_some());
    let new = b
        .engine
        .on_permission(d.keys(), turn(1), &bash_req("rm a.txt && touch a.txt"))
        .await;
    assert_eq!(
        new,
        Decided {
            decision: PermissionDecision::Allow,
            approval: Some(a)
        }
    );
    b.external(vec![delivered(a, turn(1))]).await;
    assert!(b.grant(a).await.is_none());
    assert_eq!(
        b.count(|e| matches!(e, DomainEvent::ApprovalDelivered(x) if x.approval_id == a)),
        1
    );
}

/// 第二个项目对话（不在助手删除那一笔的令牌集合里）。
fn second_conversation() -> ConversationId {
    ConversationId::from_raw(RawUlid::from_u128(0x32))
}

fn add_second_conversation(b: &Bench) -> Arc<Decider_> {
    let mut c = fold_fx::project_conversation(2, 1);
    c.id = second_conversation();
    c.kind = iwork_domain::conversation::ConversationKind::Project {
        project_id: project(),
    };
    b.store
        .seed(vec![ConversationCreated { conversation: c }.into()]);
    let mut k = keys_with(&[], other_assistant());
    k.conversation = second_conversation();
    Arc::new(b.engine.decider_for(k))
}

fn turn_in(b: &Bench, t: TurnId, c: ConversationId, a: AssistantId) {
    b.store.seed(fold_fx::waiting_turn(t, c, a));
}

fn delete_other() -> iwork_ports::store::CommitRequest {
    command(vec![
        AssistantDeleted {
            assistant_id: other_assistant(),
            name_snapshot: "o".into(),
        }
        .into(),
    ])
}

/// 助手 a（other）在 C1 有待审批；C2 的请求取完视图后，删除先落盘：C2 那笔被写线程拒，重算后回拒绝。
#[tokio::test]
async fn f2_request_after_assistant_deleted_is_rejected() {
    let b = Bench::new(Some(Manual)).await;
    let d1 = Arc::new(b.engine.decider_for(keys_with(&[], other_assistant())));
    let h1 = b.ask(&d1, turn(1), write_req("src/a.rs"));
    let _ = b.approval(0).await;
    let d2 = add_second_conversation(&b);
    turn_in(&b, turn(2), second_conversation(), other_assistant());
    let gate = notify();
    *b.store.enqueue_gate.lock().unwrap() = Some(gate.clone());
    let req2 = tokio::spawn({
        let d2 = d2.clone();
        async move { d2.decide(turn(2), &write_req("src/b.rs")).await.decision }
    });
    until("C2 的请求停在入队门口", || {
        b.store.at_enqueue_gate.load(Ordering::SeqCst)
    })
    .await;
    b.engine
        .commit_with(Scope::AssistantDeleted(other_assistant()), delete_other())
        .await
        .unwrap();
    gate.notify_one();
    assert!(is_deny(&req2.await.unwrap(), MSG_TURN_CANCELLED));
    assert_eq!(
        b.count(|e| matches!(e, DomainEvent::ApprovalRequested(_))),
        1
    );
    assert!(is_deny(&settle(h1).await, MSG_TURN_CANCELLED));
}

/// 删除已枚举（只有 C1）、停在入队门口；C2 的审批先落盘；删除落盘后清孤儿把 C2 那条作废。
#[tokio::test]
async fn f2_request_before_assistant_deleted_is_swept() {
    let b = Bench::new(Some(Manual)).await;
    let d1 = Arc::new(b.engine.decider_for(keys_with(&[], other_assistant())));
    let h1 = b.ask(&d1, turn(1), write_req("src/a.rs"));
    let _ = b.approval(0).await;
    let d2 = add_second_conversation(&b);
    turn_in(&b, turn(2), second_conversation(), other_assistant());
    let gate = notify();
    *b.store.enqueue_gate.lock().unwrap() = Some(gate.clone());
    let engine = b.engine.clone();
    let deleting = tokio::spawn(async move {
        engine
            .commit_with(Scope::AssistantDeleted(other_assistant()), delete_other())
            .await
    });
    until("删除停在入队门口", || {
        b.store.at_enqueue_gate.load(Ordering::SeqCst)
    })
    .await;
    let h2 = tokio::spawn({
        let d2 = d2.clone();
        async move { d2.decide(turn(2), &write_req("src/b.rs")).await.decision }
    });
    let c2 = b.approval(1).await;
    gate.notify_one();
    let attached = deleting.await.unwrap().unwrap().attached;
    assert!(
        !attached
            .iter()
            .any(|e| matches!(e, DomainEvent::ApprovalVoided(v) if v.approval_id == c2))
    );
    assert!(b.count(|e| matches!(e, DomainEvent::ApprovalVoided(v) if v.approval_id == c2 && v.reason == VoidReason::AssistantDeleted)) == 1, "清孤儿作废了它");
    assert!(is_deny(&h2.await.unwrap(), MSG_TURN_CANCELLED));
    assert!(is_deny(&settle(h1).await, MSG_TURN_CANCELLED));
    assert!(b.engine.folded().orphans().is_empty());
    let projection = b.store.projection.lock().unwrap();
    assert!(
        projection
            .approvals
            .values()
            .all(|(s, _, _, _)| *s != ApprovalState::Pending)
    );
}

/// 造一条孤儿：删除已枚举（C2 还没有审批）、停在入队门口；C2 的审批先落盘；删除落盘后、回执没到就丢掉
/// `commit_with`（清孤儿没发生）。
async fn orphan(b: &Bench) -> (ApprovalId, tokio::task::JoinHandle<PermissionDecision>) {
    let d2 = add_second_conversation(b);
    turn_in(b, turn(2), second_conversation(), other_assistant());
    let gate = notify();
    *b.store.enqueue_gate.lock().unwrap() = Some(gate.clone());
    let engine = b.engine.clone();
    let deleting = tokio::spawn(async move {
        engine
            .commit_with(Scope::AssistantDeleted(other_assistant()), delete_other())
            .await
    });
    until("删除停在入队门口", || {
        b.store.at_enqueue_gate.load(Ordering::SeqCst)
    })
    .await;
    let h2 = tokio::spawn({
        let d2 = d2.clone();
        async move { d2.decide(turn(2), &write_req("src/b.rs")).await.decision }
    });
    let id = b.approval(0).await;
    until("请求追平完", || {
        b.engine.pending_approval(id).is_some()
    })
    .await;
    *b.store.receipt_gate.lock().unwrap() = Some(notify());
    gate.notify_one();
    until("删除落盘", || {
        b.count(|e| matches!(e, DomainEvent::AssistantDeleted(_))) == 1
    })
    .await;
    deleting.abort();
    let _ = deleting.await;
    b.engine.catch_up().await.unwrap();
    assert!(b.pending(id).await.is_some(), "孤儿还在");
    (id, h2)
}

#[tokio::test]
async fn f2_orphan_cannot_be_approved_before_sweep() {
    // (a) 引擎路径
    let b = Bench::new(Some(Manual)).await;
    let (id, h) = orphan(&b).await;
    assert_eq!(
        b.engine
            .decide(id, allow(RememberScope::Project), user())
            .await,
        Err(DecideError::OwnerGone(id))
    );
    b.engine
        .decide(id, UserDecision::Deny, user())
        .await
        .unwrap();
    settle(h).await;
    // (b) 绕过引擎直接提交批准 + 记住规则：整笔被拒
    let b = Bench::new(Some(Manual)).await;
    let (id, h) = orphan(&b).await;
    {
        use iwork_ports::store::StoragePort;
        let rule = fold_fx::rule(9);
        let rejected = b
            .store
            .commit(command(vec![
                approved_by_user(id),
                RuleRemembered { rule }.into(),
            ]))
            .await;
        assert!(matches!(
            rejected,
            Err(iwork_ports::store::StorageError::Projection { .. })
        ));
    }
    assert_eq!(rules_remembered(&b), 0);
    b.engine
        .decide(id, UserDecision::Deny, user())
        .await
        .unwrap();
    settle(h).await;
}

#[tokio::test]
async fn f2_sweep_taken_over_by_sync_after_command_dropped() {
    let b = Bench::new(Some(Manual)).await;
    let (id, h) = orphan(&b).await;
    assert!(!h.is_finished(), "对照：不调 sync 时孤儿仍是 Pending");
    b.engine.sync().await.unwrap();
    assert_eq!(
        b.count(|e| matches!(e, DomainEvent::ApprovalVoided(v) if v.approval_id == id && v.reason == VoidReason::AssistantDeleted)),
        1
    );
    assert!(is_deny(&settle(h).await, MSG_TURN_CANCELLED));
    assert!(b.engine.folded().orphans().is_empty());
}

#[tokio::test]
async fn f2_sweep_taken_over_by_start_after_crash() {
    let b = Bench::new(Some(Manual)).await;
    let (id, h) = orphan(&b).await;
    h.abort();
    let (fresh, plan) = b.restart(|_, _| Some(keys())).await;
    assert!(plan.iter().any(|e| matches!(e, DomainEvent::ApprovalVoided(v) if v.approval_id == id && v.reason == VoidReason::AssistantDeleted)));
    assert!(fresh.pending(id).await.is_none());
    assert!(fresh.engine.folded().orphans().is_empty());
}

/// 两个对话各一条孤儿：第一次清理中途失败只清掉一个，再调一次接着清；再调一次什么都不提交。
#[tokio::test]
async fn f2_sweep_is_idempotent_and_resumable() {
    let b = Bench::new(Some(Manual)).await;
    let d1 = Arc::new(b.engine.decider_for(keys_with(&[], other_assistant())));
    let h1 = b.ask(&d1, turn(1), write_req("src/a.rs"));
    let _ = b.approval(0).await;
    let (_, h2) = orphan_after(&b).await;
    b.store.fail_next.store(true, Ordering::SeqCst);
    assert!(b.engine.sync().await.is_err(), "第一个对话那笔 Busy");
    assert_eq!(b.engine.folded().orphans().len(), 1, "另一个对话照样清掉");
    b.engine.sync().await.unwrap();
    assert!(b.engine.folded().orphans().is_empty());
    let commits = b.store.commits.load(Ordering::SeqCst);
    b.engine.sync().await.unwrap();
    assert_eq!(b.store.commits.load(Ordering::SeqCst), commits, "幂等");
    settle(h1).await;
    settle(h2).await;
}

/// 两个对话里都有 other 的待审批，删除那一笔绕过引擎直接落盘（孤儿留在两个对话里）。
async fn orphan_after(b: &Bench) -> (ApprovalId, tokio::task::JoinHandle<PermissionDecision>) {
    let d2 = add_second_conversation(b);
    turn_in(b, turn(2), second_conversation(), other_assistant());
    let h2 = tokio::spawn({
        let d2 = d2.clone();
        async move { d2.decide(turn(2), &write_req("src/b.rs")).await.decision }
    });
    let id = b.approval(1).await;
    b.store.seed(vec![
        AssistantDeleted {
            assistant_id: other_assistant(),
            name_snapshot: "o".into(),
        }
        .into(),
    ]);
    b.engine.catch_up().await.unwrap();
    assert_eq!(b.engine.folded().orphans().len(), 2);
    (id, h2)
}

/// 对话 C 里已有孤儿；`commit_with(Card(C))` 先拿到回执、放掉令牌，再清孤儿（不重入同一把令牌）。
/// 清孤儿那笔 Busy 时 `commit_with` 仍返回 Ok，孤儿留着。
#[tokio::test]
async fn commit_with_sweeps_same_conversation_without_reentry() {
    for sweep_fails in [false, true] {
        let b = Bench::new(Some(Manual)).await;
        let d1 = Arc::new(b.engine.decider_for(keys_with(&[], other_assistant())));
        let h1 = b.ask(&d1, turn(1), write_req("src/a.rs"));
        let id = b.approval(0).await;
        b.store.seed(vec![
            AssistantDeleted {
                assistant_id: other_assistant(),
                name_snapshot: "o".into(),
            }
            .into(),
        ]);
        let gate = notify();
        *b.store.receipt_gate.lock().unwrap() = Some(gate.clone());
        let engine = b.engine.clone();
        let card_cmd = tokio::spawn(async move {
            engine
                .commit_with(
                    Scope::Card(conversation()),
                    command(vec![
                        MessageAppended {
                            message: Message {
                                id: MessageId::from_raw(RawUlid::from_u128(0x79)),
                                conversation_id: conversation(),
                                author: Author::User,
                                context_project_id: None,
                                status: MessageStatus::Sent,
                                body: MessageBody::Inline("x".into()),
                                attachments: vec![],
                                created_at: Timestamp(1),
                                turn_id: None,
                            },
                        }
                        .into(),
                    ]),
                )
                .await
        });
        until("命令落盘", || {
            b.count(|e| matches!(e, DomainEvent::MessageAppended(_))) == 1
        })
        .await;
        settle_tasks().await;
        assert_eq!(
            b.count(|e| matches!(e, DomainEvent::ApprovalVoided(_))),
            0,
            "回执到之前不清"
        );
        if sweep_fails {
            b.store.fail_next.store(true, Ordering::SeqCst);
        }
        gate.notify_one();
        card_cmd.await.unwrap().unwrap();
        let voided =
            b.count(|e| matches!(e, DomainEvent::ApprovalVoided(v) if v.approval_id == id));
        assert_eq!(
            voided,
            usize::from(!sweep_fails),
            "sweep_fails={sweep_fails}"
        );
        if sweep_fails {
            b.engine.sync().await.unwrap();
        }
        assert!(is_deny(&settle(h1).await, MSG_TURN_CANCELLED));
    }
}

// ---------- 方案审查第 1 轮 F-5：同一回合两份会话快照 ----------

fn keys_disabling(group: iwork_domain::common::ToolGroup) -> SessionKeys {
    let mut k = keys();
    k.disabled_groups = [group].into_iter().collect();
    k
}

/// K1 禁用跑命令、K2 禁用改文件。回合 T 在 K1 下写文件（询问，P1）→ 中断 → 续接到 K2 → 跑命令（询问，P2）。
/// 每条在自己的快照下都问得出来；套上对方的快照就会因工具被禁用而不能放行。
async fn two_snapshots(b: &Bench) -> (Arc<Decider_>, Arc<Decider_>, ApprovalId, ApprovalId) {
    use iwork_domain::common::ToolGroup;
    let d1 = Arc::new(b.engine.decider_for(keys_disabling(ToolGroup::RunCommands)));
    let d2 = Arc::new(b.engine.decider_for(keys_disabling(ToolGroup::EditFiles)));
    let _h1 = b.ask(&d1, turn(1), write_req("src/x.rs"));
    let p1 = b.approval(0).await;
    b.interrupt(turn(1)).await;
    let _h2 = b.ask(&d2, turn(1), bash_req("npm test"));
    let p2 = b.approval(1).await;
    (d1, d2, p1, p2)
}

#[tokio::test]
async fn f5_two_snapshots_same_turn_each_approval_uses_its_own() {
    let b = Bench::new(Some(Manual)).await;
    let (d1, d2, p1, p2) = two_snapshots(&b).await;
    assert!(Arc::ptr_eq(&b.engine.binding_keys(p1).unwrap(), d1.keys()));
    assert!(Arc::ptr_eq(&b.engine.binding_keys(p2).unwrap(), d2.keys()));
    b.engine
        .decide(p1, allow(RememberScope::Once), user())
        .await
        .unwrap();
    b.engine
        .decide(p2, allow(RememberScope::Once), user())
        .await
        .unwrap();
}

#[tokio::test]
async fn f5_wrong_binding_is_caught() {
    let b = Bench::new(Some(Manual)).await;
    let (_d1, _d2, p1, p2) = two_snapshots(&b).await;
    b.engine.swap_bindings_for_test(p1, p2);
    assert_eq!(
        b.engine
            .decide(p1, allow(RememberScope::Once), user())
            .await,
        Err(DecideError::NoLongerAllowed(p1))
    );
    assert_eq!(
        b.engine
            .decide(p2, allow(RememberScope::Once), user())
            .await,
        Err(DecideError::NoLongerAllowed(p2))
    );
}

#[tokio::test]
async fn f5_phantom_binding_dropped_at_turn_end() {
    let b = Bench::new(Some(Manual)).await;
    let d = b.decider();
    b.waiting(turn(1), assistant());
    b.store.fail_next.store(true, Ordering::SeqCst);
    assert!(is_deny(
        &d.decide(turn(1), &write_req("src/a.rs")).await.decision,
        iwork_core::approval::engine::MSG_STORAGE_FAILED
    ));
    assert!(
        b.engine
            .folded()
            .pending_approval(ApprovalId::from_raw(RawUlid::from_u128(1)))
            .is_none()
    );
    assert_eq!(
        b.engine.binding_count(),
        1,
        "没落盘的请求留下一项没人查的绑定"
    );
    b.external(vec![
        TurnFinished {
            turn_id: turn(1),
            state: TurnState::Cancelled,
            usage: Default::default(),
            celebrate: false,
            change_summary: None,
        }
        .into(),
    ])
    .await;
    assert_eq!(b.engine.binding_count(), 0);
}

// ---------- 新结构自己的用例 ----------

#[tokio::test]
async fn t_barrier_only_when_unsettled() {
    let b = Bench::new(Some(Manual)).await;
    let d = b.decider();
    let h = b.ask(&d, turn(1), write_req("src/a.rs"));
    let id = b.approval(0).await;
    b.engine
        .decide(id, UserDecision::Deny, user())
        .await
        .unwrap();
    settle(h).await;
    assert_eq!(
        b.store.empty_commits.load(Ordering::SeqCst),
        0,
        "平常路径没有屏障"
    );
    let _ = d
        .void_pending(turn(1), VoidReason::TurnCancelled)
        .await
        .unwrap()
        .0;
    b.call(&d, turn(2), bash_req("ls")).await;
    assert_eq!(b.store.empty_commits.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn t_barrier_busy_keeps_flag() {
    let b = Bench::new(Some(Manual)).await;
    let d = b.decider();
    let h = b.ask(&d, turn(1), write_req("src/a.rs"));
    let id = b.approval(0).await;
    let _ = d
        .void_pending(turn(1), VoidReason::TurnCancelled)
        .await
        .unwrap()
        .0;
    b.store.fail_next.store(true, Ordering::SeqCst);
    assert_eq!(
        b.engine.decide(id, UserDecision::Deny, user()).await,
        Err(DecideError::Storage(iwork_ports::store::StorageError::Busy))
    );
    b.engine
        .decide(id, UserDecision::Deny, user())
        .await
        .unwrap();
    assert_eq!(
        b.store.empty_commits.load(Ordering::SeqCst),
        2,
        "屏障 Busy 那次标记留着，第二次再过一遍"
    );
    settle(h).await;
}

/// 每次提交之前都插进一笔改掉计划里另一条审批的提交：第 3 次仍被拒就 `Conflict`。
#[tokio::test]
async fn t_replan_gives_up_after_three() {
    let b = Bench::new(Some(Manual)).await;
    let d = b.decider();
    let mut ids = Vec::new();
    for (n, path) in ["/opt/a", "/opt/b", "/opt/c", "/opt/d"].iter().enumerate() {
        drop(b.ask(&d, turn(1), write_req(path)));
        ids.push(b.approval(n).await);
    }
    let gate = notify();
    *b.store.enqueue_gate.lock().unwrap() = Some(gate.clone());
    let engine = b.engine.clone();
    let switch = tokio::spawn(async move {
        engine
            .switch_mode(
                ApprovalModeTarget::Conversation(conversation()),
                Some(FullAccess),
                user(),
            )
            .await
    });
    for (i, id) in ids.iter().take(3).enumerate() {
        until("切档停在入队门口", || {
            b.store.gate_arrivals.load(Ordering::SeqCst) == i as u64 + 1
        })
        .await;
        b.store.seed(vec![
            ApprovalDecided {
                approval_id: *id,
                state: ApprovalState::Denied,
                remember: None,
            }
            .into(),
        ]);
        if i < 2 {
            *b.store.enqueue_gate.lock().unwrap() = Some(gate.clone());
        }
        gate.notify_one();
    }
    assert_eq!(
        switch.await.unwrap(),
        Err(ModeError::Decision(DecideError::Conflict))
    );
    assert_eq!(
        b.count(|e| matches!(e, DomainEvent::ApprovalModeChanged(_))),
        0
    );
}

#[tokio::test]
async fn t_read_failure_fails_closed() {
    let b = Bench::new(Some(Manual)).await;
    let d = b.decider();
    b.waiting(turn(1), assistant());
    let before = b.store.all().len();
    b.store.fail_read.store(true, Ordering::SeqCst);
    assert!(is_deny(
        &d.decide(turn(1), &bash_req("ls")).await.decision,
        iwork_core::approval::engine::MSG_STORAGE_FAILED
    ));
    assert_eq!(b.store.all().len(), before, "没有新事件");
    assert_eq!(
        d.decide(turn(1), &bash_req("ls")).await.decision,
        PermissionDecision::Allow
    );
}

#[tokio::test]
async fn t_fold_error_names_seq_and_fails_closed() {
    let b = Bench::new(Some(Manual)).await;
    let d = b.decider();
    b.waiting(turn(1), assistant());
    // 绕过提交时校验，往日志里塞一条折叠不认的事件
    let bad = {
        let mut events = b.store.events.lock().unwrap();
        let seq = Seq(events.len() as u64 + 1);
        events.push(iwork_ports::store::StoredEvent {
            seq,
            at: Timestamp(0),
            actor: user(),
            event: approved_by_user(ApprovalId::from_raw(RawUlid::from_u128(0xdead))),
        });
        seq
    };
    assert!(is_deny(
        &d.decide(turn(1), &bash_req("ls")).await.decision,
        iwork_core::approval::engine::MSG_STORAGE_FAILED
    ));
    match b.engine.catch_up().await {
        Err(iwork_core::approval::engine::EngineError::Fold(e)) => {
            assert_eq!(e.seq, bad);
            assert_eq!(e.kind, "approval_decided");
        }
        other => panic!("{other:?}"),
    }
}

/// 外部写者提交终态之后，挂着的 hook 在 `sync` 之后立刻收到答复。
#[tokio::test]
async fn t_sync_wakes_waiter() {
    let b = Bench::new(Some(Manual)).await;
    let h = b.ask(&b.decider(), turn(1), write_req("src/a.rs"));
    let id = b.approval(0).await;
    b.store.seed(vec![voided(id, VoidReason::TurnCancelled)]);
    settle_tasks().await;
    assert!(!h.is_finished(), "没人追平就没人叫");
    b.engine.sync().await.unwrap();
    assert!(is_deny(&settle(h).await, MSG_TURN_CANCELLED));
}

/// 一次性放行跨重启：发放后重建仍在；交付后重建不在。
#[tokio::test]
async fn t_grant_survives_restart_until_consumed() {
    let b = Bench::new(Some(Manual)).await;
    let d = b.decider();
    let a = w1_grant(&b, &d, turn(1), "rm a.txt && touch a.txt").await;
    let (fresh, _) = b.restart(|_, _| Some(keys())).await;
    assert!(fresh.grant(a).await.is_some());
    assert_eq!(
        b.engine
            .on_permission(d.keys(), turn(1), &bash_req("rm a.txt && touch a.txt"))
            .await
            .approval,
        Some(a)
    );
    b.external(vec![delivered(a, turn(1))]).await;
    let (fresh, _) = b.restart(|_, _| Some(keys())).await;
    assert!(fresh.grant(a).await.is_none());
    b.interrupt(turn(1)).await;
    assert!(b.grant(a).await.is_none(), "交付过的不回来");
}

/// 恢复计划只算一次：提交之后再重建，计划为空。
#[tokio::test]
async fn t_start_returns_abandon_plan_once() {
    let b = Bench::new(Some(Auto)).await;
    b.reviewer.push(ReviewScript::Hangs);
    let h = b.ask(
        &b.decider(),
        turn(1),
        read_req("/home/u/Documents/notes/x.txt"),
    );
    let id = b.approval(0).await;
    h.abort();
    let (fresh, plan) = b.restart(|_, _| Some(keys())).await;
    assert_eq!(plan.len(), 1, "{plan:?}");
    assert!(fresh.pending(id).await.unwrap().awaits_user());
    let (_, again) = fresh.restart(|_, _| Some(keys())).await;
    assert!(again.is_empty());
}

/// 审查开始与审查回合和请求同一笔提交。
#[tokio::test]
async fn t_review_start_is_one_commit() {
    let b = Bench::new(Some(Auto)).await;
    b.reviewer.push(ReviewScript::Hangs);
    let commits = b.store.commits.load(Ordering::SeqCst);
    let h = b.ask(
        &b.decider(),
        turn(1),
        read_req("/home/u/Documents/notes/x.txt"),
    );
    let id = b.approval(0).await;
    let stored = b.store.stored();
    let at = stored
        .iter()
        .position(|e| matches!(&e.event, DomainEvent::ApprovalRequested(r) if r.approval.id == id))
        .unwrap();
    assert!(matches!(
        stored[at - 1].event,
        DomainEvent::ApprovalEvaluated(_)
    ));
    assert!(
        matches!(&stored[at + 1].event, DomainEvent::ApprovalReviewUpdated(r) if matches!(r.review, ReviewState::InProgress { .. }))
    );
    assert!(
        matches!(&stored[at + 2].event, DomainEvent::TurnQueued(q) if q.turn.kind == TurnKind::SystemAiReview)
    );
    // 布景 1 笔（等审批）+ 请求 1 笔；审查回合自己的启动另算
    assert!(b.store.commits.load(Ordering::SeqCst) >= commits + 2);
    b.engine
        .decide(id, UserDecision::Deny, user())
        .await
        .unwrap();
    settle(h).await;
}

/// `commit_with` 返回后同一对话的决定立刻拿得到令牌（令牌不出引擎）。
#[tokio::test]
async fn commit_with_never_hands_out_token() {
    let b = Bench::new(Some(Manual)).await;
    let h = b.ask(&b.decider(), turn(1), bash_req("cat ~/.ssh/id_rsa"));
    let card = card_n(&b, 0).await;
    let committed = b
        .engine
        .commit_with(
            Scope::Card(conversation()),
            command(vec![card_state(card, UserActionState::Dismissed)]),
        )
        .await
        .unwrap();
    assert!(committed.attached.is_empty());
    tokio::time::timeout(
        Duration::from_millis(500),
        b.engine.switch_mode(
            ApprovalModeTarget::Conversation(conversation()),
            Some(Auto),
            user(),
        ),
    )
    .await
    .expect("令牌已放")
    .unwrap();
    assert!(is_deny(&settle(h).await, MSG_USER_NOT_EXECUTED));
}

/// 对话整条删掉时清它的绑定。
#[tokio::test]
async fn conversation_deleted_drops_bindings() {
    let b = Bench::new(Some(Manual)).await;
    let h = b.ask(&b.decider(), turn(1), write_req("src/a.rs"));
    let _ = b.approval(0).await;
    assert_eq!(b.engine.binding_count(), 1);
    b.engine
        .commit_with(
            Scope::ConversationDeleting(conversation()),
            command(vec![
                ConversationDeleting {
                    conversation_id: conversation(),
                }
                .into(),
            ]),
        )
        .await
        .unwrap();
    assert_eq!(b.engine.binding_count(), 0, "作废时随答复删掉");
    b.external(vec![
        ConversationDeleted {
            conversation_id: conversation(),
        }
        .into(),
    ])
    .await;
    settle(h).await;
}

/// 第 9 轮 9/F-1：审查跑完后，收尾那笔（k=1）或结论那笔（k=2）提交碰上一次 Busy。hook 拿到拒绝；
/// 之后第一次 `sync` 把这条「审查中、但没有执行者」的审查放弃（和清孤儿同一条常设义务），转问用户，
/// 不会停在审查中直到重启。对照：不调 `sync` 时它确实还在审查中。
#[tokio::test]
async fn t_review_busy_never_stays_in_progress() {
    for k in [1u64, 2] {
        for pass in [true, false] {
            let b = Bench::new(Some(Auto)).await;
            let gate = notify();
            b.reviewer.push(ReviewScript::SaysAfter(
                format!("{{\"pass\": {pass}, \"reason\": \"r\"}}"),
                gate.clone(),
            ));
            let h = b.ask(
                &b.decider(),
                turn(1),
                read_req("/home/u/Documents/notes/x.txt"),
            );
            let id = b.approval(0).await;
            until("审查开始", || {
                b.engine
                    .pending_approval(id)
                    .is_some_and(|a| matches!(a.review, ReviewState::InProgress { .. }))
            })
            .await;
            settle_tasks().await;
            let commits = b.store.commits.load(Ordering::SeqCst);
            b.store.fail_commit_at.store(commits + k, Ordering::SeqCst);
            gate.notify_one();
            let answer = settle(h).await;
            assert!(
                matches!(&answer, PermissionDecision::Deny { message } if message.contains("存储繁忙")),
                "k={k} pass={pass}: {answer:?}"
            );
            assert!(
                matches!(
                    b.pending(id).await.unwrap().review,
                    ReviewState::InProgress { .. }
                ),
                "对照：没人收敛之前停在审查中"
            );
            b.engine.sync().await.unwrap();
            let a = b.pending(id).await.unwrap();
            assert!(
                matches!(
                    a.review,
                    ReviewState::Abandoned {
                        reason: AbandonReason::ExecutorLost,
                        ..
                    }
                ),
                "k={k} pass={pass}: {:?}",
                a.review
            );
            assert!(a.awaits_user());
            assert!(no_approved(&b, id));
        }
    }
}

/// 审查还在跑（执行者活着）时 `sync` 不碰它。
#[tokio::test]
async fn t_sync_leaves_running_review_alone() {
    let b = Bench::new(Some(Auto)).await;
    b.reviewer.push(ReviewScript::Hangs);
    let h = b.ask(
        &b.decider(),
        turn(1),
        read_req("/home/u/Documents/notes/x.txt"),
    );
    let id = b.approval(0).await;
    until("审查开始", || {
        b.engine
            .pending_approval(id)
            .is_some_and(|a| matches!(a.review, ReviewState::InProgress { .. }))
    })
    .await;
    b.engine.sync().await.unwrap();
    assert!(matches!(
        b.pending(id).await.unwrap().review,
        ReviewState::InProgress { .. }
    ));
    // 请求被丢弃（等待上限）：执行者没了，下一次 sync 放弃它
    h.abort();
    let _ = h.await;
    b.engine.sync().await.unwrap();
    assert!(b.pending(id).await.unwrap().awaits_user());
}
