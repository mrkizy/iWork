//! 状态转换先在副本上计算；只有已提交的事件能更新等待表。
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

pub(super) fn apply_approval_changes(state: &mut ConversationState, events: &[DomainEvent]) {
    for event in events {
        match event {
            DomainEvent::ApprovalReviewUpdated(e) => {
                if let Some(p) = state.pending.get_mut(&e.approval_id) {
                    p.approval.review = e.review.clone();
                }
            }
            DomainEvent::ApprovalDecided(e) => {
                if let Some(p) = state.pending.get_mut(&e.approval_id) {
                    p.approval.state = e.state;
                }
            }
            DomainEvent::ApprovalVoided(e) => {
                if let Some(mut p) = state.pending.remove(&e.approval_id) {
                    state.grants.remove(&e.approval_id);
                    if let Some(tx) = p.reply.take() {
                        let _ = tx.send(deny(MSG_TURN_CANCELLED));
                    }
                }
            }
            DomainEvent::UserActionStateChanged(e) => {
                if let Some(c) = state.cards.get_mut(&e.card_id) {
                    c.card.state = e.state.clone();
                }
                if let Some(message) = card_settled_message(&e.state) {
                    if let Some(mut c) = state.cards.remove(&e.card_id) {
                        if let Some(tx) = c.reply.take() {
                            let _ = tx.send(message);
                        }
                    }
                }
            }
            _ => {}
        }
    }
}

impl LiveRules {
    /// 订阅与本地提交回执共用入口；逐 ID 游标让重投、乱序及撤销后迟到的回执都幂等。
    pub(super) fn apply(&mut self, seq: Seq, event: &DomainEvent) {
        let (ids, rule) = match event {
            DomainEvent::RuleRemembered(e) => (vec![e.rule.id], Some(&e.rule)),
            DomainEvent::RuleRevoked(e) => (vec![e.rule_id], None),
            DomainEvent::RulesDeleted(e) => (e.rule_ids.clone(), None),
            _ => return,
        };
        for id in ids {
            if self.versions.get(&id).is_some_and(|seen| *seen >= seq) {
                continue;
            }
            self.versions.insert(id, seq);
            self.entries.retain(|r| r.id != id);
            if let Some(rule) = rule {
                if is_live_layer(rule.layer) {
                    self.entries.push(rule.clone());
                }
            }
        }
        self.seq = self.seq.max(seq);
    }
}
