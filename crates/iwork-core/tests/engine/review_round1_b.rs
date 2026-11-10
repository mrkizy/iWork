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
    assert_eq!(b.mode(conversation(), assistant()).await, Manual);
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
    assert_eq!(b.pending(id).await.unwrap().state, ApprovalState::Pending);
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
    // 把日志里的审批改成 T07 之前的样子（没有调用记录），在这份日志上重启
    let legacy = Bench::new(Some(Manual)).await;
    let mut events = b.store.all();
    for e in &mut events {
        if let DomainEvent::ApprovalRequested(r) = e {
            r.approval.call = None;
        }
    }
    legacy.store.seed(events[4..].to_vec());
    let (recovered, _) = legacy.restart(|_, _| Some(keys())).await;
    recovered.interrupt(turn(101)).await;
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
    // 没有调用记录就发不出一次性放行；续接以「有放行可用」为准（方案 6.10.6 第 2 条），所以也不续接
    assert_eq!(result.resume_turn, None);
    assert!(recovered.grant(id).await.is_none());
    assert!(recovered.engine.turns_to_resume().is_empty());
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
    b.external(vec![RuleRemembered { rule }.into()]).await;
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
    assert!(matches!(stored.event, DomainEvent::RuleRemembered(_)));
    // 别的任务在回执到达之前追平：规则只按日志顺序应用一次
    b.engine.catch_up().await.unwrap();
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
    let plan = d
        .void_pending(turn(104), VoidReason::TurnCancelled)
        .await
        .unwrap()
        .0;
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
    assert_eq!(b.pending(id).await.unwrap().state, ApprovalState::Pending);
    assert!(!h.is_finished());
    let retry = d
        .void_pending(turn(104), VoidReason::TurnCancelled)
        .await
        .unwrap()
        .0;
    assert_eq!(retry, plan);
    b.store
        .commit(CommitRequest {
            command_id: CommandId::from_raw(RawUlid::from_u128(999)),
            actor: user(),
            events: retry.clone(),
            outbox: vec![],
        })
        .await
        .unwrap();
    d.committed(&retry).await;
    assert!(b.pending(id).await.is_none());
    assert!(is_deny(&h.await.unwrap(), MSG_TURN_CANCELLED));
}

