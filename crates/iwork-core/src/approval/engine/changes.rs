//! 状态转换先在副本上计算；只有已提交的事件能更新内存。
//!
//! 已提交事件的应用只有一个入口 [`apply_committed`]（2/F-1、2/F-2）：发起方拿到提交回执后调它，订阅把
//! 同一批事件再投给 `on_event` 时也调它。它是幂等的——等待项还在才动，动完就移除——所以等回执时请求被
//! 取消（事件已落盘、本地没来得及应用）也不会丢：订阅重投时补上。取消等待回执不等于回滚。
use super::*;

pub(super) fn plan_approval(
    current: &Approval,
    trigger: ApprovalTrigger,
    mode_now: ApprovalMode,
) -> Result<Vec<DomainEvent>, ApprovalApplyError> {
    let id = current.id;
    let mut approval = current.clone();
    let remember = match trigger {
        ApprovalTrigger::UserAllow { remember } => Some(remember),
        _ => None,
    };
    match approval.apply(trigger, &ApprovalContext { mode_now }) {
        Ok(applied) => {
            let mut events = Vec::new();
            if applied.review_changed {
                events.push(
                    ApprovalReviewUpdated {
                        approval_id: id,
                        review: approval.review.clone(),
                    }
                    .into(),
                );
            }
            if applied.state_changed {
                events.push(match approval.state {
                    ApprovalState::Voided { reason } => ApprovalVoided {
                        approval_id: id,
                        reason,
                    }
                    .into(),
                    state => ApprovalDecided {
                        approval_id: id,
                        state,
                        remember,
                    }
                    .into(),
                });
            }
            Ok(events)
        }
        Err(ApprovalApplyError::Stale {
            trigger,
            review_id,
            why,
        }) => Ok(vec![
            ApprovalStaleResultDropped {
                approval_id: id,
                review_id,
                result: match trigger {
                    iwork_domain::approval::ApprovalTriggerKind::AiReviewPassed => {
                        iwork_domain::approval::ReviewPhase::Passed
                    }
                    iwork_domain::approval::ApprovalTriggerKind::AiReviewFailed => {
                        iwork_domain::approval::ReviewPhase::Failed
                    }
                    iwork_domain::approval::ApprovalTriggerKind::AiReviewErrored => {
                        iwork_domain::approval::ReviewPhase::Errored
                    }
                    _ => iwork_domain::approval::ReviewPhase::TimedOut,
                },
                why,
            }
            .into(),
        ]),
        Err(error) => Err(error),
    }
}

/// 放行一条待审批：回答通道还在就答复，否则（detached）记一次性放行，返回要续接的回合。
pub(super) fn release(state: &mut ConversationState, id: ApprovalId) -> Option<TurnId> {
    let mut p = state.pending.remove(&id)?;
    let reply = p.reply.take();
    match reply.map(|tx| tx.send(PermissionDecision::Allow)) {
        Some(Ok(())) => None,
        _ => {
            if let Some(record) = p.approval.call.as_deref().cloned() {
                state.grants.insert(
                    id,
                    GrantOnce {
                        approval_id: id,
                        turn: p.approval.turn_id,
                        record,
                    },
                );
            }
            Some(p.approval.turn_id)
        }
    }
}

