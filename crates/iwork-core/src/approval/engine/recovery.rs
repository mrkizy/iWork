//! 从持久化事件重建审批与卡片；会话线必须按原回合定位。
use super::*;

impl<C, G, S, A, T, R> ApprovalEngine<C, G, S, A, T, R>
where
    C: Clock + 'static,
    G: IdGenerator + 'static,
    S: StoragePort + 'static,
    A: AgentPort + 'static,
    T: Timer + 'static,
    R: PathResolver + 'static,
{
    /// 启动恢复（方案 E1、E4）：按事件重建待审批（全部 detached）、拒绝记忆与活规则；
    /// 进行中的审查失去执行者 → 放弃、转问用户。返回放弃事件，由调用方在恢复事务里提交。
    pub async fn recover(
        &self,
        events: &[StoredEvent],
        keys_for: impl Fn(ConversationId, TurnId) -> Option<SessionKeys>,
    ) -> Result<Vec<DomainEvent>, DecideError> {
        let mut approvals: BTreeMap<ApprovalId, Approval> = BTreeMap::new();
        let mut cards: BTreeMap<UserActionId, UserActionCard> = BTreeMap::new();
        let mut denial: BTreeMap<ConversationId, Vec<DenialEvent>> = BTreeMap::new();
        for stored in events {
            match &stored.event {
                DomainEvent::UserActionProposed(e) => {
                    cards.insert(e.card.id, e.card.clone());
                }
                DomainEvent::UserActionStateChanged(e) => {
                    if let Some(card) = cards.get_mut(&e.card_id) {
                        card.state = e.state.clone();
                        card.output_artifact = e.output_artifact;
                    }
                }
                DomainEvent::ApprovalRequested(e) => {
                    approvals.insert(e.approval.id, e.approval.clone());
                }
                DomainEvent::ApprovalReviewUpdated(e) => {
                    if let Some(a) = approvals.get_mut(&e.approval_id) {
                        a.review = e.review.clone();
                    }
                }
                DomainEvent::ApprovalDecided(e) => {
                    if let Some(a) = approvals.get_mut(&e.approval_id) {
                        a.state = e.state;
                        let ev = match e.state {
                            ApprovalState::Denied => {
                                Some(DenialEvent::UserDenied(a.fingerprints()))
                            }
                            ApprovalState::Approved {
                                decider: Decider::User,
                            } => Some(DenialEvent::UserAllowed(a.fingerprints())),
                            _ => None,
                        };
                        denial.entry(a.conversation_id).or_default().extend(ev);
                    }
                }
                DomainEvent::ApprovalVoided(e) => {
                    if let Some(a) = approvals.get_mut(&e.approval_id) {
                        a.state = ApprovalState::Voided { reason: e.reason };
                    }
                }
                DomainEvent::MessageAppended(e) if e.message.author == Author::User => {
                    self.on_event(stored.seq, &stored.event).await?;
                    denial
                        .entry(e.message.conversation_id)
                        .or_default()
                        .push(DenialEvent::UserMessage);
                }
                other => self.on_event(stored.seq, other).await?,
            }
        }
        for (conv_id, evs) in &denial {
            self.conversation(*conv_id).lock().await.denial = DenialMemory::replay(evs);
        }
        let mut out = Vec::new();
        for (id, approval) in approvals
            .into_iter()
            .filter(|(_, a)| a.state == ApprovalState::Pending)
        {
            let Some(keys) = keys_for(approval.conversation_id, approval.turn_id) else {
                continue;
            };
            let conv = self.conversation(approval.conversation_id);
            let mut state = conv.lock().await;
            let call = approval
                .call
                .as_deref()
                .map(|r| NormalizedCall::from_record(r, &approval.action.raw_summary));
            let review = approval.review.clone();
            lock_std(&self.approvals).insert(id, approval.conversation_id);
            state.pending.insert(
                id,
                PendingAsk {
                    approval,
                    call,
                    eval_at_request: None,
                    keys: Arc::new(keys),
                    reply: None,
                    stop_review: None,
                },
            );
            if let ReviewState::InProgress { review_id } = review {
                let mode = ApprovalMode::Auto;
                let planned = self
                    .plan_change(
                        &state,
                        id,
                        ApprovalTrigger::AiReviewAbandoned { review_id },
                        mode,
                    )
                    .expect("恢复中的 Pending + InProgress 可以放弃审查");
                out.extend(planned);
            }
        }
        for card in cards.into_values() {
            let Some(keys) = keys_for(card.conversation_id, card.turn_id) else {
                continue;
            };
            if card.state.is_terminal() {
                continue;
            }
            lock_std(&self.cards).insert(card.id, card.conversation_id);
            let conv = self.conversation(card.conversation_id);
            conv.lock().await.cards.insert(
                card.id,
                PendingCard {
                    assistant: keys.assistant,
                    card,
                    reply: None,
                },
            );
        }
        Ok(out)
    }
}
