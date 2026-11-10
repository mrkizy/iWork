//! AI 审查的系统回合：同一个截止时间覆盖建连、prompt、更新流和关闭。
use super::*;
use iwork_domain::common::Usage;
use iwork_domain::event::{
    QuotaObserved, TurnFinished, TurnModelReported, TurnQueued, TurnStateChanged, UsageReported,
};
use iwork_domain::turn::{Turn, TurnState, TurnTrigger};

pub(super) struct ReviewRequest {
    pub turn: Turn,
    pub input: ReviewInput,
    pub stopped: oneshot::Receiver<()>,
}

pub(super) enum ReviewExit {
    Finished(ReviewResult),
    Answer(PermissionDecision),
    Abandoned,
}

pub(super) struct ReviewExecution<S> {
    pub exit: ReviewExit,
    pub session: Option<S>,
    pub error: Option<StorageError>,
}

enum Stop {
    Deadline,
    Answer(PermissionDecision),
    Abandoned,
}

impl<C, G, S, A, T, R> ApprovalEngine<C, G, S, A, T, R>
where
    C: Clock + 'static,
    G: IdGenerator + 'static,
    S: StoragePort + 'static,
    A: AgentPort + 'static,
    T: Timer + 'static,
    R: PathResolver + 'static,
{
    pub(super) fn review_turn(&self, keys: &SessionKeys, id: ReviewId) -> Turn {
        Turn {
            id: id.0,
            conversation_id: keys.conversation,
            assistant_id: keys.assistant,
            assistant_version: keys.assistant_version,
            runtime_id: keys.ai_review.runtime_id.unwrap_or(keys.runtime_id),
            model: keys
                .ai_review
                .model
                .clone()
                .unwrap_or_else(|| keys.review_model.clone()),
            runtime_session_id: None,
            kind: TurnKind::SystemAiReview,
            state: TurnState::Queued,
            started_at: self.ports.clock.now(),
            ended_at: None,
        }
    }

    pub(super) fn review_queued(&self, turn: Turn) -> DomainEvent {
        TurnQueued { turn }.into()
    }

    async fn review_started(&self, turn: &mut Turn) -> Result<(), StorageError> {
        let mut next = turn.clone();
        next.apply(TurnTrigger::Started, self.ports.clock.now())
            .expect("Queued 系统回合可以启动");
        self.commit(
            ENGINE_ACTOR,
            vec![
                TurnStateChanged {
                    turn_id: turn.id,
                    from: turn.state,
                    to: next.state,
                    reason: None,
                }
                .into(),
            ],
        )
        .await?;
        *turn = next;
        Ok(())
    }

    pub(super) async fn run_review(
        &self,
        keys: &Arc<SessionKeys>,
        request: ReviewRequest,
        reply: &mut oneshot::Receiver<PermissionDecision>,
    ) -> ReviewExecution<A::Session> {
        let ReviewRequest {
            mut turn,
            input,
            stopped,
        } = request;
        let (system_prompt, user) = review_prompt(&input);
        let spec = SessionSpec {
            cwd: keys.paths.workspace_root.0.clone(),
            model: turn.model.clone(),
            system_prompt,
            mcp: Vec::new(),
            constraints: keys.snapshot.constraints.clone(),
            tool_set: crate::approval::session::tool_set_for(
                TurnKind::SystemAiReview,
                &ToolGroups::new(),
            ),
            resume: None,
            kind: TurnKind::SystemAiReview,
        };
        let mut session = None;
        let mut usage = Usage::default();
        let timeout = Duration::from_secs(u64::from(keys.ai_review.timeout_secs.max(1)));
        let end = {
            let process = async {
                session = match self.ports.reviewer.open_session(spec, DenyAll).await {
                    Ok(opened) => Some(opened),
                    Err(_) => return Ok(ReviewResult::Errored),
                };
                let active = session.as_ref().expect("刚打开的审查会话");
                let mut updates = match active.prompt(vec![ContentBlock::Text(user)]).await {
                    Ok(updates) => updates,
                    Err(_) => return Ok(ReviewResult::Errored),
                };
                self.review_started(&mut turn).await?;
                let mut text = String::new();
                let mut completed = false;
                loop {
                    // 即使运行时始终 Ready，也给截止时间和取消信号一次 poll 机会。
                    let mut yielded = false;
                    poll_fn(|cx| {
                        if yielded {
                            Poll::Ready(())
                        } else {
                            yielded = true;
                            cx.waker().wake_by_ref();
                            Poll::Pending
                        }
                    })
                    .await;
                    let Some(update) = updates.next().await else {
                        break;
                    };
                    match update {
                        AgentUpdate::Text { text: chunk, .. } => text.push_str(&chunk),
                        AgentUpdate::Usage(reported) => usage = reported,
                        AgentUpdate::ModelReported(model) => {
                            self.commit(
                                ENGINE_ACTOR,
                                vec![
                                    TurnModelReported {
                                        turn_id: turn.id,
                                        model,
                                    }
                                    .into(),
                                ],
                            )
                            .await?
                        }
                        AgentUpdate::Quota(windows) => {
                            self.commit(
                                ENGINE_ACTOR,
                                vec![
                                    QuotaObserved {
                                        runtime_id: turn.runtime_id,
                                        windows,
                                        observed_at: self.ports.clock.now(),
                                    }
                                    .into(),
                                ],
                            )
                            .await?
                        }
                        AgentUpdate::Finished(TurnOutcome::Completed) => {
                            completed = true;
                            break;
                        }
                        AgentUpdate::Finished(TurnOutcome::Cancelled) | AgentUpdate::Failed(_) => {
                            return Ok(ReviewResult::Errored);
                        }
                        _ => {}
                    }
                }
                let result = if completed {
                    match parse_review_output(&text) {
                        Ok(ReviewOutcome { pass: true, reason }) => ReviewResult::Passed(reason),
                        Ok(ReviewOutcome {
                            pass: false,
                            reason,
                        }) => ReviewResult::Failed(reason),
                        Err(_) => ReviewResult::Errored,
                    }
                } else {
                    ReviewResult::Errored
                };
                if session
                    .take()
                    .expect("审查会话未被消费")
                    .close()
                    .await
                    .is_err()
                {
                    return Ok(ReviewResult::Errored);
                }
                Ok::<_, StorageError>(result)
            };
            let control = async {
                match race(race(self.ports.timer.sleep(timeout), stopped), reply).await {
                    Ok(Ok(())) => Stop::Deadline,
                    Ok(Err(_)) => Stop::Abandoned,
                    Err(answer) => {
                        Stop::Answer(answer.unwrap_or_else(|_| deny(MSG_TURN_CANCELLED)))
                    }
                }
            };
            // 先登记总截止时间，再 poll 建连，避免 Ready 的阶段推迟计时。
            match race(control, process).await {
                Ok(stop) => Err(stop),
                Err(result) => Ok(result),
            }
        };
        let (exit, mut error) = match end {
            Ok(Ok(result)) => (ReviewExit::Finished(result), None),
            Ok(Err(error)) => (ReviewExit::Finished(ReviewResult::Errored), Some(error)),
            Err(Stop::Deadline) => (ReviewExit::Finished(ReviewResult::TimedOut), None),
            Err(Stop::Answer(answer)) => (ReviewExit::Answer(answer), None),
            Err(Stop::Abandoned) => (ReviewExit::Abandoned, None),
        };
        let triggers: &[TurnTrigger] = match &exit {
            ReviewExit::Finished(ReviewResult::Passed(_) | ReviewResult::Failed(_)) => {
                &[TurnTrigger::Completed]
            }
            ReviewExit::Finished(_) => &[TurnTrigger::Failed],
            _ if turn.state == TurnState::Queued => &[TurnTrigger::Cancel],
            _ => &[TurnTrigger::Cancel, TurnTrigger::ProcessEnded],
        };
        let mut events = Vec::new();
        for trigger in triggers {
            let from = turn.state;
            turn.apply(*trigger, self.ports.clock.now())
                .expect("审查回合只从 Queued 或 Running 收尾");
            events.push(
                TurnStateChanged {
                    turn_id: turn.id,
                    from,
                    to: turn.state,
                    reason: None,
                }
                .into(),
            );
        }
        if usage != Usage::default() {
            events.push(
                UsageReported {
                    turn_id: turn.id,
                    runtime_id: turn.runtime_id,
                    usage,
                }
                .into(),
            );
        }
        events.push(
            TurnFinished {
                turn_id: turn.id,
                state: turn.state,
                usage,
                celebrate: false,
                change_summary: None,
            }
            .into(),
        );
        if let Err(failed) = self.commit(ENGINE_ACTOR, events).await {
            error = Some(failed);
        }
        ReviewExecution {
            exit,
            session,
            error,
        }
    }

    /// 审批已先落到可由用户处理的状态；收尾最多等待三秒，不会挡住状态转换。
    pub(super) async fn cleanup_review(&self, session: Option<A::Session>) {
        if let Some(session) = session {
            let cleanup = async {
                let _ = session.cancel().await;
                let _ = session.close().await;
            };
            let _ = race(cleanup, self.ports.timer.sleep(Duration::from_secs(3))).await;
        }
    }
}