async fn first_card(b: &Bench) -> iwork_domain::user_action::UserActionCard {
    tokio::time::timeout(Duration::from_secs(2), async {
        loop {
            if let Some(card) = b.store.all().into_iter().find_map(|e| match e {
                DomainEvent::UserActionProposed(e) => Some(e.card),
                _ => None,
            }) {
                return card;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap()
}

#[tokio::test]
async fn assistant_deletion_voids_card_without_ordinary_approval() {
    let b = Bench::new(Some(Manual)).await;
    let h = b.ask(
        &b.decider(),
        turn(105),
        bash_req(&format!("cat '{DATA}/internal'")),
    );
    let card = first_card(&b).await;
    let plan = b
        .engine
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
        .unwrap()
        .attached;
    assert!(plan.iter().any(|e| matches!(e, DomainEvent::UserActionStateChanged(e) if e.card_id == card.id && matches!(e.state, UserActionState::Voided { reason: VoidReason::AssistantDeleted }))));
    assert!(is_deny(&settle(h).await, MSG_TURN_CANCELLED));
}

#[tokio::test]
async fn active_and_unknown_cards_are_not_voided() {
    for reached in 1..=3 {
        let path = [
            UserActionState::Starting,
            UserActionState::Executing,
            UserActionState::Stopping,
        ];
        let b = Bench::new(Some(Manual)).await;
        let h = b.ask(
            &b.decider(),
            turn(106),
            bash_req(&format!("cat '{DATA}/internal'")),
        );
        let card = first_card(&b).await;
        b.external(
            path[..reached]
                .iter()
                .map(|s| card_state(card.id, s.clone()))
                .collect(),
        )
        .await;
        assert!(
            b.engine
                .void_plan(conversation(), turn(106), VoidReason::TurnCancelled)
                .await
                .unwrap()
                .0
                .is_empty()
        );
        assert!(!h.is_finished());
        h.abort();
    }
}

#[tokio::test]
async fn recovered_cards_keep_owner_and_latest_state() {
    let b = Bench::new(Some(Manual)).await;
    let h = b.ask(
        &b.decider(),
        turn(107),
        bash_req(&format!("cat '{DATA}/internal'")),
    );
    let card = first_card(&b).await;
    h.abort();
    let (fresh, _) = b.restart(|_, _| Some(keys())).await;
    let plan = fresh
        .engine
        .void_plan(conversation(), turn(107), VoidReason::TurnCancelled)
        .await
        .unwrap()
        .0;
    assert!(plan.iter().any(|e| matches!(e, DomainEvent::UserActionStateChanged(e) if e.card_id == card.id && matches!(e.state, UserActionState::Voided { .. }))));
    b.store.seed(vec![
        card_state(card.id, UserActionState::Starting),
        card_state(card.id, UserActionState::Executing),
    ]);
    let (active, _) = b.restart(|_, _| Some(keys())).await;
    assert!(
        active
            .engine
            .void_plan(conversation(), turn(107), VoidReason::TurnCancelled)
            .await
            .unwrap()
            .0
            .is_empty()
    );
}

#[tokio::test]
async fn recovery_resolves_each_original_turn_line() {
    use iwork_domain::rule::RuleScope;
    let b = Bench::new(Some(Manual)).await;
    let first = keys();
    let second_assistant = other_assistant();
    let mut second = keys_with(&[], second_assistant);
    second.project = Some(other_project());
    let d1 = Arc::new(b.engine.decider_for(first.clone()));
    let d2 = Arc::new(b.engine.decider_for(second.clone()));
    let h1 = b.ask(&d1, turn(108), bash_req("npm test"));
    let a = b.approval(0).await;
    let h2 = b.ask(&d2, turn(109), bash_req("npm install"));
    let c = b.approval(1).await;
    h1.abort();
    h2.abort();
    let lookups = std::sync::Mutex::new(Vec::new());
    let (fresh, _) = b
        .restart(|conversation_id, original_turn| {
            assert_eq!(conversation_id, conversation());
            lookups.lock().unwrap().push(original_turn);
            Some(if original_turn == turn(108) {
                first.clone()
            } else {
                second.clone()
            })
        })
        .await;
    for id in [a, c] {
        fresh
            .engine
            .decide(id, allow(RememberScope::Forever), user())
            .await
            .unwrap();
    }
    let scopes: Vec<_> = fresh
        .store
        .all()
        .into_iter()
        .filter_map(|e| match e {
            DomainEvent::RuleRemembered(e) => Some(e.rule.scope),
            _ => None,
        })
        .collect();
    assert_eq!(
        scopes,
        vec![
            RuleScope::Assistant(assistant()),
            RuleScope::Assistant(second_assistant)
        ]
    );
    assert_eq!(*lookups.lock().unwrap(), vec![turn(108), turn(109)]);
}

#[tokio::test]
async fn review_deadline_covers_open_cancel_and_close() {
    for script in [
        ReviewScript::OpenHangs,
        ReviewScript::CancelHangs,
        ReviewScript::CloseHangs("{\"pass\":true,\"reason\":\"需要\"}".into()),
    ] {
        let b = Bench::new(Some(Auto)).await;
        let phase = match script {
            ReviewScript::OpenHangs => "open",
            ReviewScript::CancelHangs => "prompt",
            _ => "close",
        };
        b.reviewer.push(script);
        let h = b.ask(
            &b.decider(),
            turn(110),
            read_req("/home/u/Documents/notes/x.txt"),
        );
        let id = b.approval(0).await;
        tokio::time::timeout(Duration::from_secs(1), async {
            loop {
                if b.reviewer.phases.lock().unwrap().contains(&phase) {
                    break;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        b.timer.expire();
        tokio::time::timeout(Duration::from_secs(1), async {
            loop {
                if let Some(a) = b.pending(id).await
                    && matches!(a.review, ReviewState::TimedOut { .. })
                {
                    assert!(a.awaits_user());
                    break;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("审查全生命周期到期后必须转问用户");
        b.engine
            .decide(id, UserDecision::Deny, user())
            .await
            .unwrap();
        settle(h).await;
    }
}

#[tokio::test]
async fn leaving_auto_abandons_review_blocked_by_denial_memory() {
    let b = Bench::new(Some(Auto)).await;
    b.reviewer.push(ReviewScript::Hangs);
    b.reviewer.push(ReviewScript::Hangs);
    let h1 = b.ask(
        &b.decider(),
        turn(111),
        read_req("/home/u/Documents/notes/x.txt"),
    );
    let a = b.approval(0).await;
    let h2 = b.ask(
        &b.decider(),
        turn(112),
        read_req("/home/u/Documents/notes/x.txt"),
    );
    let c = b.approval(1).await;
    b.engine
        .decide(a, UserDecision::Deny, user())
        .await
        .unwrap();
    settle(h1).await;
    b.engine
        .switch_mode(
            ApprovalModeTarget::Conversation(conversation()),
            Some(FullAccess),
            user(),
        )
        .await
        .unwrap();
    let pending = b.pending(c).await.unwrap();
    assert!(pending.awaits_user(), "{pending:?}");
    assert!(matches!(pending.review, ReviewState::Abandoned { .. }));
    b.engine
        .decide(c, UserDecision::Deny, user())
        .await
        .unwrap();
    settle(h2).await;
}

#[tokio::test]
async fn review_context_is_bounded_isolated_and_replayed() {
    use iwork_domain::event::MessageCompleted;
    use iwork_domain::id::CommandId;
    use iwork_ports::agent::ContentBlock;
    use iwork_ports::store::{CommitRequest, StoragePort};
    let b = Bench::new(Some(Auto)).await;
    let mut events = Vec::new();
    for (n, author, project_id, text) in [
        (1, Author::User, project(), "整理项目甲".to_owned()),
        (
            2,
            Author::Assistant {
                id: assistant(),
                name_snapshot: "a".into(),
            },
            project(),
            format!("不应保留{}甲的必要说明", "甲".repeat(2100)),
        ),
        (3, Author::User, other_project(), "项目乙目标".to_owned()),
        (
            4,
            Author::Assistant {
                id: assistant(),
                name_snapshot: "a".into(),
            },
            other_project(),
            "乙的私密上下文".to_owned(),
        ),
        (5, Author::User, project(), "继续".to_owned()),
    ] {
        let message_id = MessageId::from_raw(RawUlid::from_u128(0x700 + n));
        let is_assistant = matches!(author, Author::Assistant { .. });
        events.push(
            MessageAppended {
                message: Message {
                    id: message_id,
                    conversation_id: conversation(),
                    author,
                    context_project_id: Some(project_id),
                    status: if is_assistant {
                        MessageStatus::Streaming
                    } else {
                        MessageStatus::Sent
                    },
                    body: MessageBody::Inline(if is_assistant {
                        String::new()
                    } else {
                        text.clone()
                    }),
                    attachments: vec![],
                    created_at: Timestamp(1),
                    turn_id: None,
                },
            }
            .into(),
        );
        if is_assistant {
            events.push(
                MessageCompleted {
                    conversation_id: conversation(),
                    message_id,
                    body: MessageBody::Inline(text),
                    status: MessageStatus::Completed,
                }
                .into(),
            );
        }
    }
    b.store
        .commit(CommitRequest {
            command_id: CommandId::from_raw(RawUlid::from_u128(0x777)),
            actor: user(),
            events: events.clone(),
            outbox: vec![],
        })
        .await
        .unwrap();
    b.engine.sync().await.unwrap();
    let (fresh, _) = b.restart(|_, _| Some(keys())).await;
    for bench in [&b, &fresh] {
        bench.reviewer.push(ReviewScript::Says(
            "{\"pass\":false,\"reason\":\"需用户确认\"}".into(),
        ));
        let h = bench.ask(
            &bench.decider(),
            turn(113),
            read_req("/home/u/Documents/notes/x.txt"),
        );
        let id = bench.approval(0).await;
        let text = tokio::time::timeout(Duration::from_secs(1), async {
            loop {
                if let Some(input) = bench.reviewer.inputs.lock().unwrap().first()
                    && let ContentBlock::Text(text) = &input[0]
                {
                    break text.clone();
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        assert!(text.contains("任务目标：\n继续"), "{text}");
        assert!(text.contains("甲的必要说明"));
        assert!(!text.contains("不应保留"));
        assert!(!text.contains("乙的私密上下文"));
        let context = text
            .split("最近的上下文：\n")
            .nth(1)
            .unwrap()
            .split("\n\n请求的命令")
            .next()
            .unwrap();
        assert_eq!(context.chars().count(), 2000);
        bench
            .engine
            .decide(id, UserDecision::Deny, user())
            .await
            .unwrap();
        settle(h).await;
    }
}

#[tokio::test]
async fn system_review_records_turn_usage_quota_model_and_reason() {
    use iwork_domain::common::Usage;
    use iwork_domain::turn::TurnState;
    use iwork_ports::agent::{AgentUpdate, TurnOutcome};
    let b = Bench::new(Some(Auto)).await;
    let usage = Usage {
        input_tokens: 123,
        output_tokens: 45,
        ..Usage::default()
    };
    let model = ModelSelection {
        model_id: ModelId("actual-review-model".into()),
        effort: None,
    };
    b.reviewer.push(ReviewScript::Updates(vec![
        AgentUpdate::Usage(usage),
        AgentUpdate::Quota(vec![]),
        AgentUpdate::ModelReported(model.clone()),
        AgentUpdate::Text {
            offset: 0,
            text: "{\"pass\":true,\"reason\":\"读笔记是完成任务的必要步骤\"}".into(),
        },
        AgentUpdate::Finished(TurnOutcome::Completed),
    ]));
    assert_eq!(
        b.call(
            &b.decider(),
            turn(114),
            read_req("/home/u/Documents/notes/x.txt")
        )
        .await,
        PermissionDecision::Allow
    );
    let events = b.store.all();
    let review_turn = events
        .iter()
        .find_map(|e| match e {
            DomainEvent::TurnQueued(e) if e.turn.kind == TurnKind::SystemAiReview => {
                Some(e.turn.clone())
            }
            _ => None,
        })
        .expect("审查 ID 必须对应真实系统回合");
    assert_eq!(review_turn.assistant_id, assistant());
    assert!(events.iter().any(|e| matches!(e, DomainEvent::UsageReported(e) if e.turn_id == review_turn.id && e.runtime_id == review_turn.runtime_id && e.usage == usage)));
    assert!(events.iter().any(
        |e| matches!(e, DomainEvent::QuotaObserved(e) if e.runtime_id == review_turn.runtime_id)
    ));
    assert!(events.iter().any(|e| matches!(e, DomainEvent::TurnModelReported(e) if e.turn_id == review_turn.id && e.model == model)));
    assert!(events.iter().any(|e| matches!(e, DomainEvent::TurnFinished(e) if e.turn_id == review_turn.id && e.state == TurnState::Completed && e.usage == usage)));
    assert!(events.iter().any(|e| matches!(e, DomainEvent::ApprovalReviewUpdated(e) if matches!(&e.review, ReviewState::Passed { reason, .. } if reason == "读笔记是完成任务的必要步骤"))));
}

#[tokio::test]
async fn invalid_ruleset_fails_closed_with_rule_id() {
    use iwork_domain::event::RuleRemembered;
    use iwork_domain::rule::{RuleLayer, RuleMatcher, RuleScope};
    let b = Bench::new(Some(Auto)).await;
    let bad = live_rule(
        0x334,
        RuleLayer::Default,
        RuleMatcher::ToolName("Write".into()),
        RuleScope::Global,
    );
    let rule_id = bad.id.to_string();
    b.external(vec![RuleRemembered { rule: bad }.into()]).await;
    let result = b.call(&b.decider(), turn(115), write_req("src/a.rs")).await;
    assert!(matches!(result, PermissionDecision::Deny { message } if message.contains(&rule_id)));
}

#[tokio::test]
async fn failed_project_default_commit_keeps_old_default() {
    use iwork_domain::event::DmProjectSwitched;
    let b = Bench::new(Some(Manual)).await;
    b.store.fail_next.store(true, Ordering::SeqCst);
    assert!(
        b.engine
            .switch_mode(ApprovalModeTarget::Project(project()), Some(Auto), user())
            .await
            .is_err()
    );
    b.external(vec![
        DmProjectSwitched {
            conversation_id: conversation(),
            project_id: Some(project()),
        }
        .into(),
    ])
    .await;
    assert_eq!(b.mode(conversation(), assistant()).await, Manual);
}

#[tokio::test]
async fn cancelled_mode_commit_does_not_publish_uncommitted_mode() {
    let b = Bench::new(Some(Manual)).await;
    let gate = Arc::new(tokio::sync::Notify::new());
    *b.store.before_commit.lock().unwrap() = Some(gate.clone());
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
    tokio::time::timeout(Duration::from_secs(1), async {
        while !b.store.commit_entered.load(Ordering::SeqCst) {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    switch.abort();
    let _ = switch.await;
    // 已入队、还没处理：档位不变，日志里也没有
    assert_eq!(b.mode(conversation(), assistant()).await, Manual);
    assert_eq!(
        b.count(|e| matches!(e, DomainEvent::ApprovalModeChanged(_))),
        0
    );
    // 调用方没了，已入队的那笔照样处理；档位只随日志变
    gate.notify_one();
    tokio::time::timeout(Duration::from_secs(1), async {
        while b.store.queued() > 0 {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    assert_eq!(
        b.count(|e| matches!(e, DomainEvent::ApprovalModeChanged(_))),
        1
    );
    assert_eq!(b.mode(conversation(), assistant()).await, FullAccess);
}

#[tokio::test]
async fn artifact_messages_are_skipped_without_polluting_review_context() {
    use iwork_domain::common::{ArtifactArea, ArtifactRef, Sha256};
    use iwork_domain::event::MessageCompleted;
    use iwork_domain::id::CommandId;
    use iwork_ports::agent::ContentBlock;
    use iwork_ports::store::{CommitRequest, StoragePort};
    let b = Bench::new(Some(Auto)).await;
    let artifact = MessageBody::Artifact(ArtifactRef {
        sha256: Sha256([9; 32]),
        area: ArtifactArea::Artifacts,
        size: 10000,
    });
    let mut events = Vec::new();
    for (n, author, body) in [
        (1, Author::User, MessageBody::Inline("任务目标甲".into())),
        (
            2,
            Author::Assistant {
                id: assistant(),
                name_snapshot: "a".into(),
            },
            MessageBody::Inline("已知助手上下文甲".into()),
        ),
        (3, Author::User, artifact.clone()),
        (
            4,
            Author::Assistant {
                id: assistant(),
                name_snapshot: "a".into(),
            },
            artifact,
        ),
    ] {
        let id = MessageId::from_raw(RawUlid::from_u128(0x800 + n));
        let is_assistant = matches!(author, Author::Assistant { .. });
        events.push(
            MessageAppended {
                message: Message {
                    id,
                    conversation_id: conversation(),
                    author,
                    context_project_id: Some(project()),
                    status: if is_assistant {
                        MessageStatus::Streaming
                    } else {
                        MessageStatus::Sent
                    },
                    body: if is_assistant {
                        MessageBody::Inline(String::new())
                    } else {
                        body.clone()
                    },
                    attachments: vec![],
                    created_at: Timestamp(1),
                    turn_id: None,
                },
            }
            .into(),
        );
        if is_assistant {
            events.push(
                MessageCompleted {
                    conversation_id: conversation(),
                    message_id: id,
                    body,
                    status: MessageStatus::Completed,
                }
                .into(),
            );
        }
    }
    b.store
        .commit(CommitRequest {
            command_id: CommandId::from_raw(RawUlid::from_u128(0x888)),
            actor: user(),
            events,
            outbox: vec![],
        })
        .await
        .unwrap();
    b.engine.sync().await.unwrap();
    let (fresh, _) = b.restart(|_, _| Some(keys())).await;
    for bench in [&b, &fresh] {
        bench.reviewer.push(ReviewScript::Says(
            "{\"pass\":false,\"reason\":\"需确认\"}".into(),
        ));
        let h = bench.ask(
            &bench.decider(),
            turn(116),
            read_req("/home/u/Documents/notes/x.txt"),
        );
        let id = bench.approval(0).await;
        let input = tokio::time::timeout(Duration::from_secs(1), async {
            loop {
                if let Some(input) = bench.reviewer.inputs.lock().unwrap().first()
                    && let ContentBlock::Text(text) = &input[0]
                {
                    break text.clone();
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        assert!(input.contains("任务目标：\n任务目标甲"));
        assert!(input.contains("最近的上下文：\n已知助手上下文甲"));
        assert!(!input.contains("09090909"));
        bench
            .engine
            .decide(id, UserDecision::Deny, user())
            .await
            .unwrap();
        settle(h).await;
    }
}

#[tokio::test]
async fn dm_project_switch_out_of_auto_abandons_review_immediately() {
    use iwork_domain::event::DmProjectSwitched;
    let b = Bench::new(Some(Auto)).await;
    b.reviewer.push(ReviewScript::Hangs);
    let h = b.ask(
        &b.decider(),
        turn(117),
        read_req("/home/u/Documents/notes/x.txt"),
    );
    let id = b.approval(0).await;
    b.store.fail_next.store(true, Ordering::SeqCst);
    let switched = || {
        command(vec![
            DmProjectSwitched {
                conversation_id: conversation(),
                project_id: None,
            }
            .into(),
        ])
    };
    let scope = Scope::DmProjectSwitched {
        conversation: conversation(),
        project: None,
    };
    assert!(b.engine.commit_with(scope, switched()).await.is_err());
    assert_eq!(b.mode(conversation(), assistant()).await, Auto);
    assert!(matches!(
        b.pending(id).await.unwrap().review,
        ReviewState::InProgress { .. }
    ));
    b.engine.commit_with(scope, switched()).await.unwrap();
    let pending = b.pending(id).await.unwrap();
    assert!(
        pending.awaits_user(),
        "切回纯私聊已离开 Auto，不能留下 InProgress"
    );
    assert!(matches!(pending.review, ReviewState::Abandoned { .. }));
    b.engine
        .decide(id, UserDecision::Deny, user())
        .await
        .unwrap();
    settle(h).await;
}
