//! 计划都是纯函数：在折叠的只读视图上算出要提交的事件，不改任何内存（T07 方案 6.10.2）。
//! 内存只由追平时折叠应用已提交的事件推进（[`super::fold::Folded::apply`]）。
use super::fold::{ConversationFold, Folded, VoidTarget};
use super::*;

pub(super) fn plan_approval(
    current: &Approval,
    trigger: ApprovalTrigger,
    mode_now: ApprovalMode,
) -> Result<Vec<DomainEvent>, ApprovalApplyError> {
    let id = current.id;
    let conversation_id = Some(current.conversation_id);
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
                        conversation_id,
                        review: approval.review.clone(),
                    }
                    .into(),
                );
            }
            if applied.state_changed {
                events.push(match approval.state {
                    ApprovalState::Voided { reason } => ApprovalVoided {
                        approval_id: id,
                        conversation_id,
                        reason,
                    }
                    .into(),
                    state => ApprovalDecided {
                        approval_id: id,
                        conversation_id,
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

/// 所有档位来源共用：离开 Auto 后，只有本次已经放行的审批可以跳过放弃。审批属于哪个助手从折叠的回合归属查。
pub(super) fn plan_reviews_leaving_auto(
    folded: &Folded,
    conv: &ConversationFold,
    modes: &ConversationModes,
    released: &[ApprovalId],
) -> Result<Vec<DomainEvent>, ApprovalApplyError> {
    let mut events = Vec::new();
    for (id, pending) in &conv.pending {
        let Some(owner) = folded.turn_owner(pending.approval.turn_id) else {
            continue;
        };
        if conv.modes.effective(owner.assistant) == ApprovalMode::Auto
            && modes.effective(owner.assistant) != ApprovalMode::Auto
            && matches!(pending.approval.review, ReviewState::InProgress { .. })
            && !released.contains(id)
        {
            events.extend(plan_approval(
                &pending.approval,
                ApprovalTrigger::ModeChangedFromAuto,
                modes.effective(owner.assistant),
            )?);
        }
    }
    Ok(events)
}

/// 这个对话里满足 `pick(回合)` 的待审批与 Pending 卡片，作废原因 `reason`。
pub(super) fn targets_where(
    conv: &ConversationFold,
    reason: VoidReason,
    pick: impl Fn(TurnId) -> bool,
) -> Vec<VoidTarget> {
    let approvals = conv
        .pending
        .values()
        .filter(|p| pick(p.approval.turn_id))
        .map(|p| VoidTarget::Approval {
            id: p.approval.id,
            reason,
        });
    let cards = conv
        .cards
        .values()
        .filter(|c| pick(c.turn_id) && c.state == UserActionState::Pending)
        .map(|c| VoidTarget::Card { id: c.id, reason });
    approvals.chain(cards).collect()
}

/// 作废事件。目标都取自同一份视图（待审批、Pending 卡片），所以转换一定合法。
pub(super) fn plan_voids(
    folded: &Folded,
    conv: &ConversationFold,
    targets: &[VoidTarget],
) -> Vec<DomainEvent> {
    let mut events = Vec::new();
    for target in targets {
        match *target {
            VoidTarget::Approval { id, reason } => {
                let Some(p) = conv.pending.get(&id) else {
                    continue;
                };
                let mode = folded
                    .turn_owner(p.approval.turn_id)
                    .map_or(ApprovalMode::Manual, |o| conv.modes.effective(o.assistant));
                events.extend(
                    plan_approval(&p.approval, ApprovalTrigger::Void { reason }, mode)
                        .expect("待审批可以作废"),
                );
            }
            VoidTarget::Card { id, reason } => {
                let Some(card) = conv.cards.get(&id) else {
                    continue;
                };
                let mut card = card.clone();
                card.apply(UserActionTrigger::Void { reason })
                    .expect("Pending 卡片可以作废");
                events.push(
                    UserActionStateChanged {
                        card_id: card.id,
                        conversation_id: Some(card.conversation_id),
                        state: card.state,
                        output_artifact: None,
                    }
                    .into(),
                );
            }
        }
    }
    events
}