/// 应用已提交的事件（幂等）。返回因放行而要续接的回合（等待上限中断过的审批）。
pub(super) fn apply_committed(
    state: &mut ConversationState,
    events: &[DomainEvent],
) -> Vec<TurnId> {
    let mut resumes = Vec::new();
    for event in events {
        match event {
            DomainEvent::ApprovalReviewUpdated(e) => {
                if let Some(p) = state.pending.get_mut(&e.approval_id) {
                    p.approval.review = e.review.clone();
                    if !matches!(e.review, ReviewState::InProgress { .. })
                        && let Some(stop) = p.stop_review.take()
                    {
                        let _ = stop.send(());
                    }
                }
            }
            DomainEvent::ApprovalDecided(e) => {
                state.voiding.remove(&e.approval_id);
                let Some(p) = state.pending.get_mut(&e.approval_id) else {
                    continue;
                };
                p.approval.state = e.state;
                let fingerprints = p.approval.fingerprints();
                match e.state {
                    ApprovalState::Denied => {
                        state.denial.apply(&DenialEvent::UserDenied(fingerprints));
                        if let Some(mut p) = state.pending.remove(&e.approval_id)
                            && let Some(tx) = p.reply.take()
                        {
                            let _ = tx.send(deny(MSG_USER_DENIED));
                        }
                    }
                    ApprovalState::Approved { decider } => {
                        if decider == Decider::User {
                            state.denial.apply(&DenialEvent::UserAllowed(fingerprints));
                        }
                        resumes.extend(release(state, e.approval_id));
                    }
                    ApprovalState::Pending | ApprovalState::Voided { .. } => {}
                }
            }
            DomainEvent::ApprovalVoided(e) => {
                state.voiding.remove(&e.approval_id);
                if let Some(mut p) = state.pending.remove(&e.approval_id) {
                    state.grants.remove(&e.approval_id);
                    if let Some(tx) = p.reply.take() {
                        let _ = tx.send(deny(MSG_TURN_CANCELLED));
                    }
                }
            }
            DomainEvent::ApprovalModeChanged(e) => {
                if let Some(modes) = modes_after(&state.modes, e.target, e.mode) {
                    state.modes = modes;
                }
            }
            DomainEvent::TurnFinished(e) => {
                state.grants.retain(|_, grant| grant.turn != e.turn_id);
                state.voiding_turns.remove(&e.turn_id);
            }
            DomainEvent::UserActionStateChanged(e) => {
                let voided = matches!(e.state, UserActionState::Voided { .. });
                if voided {
                    state.voiding_cards.remove(&e.card_id);
                }
                if let Some(c) = state.cards.get_mut(&e.card_id) {
                    // 作废计划交出后卡片若已开始执行，迟到的作废不覆盖它（2/F-1）
                    if voided && c.card.state != UserActionState::Pending {
                        continue;
                    }
                    c.card.state = e.state.clone();
                }
                if let Some(message) = card_settled_message(&e.state)
                    && let Some(mut c) = state.cards.remove(&e.card_id)
                    && let Some(tx) = c.reply.take()
                {
                    let _ = tx.send(message);
                }
            }
            _ => {}
        }
    }
    resumes
}

/// 对话级档位变化后的档位输入（纯函数）；项目默认不在对话里，返回 `None`。
pub(super) fn modes_after(
    current: &ConversationModes,
    target: ApprovalModeTarget,
    mode: Option<ApprovalMode>,
) -> Option<ConversationModes> {
    let mut modes = current.clone();
    match (&mut modes, target) {
        (_, ApprovalModeTarget::Project(_)) => return None,
        (
            ConversationModes::Project {
                conversation_setting,
                ..
            },
            ApprovalModeTarget::Conversation(_),
        ) => *conversation_setting = mode,
        (
            ConversationModes::Project {
                member_overrides, ..
            },
            ApprovalModeTarget::Member { assistant_id, .. },
        ) => {
            match mode {
                Some(m) => member_overrides.insert(assistant_id, m),
                None => member_overrides.remove(&assistant_id),
            };
        }
        (ConversationModes::Dm { own_setting, .. }, _) => *own_setting = mode,
    }
    Some(modes)
}

/// 活规则流里的事件。
pub(super) fn is_rule_event(event: &DomainEvent) -> bool {
    matches!(
        event,
        DomainEvent::RuleRemembered(_) | DomainEvent::RuleRevoked(_) | DomainEvent::RulesDeleted(_)
    )
}

impl LiveRules {
    /// 应用规则流里序号为 `seq` 的事件（2/F-3）。缓存与 `seq` 描述同一段完整有序前缀：只接受比 `seq` 新的事件，
    /// 调用方保证按序号递增、不跳过任何规则事件（订阅按序投递；本地回执先经 [`ApprovalEngine::catch_up_rules`]
    /// 从日志补齐）。重投、迟到的旧事件按序号跳过，所以幂等。
    pub(super) fn apply(&mut self, seq: Seq, event: &DomainEvent) {
        if seq <= self.seq {
            return;
        }
        match event {
            DomainEvent::RuleRemembered(e) => {
                self.entries.retain(|r| r.id != e.rule.id);
                self.entries.push(e.rule.clone());
            }
            DomainEvent::RuleRevoked(e) => self.entries.retain(|r| r.id != e.rule_id),
            DomainEvent::RulesDeleted(e) => self.entries.retain(|r| !e.rule_ids.contains(&r.id)),
            _ => {}
        }
        self.seq = seq;
    }
}

/// 所有档位来源共用：离开 Auto 后，只有本次已经放行的审批可以跳过放弃。
pub(super) fn plan_reviews_leaving_auto(
    state: &ConversationState,
    modes: &ConversationModes,
    released: &[ApprovalId],
) -> Result<Vec<DomainEvent>, ApprovalApplyError> {
    let mut events = Vec::new();
    for (id, pending) in &state.pending {
        let assistant = pending.keys.assistant;
        if state.modes.effective(assistant) == ApprovalMode::Auto
            && modes.effective(assistant) != ApprovalMode::Auto
            && matches!(pending.approval.review, ReviewState::InProgress { .. })
            && !released.contains(id)
        {
            events.extend(plan_approval(
                &pending.approval,
                ApprovalTrigger::ModeChangedFromAuto,
                modes.effective(assistant),
            )?);
        }
    }
    Ok(events)
}
