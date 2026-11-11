//! 各种决定在新结构下的步骤（T07 方案 6.10.6）。
//!
//! 每条路径都是：拿令牌（含屏障）→ 追平 → 在折叠的视图上算计划（纯函数）→ 提交 → 追平 → 放令牌。
//! 被投影器拒（状态前提不成立）就回到「拿令牌」重算，最多 [`REPLAN_ATTEMPTS`] 次。内存从不在这里改。
use super::sync::{REPLAN_ATTEMPTS, TokenGuard};
use super::waiting::{AskRegistration, register_ask, register_card};
use super::*;
use crate::turn::Decided;
use iwork_domain::approval::owner_alive;

/// 一次请求要用的视图：在 inner 锁里一次取出，出锁后在它上面算（求值会碰文件系统）。
/// 只取要用的几样（方案 6.10.12「取视图」）：拒绝记忆、这个回合的一次性放行、审查上下文。
struct View {
    cursor: Seq,
    live: Arc<Vec<RuleEntry>>,
    mode: ApprovalMode,
    denial: DenialMemory,
    grants: Vec<(ApprovalId, GrantOnce)>,
    context: super::context::ReviewContext,
}

/// 一次请求的身份：哪条会话线、哪个回合、归一后的调用。
#[derive(Clone, Copy)]
struct Request<'a> {
    keys: &'a Arc<SessionKeys>,
    turn: TurnId,
    call: &'a NormalizedCall,
}

/// 一次尝试的结局。
enum Attempt {
    Done(Decided),
    /// 提交被投影器拒：追平后重来
    Replan,
}

fn answer(decision: PermissionDecision, approval: Option<ApprovalId>) -> Decided {
    Decided { decision, approval }
}

fn done(decision: PermissionDecision, approval: Option<ApprovalId>) -> Attempt {
    Attempt::Done(answer(decision, approval))
}

/// 结果是询问或沙箱外时，在这个回合的一次性放行里找同一次调用。命中就记进审计：资格还在则改成放行
/// （折叠看到后把它挪去等交付）；资格不在了结果不改（折叠看到后作废它）。返回命中的那条审批。
fn apply_grant(
    eval: &mut CallEvaluation,
    call: &NormalizedCall,
    turn: TurnId,
    grants: &[(ApprovalId, GrantOnce)],
) -> Option<ApprovalId> {
    if !matches!(
        eval.evaluation.verdict,
        Verdict::Ask(_) | Verdict::OutsideSandbox(_)
    ) {
        return None;
    }
    let record = call.record(eval.representative);
    let id = grants
        .iter()
        .find(|(_, g)| g.matches(turn, &record))
        .map(|(id, _)| *id)?;
    if still_eligible(eval, Waiver::GrantOnce).is_ok() {
        eval.evaluation.verdict = Verdict::Allow;
        eval.evaluation.layer_hit = RuleLayer::Remembered;
        eval.evaluation.matched = Vec::new();
    }
    Some(id)
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
    fn evaluate(
        &self,
        keys: &SessionKeys,
        call: &NormalizedCall,
        live: &[RuleEntry],
        denial: &DenialMemory,
        mode: ApprovalMode,
    ) -> Result<(CallEvaluation, RuleSet), RuleSetError> {
        let rules = build_ruleset(&keys.snapshot.rules, live, &keys.scope())?;
        let ctx = EvalContext {
            mode,
            turn_kind: keys.turn_kind,
            tool_set: &keys.tool_set,
            disabled_groups: &keys.disabled_groups,
            denial,
        };
        Ok((evaluate_call(call, &rules, &ctx, &self.ports.fs), rules))
    }

    /// 一次权限请求（方案 6.10.6 第 1 条）。返回答复与它对应的审批（编排凭它提交交付事件）。
    pub async fn on_permission(
        self: &Arc<Self>,
        keys: &Arc<SessionKeys>,
        turn: TurnId,
        req: &PermissionRequest,
    ) -> Decided {
        let t0 = self.ports.clock.monotonic();
        // 1. 取事实（I/O）在令牌外
        let analysis = analyze(req, &keys.snapshot.boundaries, &self.mcp_read_only);
        let facts = gather_path_facts(&analysis, &keys.cwd, &self.ports.fs);
        let ctx = NormalizeContext {
            paths: &keys.paths,
            boundaries: &keys.snapshot.boundaries,
            mcp_read_only: &self.mcp_read_only,
            cwd: &keys.cwd,
        };
        let call = normalize(&analysis, &ctx, &facts, &self.ports.fs);
        for _ in 0..REPLAN_ATTEMPTS {
            if let Attempt::Done(decided) = self.request_once(keys, turn, &call, t0).await {
                return decided;
            }
        }
        answer(deny(MSG_STORAGE_FAILED), None)
    }

    /// 第 2～8 步走一遍；提交被投影器拒（状态前提不成立）返回 `Replan`，由调用方追平重来。
    async fn request_once(
        self: &Arc<Self>,
        keys: &Arc<SessionKeys>,
        turn: TurnId,
        call: &NormalizedCall,
        t0: iwork_ports::clock::MonotonicInstant,
    ) -> Attempt {
        // 2. 拿令牌，追平，取视图
        let Ok(token) = self.acquire(keys.conversation).await else {
            return done(deny(MSG_STORAGE_FAILED), None);
        };
        if self.catch_up().await.is_err() {
            return done(deny(MSG_STORAGE_FAILED), None);
        }
        // 3. 还能不能受理
        let view = match self.request_view(keys, turn) {
            Ok(view) => view,
            Err(refused) => return Attempt::Done(refused),
        };
        // 4. 求值；要问时在一次性放行里找同一次调用
        let (mut eval, rules) = match self.evaluate(keys, call, &view.live, &view.denial, view.mode)
        {
            Ok(evaluated) => evaluated,
            Err(error) => return done(deny(format!("iWork 审批规则无效：{error}")), None),
        };
        let grant = apply_grant(&mut eval, call, turn, &view.grants);
        let elapsed = self.ports.clock.monotonic().0.saturating_sub(t0.0);
        let audit: DomainEvent = ApprovalEvaluated {
            turn_id: turn,
            action: call.representative_action(eval.representative),
            matched_rule_ids: eval.evaluation.matched.clone(),
            ruleset_version: eval.evaluation.ruleset_version.clone(),
            verdict: eval.evaluation.verdict.clone(),
            layer_hit: eval.evaluation.layer_hit,
            duration_ms: u32::try_from(elapsed / 1_000_000).unwrap_or(u32::MAX),
            snapshot_lists_seq: keys.snapshot.lists_seq,
            live_rules_seq: view.cursor,
            sub_actions: call.actions.clone(),
            call_shape: Some(call.shape.kind()),
            snapshot_rule_paths: keys.snapshot.rule_paths.clone(),
            grant_approval_id: grant,
        }
        .into();
        let actor = format!("agent:{}", keys.assistant);
        // 5. 组计划并提交，一次请求一笔提交
        let mut token = token;
        match eval.evaluation.verdict.clone() {
            Verdict::Allow => {
                // 只含审计的提交不用再追平：一次性放行的去向由同一对话下一个决定开头的追平折到
                match self
                    .commit_under(&mut [&mut token], &actor, vec![audit])
                    .await
                {
                    Ok(_) => done(PermissionDecision::Allow, grant),
                    Err(_) => done(deny(MSG_STORAGE_FAILED), None),
                }
            }
            Verdict::Deny(reason) => {
                if let Err(error) = self
                    .commit_under(&mut [&mut token], &actor, vec![audit])
                    .await
                {
                    tracing::warn!(%error, "拒绝的审计没能落盘");
                }
                done(deny(deny_message(&reason, &rules, call)), None)
            }
            Verdict::UserAction(proposal) => {
                self.propose_card(keys, turn, call, proposal, audit, token)
                    .await
            }
            Verdict::Ask(_) | Verdict::OutsideSandbox(_) => {
                self.ask_user(Request { keys, turn, call }, &eval, view, audit, token)
                    .await
            }
        }
    }

    /// 受理门（第 3 步）：回合不在等审批、对话在删、助手没了，都不建审批、不写审计。
    /// 通过时只取求值、找放行、审查上下文要用的那几样（不整份克隆对话的折叠）。
    fn request_view(&self, keys: &SessionKeys, turn: TurnId) -> Result<View, Decided> {
        let inner = lock_std(&self.inner);
        let f = &inner.folded;
        let Some(conv) = f.conversation(keys.conversation) else {
            return Err(answer(deny(MSG_TURN_CANCELLED), None));
        };
        if !f.is_waiting(turn) || conv.deleting || !f.has_assistant(keys.assistant) {
            return Err(answer(deny(MSG_TURN_CANCELLED), None));
        }
        Ok(View {
            cursor: f.cursor(),
            live: f.live_rules(),
            mode: conv.effective_mode(keys.assistant),
            denial: conv.denial.clone(),
            grants: conv
                .grants
                .iter()
                .filter(|(_, g)| g.turn == turn)
                .map(|(id, g)| (*id, g.clone()))
                .collect(),
            context: conv
                .review_messages
                .contexts
                .get(&keys.project)
                .cloned()
                .unwrap_or_default(),
        })
    }

    /// 亲手执行：先登记卡片等待方，审计与卡片同一笔提交，然后令牌外等卡片的结果。
    async fn propose_card(
        &self,
        keys: &SessionKeys,
        turn: TurnId,
        call: &NormalizedCall,
        proposal: iwork_domain::user_action::Proposal,
        audit: DomainEvent,
        mut token: TokenGuard,
    ) -> Attempt {
        let card = UserActionCard {
            id: UserActionId::from_raw(self.new_id()),
            turn_id: turn,
            conversation_id: keys.conversation,
            proposal_hash: proposal_hash(&proposal),
            proposal,
            reason: call
                .description
                .clone()
                .unwrap_or_else(|| "Agent 未说明".into()),
            state: UserActionState::Pending,
            output_artifact: None,
        };
        let (guard, rx) = register_card(&self.inner, card.id);
        let actor = format!("agent:{}", keys.assistant);
        match self
            .commit_under(
                &mut [&mut token],
                &actor,
                vec![audit, UserActionProposed { card }.into()],
            )
            .await
        {
            Ok(_) => {}
            Err(StorageError::Projection { .. }) => return Attempt::Replan,
            Err(_) => return done(deny(MSG_STORAGE_FAILED), None),
        }
        self.catch_up_after_commit("on_permission").await;
        drop(token);
        let message = rx.await.unwrap_or_else(|_| MSG_TURN_CANCELLED.into());
        drop(guard);
        done(deny(message), None)
    }

    /// 询问 / 沙箱外：先登记等待方与会话绑定，审计、审批（要审查时连同审查开始与审查回合）同一笔提交，
    /// 然后令牌外等答复（第 7 步）。经审批得到的答复（允许和拒绝都算）带这条审批（第 8 步）。
    async fn ask_user(
        self: &Arc<Self>,
        req: Request<'_>,
        eval: &CallEvaluation,
        view: View,
        audit: DomainEvent,
        mut token: TokenGuard,
    ) -> Attempt {
        let Request { keys, turn, call } = req;
        let (offered, label) = offered_remember(call, eval, &keys.scope());
        let approval = Approval {
            id: ApprovalId::from_raw(self.new_id()),
            turn_id: turn,
            conversation_id: keys.conversation,
            kind: if eval.constraints.outside_sandbox {
                ApprovalKind::OutsideSandbox
            } else {
                ApprovalKind::Ask
            },
            action: call.representative_action(eval.representative),
            call: Some(Box::new(call.record(eval.representative))),
            matched_rules: eval.evaluation.matched.clone(),
            ruleset_version: eval.evaluation.ruleset_version.clone(),
            state: ApprovalState::Pending,
            review: ReviewState::NotReviewed,
            offered_remember: offered,
            remember_label: label,
            requested_at: self.ports.clock.now(),
        };
        let id = approval.id;
        let registration = register_ask(&self.inner, id, turn, keys);
        let mut events = vec![
            audit,
            ApprovalRequested {
                approval: approval.clone(),
            }
            .into(),
        ];
        // 要审查时，审查开始与审查回合和请求同一笔提交（不再留下「已请求、没人等」的审批）
        let wants_review = view.mode == ApprovalMode::Auto
            && eval.constraints.review_eligible()
            && keys.ai_review.enabled
            && keys.turn_kind == TurnKind::Assistant;
        let review = if wants_review {
            let review_id = ReviewId(TurnId::from_raw(self.new_id()));
            let Ok(started) = plan_approval(
                &approval,
                ApprovalTrigger::AiReviewStarted { review_id },
                view.mode,
            ) else {
                return done(deny(MSG_STORAGE_FAILED), None);
            };
            events.extend(started);
            let review_turn = self.review_turn(keys, review_id);
            events.push(self.review_queued(review_turn.clone()));
            Some((review_id, review_turn))
        } else {
            None
        };
        let actor = format!("agent:{}", keys.assistant);
        match self.commit_under(&mut [&mut token], &actor, events).await {
            Ok(_) => {}
            Err(StorageError::Projection { .. }) => return Attempt::Replan,
            Err(_) => return done(deny(MSG_STORAGE_FAILED), None),
        }
        self.catch_up_after_commit("on_permission").await;
        drop(token);
        let decision = self
            .await_answer(keys, call, id, review, view.context, registration)
            .await;
        done(decision, Some(id))
    }

    /// 令牌外等答复；有审查时同时跑审查（6.6 ③④），审查出结论走 `settle_review`。
    async fn await_answer(
        &self,
        keys: &Arc<SessionKeys>,
        call: &NormalizedCall,
        id: ApprovalId,
        review: Option<(ReviewId, iwork_domain::turn::Turn)>,
        context: super::context::ReviewContext,
        registration: AskRegistration<'_>,
    ) -> PermissionDecision {
        let mut reply = registration.reply;
        let Some((review_id, review_turn)) = review else {
            return reply.await.unwrap_or_else(|_| deny(MSG_TURN_CANCELLED));
        };
        let input = ReviewInput {
            actions: call
                .actions
                .iter()
                .map(|a| format!("{:?} {:?}", a.kind, a.target))
                .collect(),
            command_text: call.raw_summary.clone(),
            task_goal: context.task_goal,
            recent_context: context.recent_context,
        };
        let execution = self
            .run_review(
                keys,
                runtime::ReviewRequest {
                    turn: review_turn,
                    input,
                    stopped: registration.stopped,
                },
                &mut reply,
            )
            .await;
        let settled = match execution.exit {
            runtime::ReviewExit::Finished(result) if execution.error.is_none() => self
                .settle_review(id, review_id, result)
                .await
                .map(|()| None),
            runtime::ReviewExit::Answer(answer) => Ok(Some(answer)),
            _ => Ok(None),
        };
        self.cleanup_review(execution.session).await;
        // 记不下来的审查留给常设义务放弃（失去执行者，9/F-1）；这里只把拒绝交回运行时
        let decision = if let Some(error) = execution.error {
            deny(format!("iWork 无法记录审查回合：{error}"))
        } else {
            match settled {
                Ok(Some(answer)) => answer,
                Ok(None) => reply.await.unwrap_or_else(|_| deny(MSG_TURN_CANCELLED)),
                Err(error) => deny(format!("iWork 无法记录审查结果：{error}")),
            }
        };
        drop(registration.guard);
        decision
    }

    /// AI 审查结论回来（方案 6.10.6 第 3 条）：在克隆上判 Stale → 通过时复查资格 → 只施加一次。
    /// 作废计划交出期间到来时在令牌上排队，不丢结论。
    pub(super) async fn settle_review(
        &self,
        id: ApprovalId,
        review_id: ReviewId,
        result: ReviewResult,
    ) -> Result<(), DecideError> {
        // 先追平再查索引：提交后的追平失败过时，没追平的折叠里会查不到这条审批，结论就被当成「已处理」丢掉
        self.catch_up().await?;
        let Some(conversation) = lock_std(&self.inner).folded.approval_conversation(id) else {
            return Ok(());
        };
        let raw_trigger = match &result {
            ReviewResult::Passed(reason) => ApprovalTrigger::AiReviewPassed {
                review_id,
                reason: reason.clone(),
            },
            ReviewResult::Failed(reason) => ApprovalTrigger::AiReviewFailed {
                review_id,
                reason: reason.clone(),
            },
            ReviewResult::Errored => ApprovalTrigger::AiReviewErrored { review_id },
            ReviewResult::TimedOut => ApprovalTrigger::AiReviewTimedOut { review_id },
        };
        let mut last = None;
        for _ in 0..REPLAN_ATTEMPTS {
            let mut token = self.acquire(conversation).await?;
            self.catch_up().await?;
            let (approval, mode, owner_alive_now, keys, live, denial) = {
                let inner = lock_std(&self.inner);
                let f = &inner.folded;
                let (Some(p), Some(conv)) = (f.pending_approval(id), f.conversation(conversation))
                else {
                    return Ok(()); // 用户先决定了、作废了
                };
                let assistant = f.turn_owner(p.approval.turn_id).map(|o| o.assistant);
                (
                    p.approval.clone(),
                    assistant.map_or(ApprovalMode::Manual, |a| conv.effective_mode(a)),
                    owner_alive(&f.owner_facts(p.approval.turn_id)).is_ok(),
                    inner.bindings.get(&id).map(|b| b.keys.clone()),
                    f.live_rules(),
                    conv.denial.clone(),
                )
            };
            let mut probe = approval.clone();
            let trigger = if let Err(ApprovalApplyError::Stale { .. }) =
                probe.apply(raw_trigger.clone(), &ApprovalContext { mode_now: mode })
            {
                raw_trigger.clone()
            } else if let ReviewResult::Passed(_) = result {
                let eligibility = if !owner_alive_now {
                    Err(NotEligible::Denied("对话或助手已删除".into()))
                } else {
                    match (&keys, approval.call.as_deref()) {
                        (Some(keys), Some(record)) => {
                            let call =
                                NormalizedCall::from_record(record, &approval.action.raw_summary);
                            match self.evaluate(keys, &call, &live, &denial, mode) {
                                Ok((now, _)) => still_eligible(&now, Waiver::AiReviewPassed),
                                Err(error) => {
                                    Err(NotEligible::Denied(format!("审批规则无效：{error}")))
                                }
                            }
                        }
                        _ => Err(NotEligible::Changed),
                    }
                };
                match eligibility {
                    Ok(()) => raw_trigger.clone(),
                    Err(why) => ApprovalTrigger::AiReviewFailed {
                        review_id,
                        reason: why.reason(),
                    },
                }
            } else {
                raw_trigger.clone()
            };
            let events = plan_approval(&approval, trigger, mode)?;
            match self
                .commit_under(&mut [&mut token], ENGINE_ACTOR, events)
                .await
            {
                Ok(_) => {
                    self.catch_up_after_commit("settle_review").await;
                    return Ok(());
                }
                Err(rejected @ StorageError::Projection { .. }) => last = Some(rejected),
                Err(error) => return Err(error.into()),
            }
        }
        tracing::error!(approval = %id, review = %review_id.0, rejection = ?last, "审查结论重算 3 次仍被拒，审批留在审查中");
        Err(DecideError::Conflict)
    }

    /// 用户对待审批的决定（方案 6.10.6 第 2 条；`DecideApproval` 的核心，T10 调用）。
    pub async fn decide(
        &self,
        id: ApprovalId,
        decision: UserDecision,
        by: Actor,
    ) -> Result<DecideOutcome, DecideError> {
        self.catch_up().await?;
        let conversation = lock_std(&self.inner)
            .folded
            .approval_conversation(id)
            .ok_or(DecideError::NotPending(id))?;
        for _ in 0..REPLAN_ATTEMPTS {
            let mut token = self.acquire(conversation).await?;
            self.catch_up().await?;
            let (approval, mode, owner, keys, live, denial) = {
                let inner = lock_std(&self.inner);
                let f = &inner.folded;
                let (Some(p), Some(conv)) = (f.pending_approval(id), f.conversation(conversation))
                else {
                    return Err(DecideError::NotPending(id));
                };
                let assistant = f.turn_owner(p.approval.turn_id).map(|o| o.assistant);
                (
                    p.approval.clone(),
                    assistant.map_or(ApprovalMode::Manual, |a| conv.effective_mode(a)),
                    owner_alive(&f.owner_facts(p.approval.turn_id)),
                    inner.bindings.get(&id).map(|b| b.keys.clone()),
                    f.live_rules(),
                    conv.denial.clone(),
                )
            };
            let events = match decision {
                UserDecision::Allow { remember } => {
                    if !approval.offered_remember.contains(&remember) {
                        return Err(DecideError::RememberNotOffered { id, remember });
                    }
                    owner.map_err(|_| DecideError::OwnerGone(id))?;
                    let keys = keys.ok_or(DecideError::SessionUnavailable(id))?;
                    if let Some(record) = approval.call.as_deref() {
                        let call =
                            NormalizedCall::from_record(record, &approval.action.raw_summary);
                        let (now, _) = self.evaluate(&keys, &call, &live, &denial, mode)?;
                        if still_eligible(&now, Waiver::User).is_err() {
                            return Err(DecideError::NoLongerAllowed(id));
                        }
                    }
                    let mut events =
                        plan_approval(&approval, ApprovalTrigger::UserAllow { remember }, mode)?;
                    if let Some(rule) = remember_rule(
                        &approval,
                        remember,
                        &keys.scope(),
                        by.clone(),
                        self.ports.clock.now(),
                        RuleId::from_raw(self.new_id()),
                    ) {
                        events.push(RuleRemembered { rule }.into());
                    }
                    events
                }
                UserDecision::Deny => plan_approval(&approval, ApprovalTrigger::UserDeny, mode)?,
            };
            match self
                .commit_under(&mut [&mut token], &by.0, events.clone())
                .await
            {
                Ok(_) => {}
                Err(StorageError::Projection { .. }) => continue,
                Err(error) => return Err(error.into()),
            }
            // 成功后追平：拒绝记忆、一次性放行、对等待方的答复都在折叠应用这条事件时发生
            let resume_turn = self
                .catch_up_after_commit("decide")
                .await
                .then(|| self.resume_turn_for(id, approval.turn_id))
                .flatten();
            return Ok(DecideOutcome {
                events,
                resume_turn,
            });
        }
        Err(DecideError::Conflict)
    }

    /// 追平后这条审批在一次性放行里、且它的回合在 `Interrupted`：要续接的回合。
    fn resume_turn_for(&self, id: ApprovalId, turn: TurnId) -> Option<TurnId> {
        let inner = lock_std(&self.inner);
        (inner.folded.grant(id).is_some() && inner.folded.is_interrupted(turn)).then_some(turn)
    }

    /// 切换档位（方案 6.10.6 第 4 条；`SetApprovalMode` 的核心）。`mode = None` 表示成员恢复跟随。
    /// 返回已提交的事件与要续接的回合。
    pub async fn switch_mode(
        &self,
        target: ApprovalModeTarget,
        mode: Option<ApprovalMode>,
        by: Actor,
    ) -> Result<(Vec<DomainEvent>, Vec<TurnId>), ModeError> {
        let changed: DomainEvent = ApprovalModeChanged {
            target,
            mode,
            cause: ApprovalModeChangeCause::User,
        }
        .into();
        let conversation = match target {
            ApprovalModeTarget::Project(_) => {
                if mode == Some(ApprovalMode::FullAccess) {
                    return Err(ModeError::FullAccessNotAllowedAsDefault);
                }
                // 项目默认只影响之后新建的对话，谁先谁后由 seq 说了算：不拿令牌
                self.commit_raw(&by.0, vec![changed.clone()]).await?;
                self.catch_up_after_commit("switch_mode").await;
                return Ok((vec![changed], Vec::new()));
            }
            ApprovalModeTarget::Conversation(c) => {
                if mode.is_none() {
                    return Err(ModeError::FollowOnlyForMember);
                }
                c
            }
            ApprovalModeTarget::Member {
                conversation_id, ..
            } => conversation_id,
        };
        for _ in 0..REPLAN_ATTEMPTS {
            let mut token = self
                .acquire(conversation)
                .await
                .map_err(DecideError::from)?;
            self.catch_up().await.map_err(DecideError::from)?;
            // 视图：每条待审批带上它的助手、绑定与归属是否还在
            let (conv, live, candidates) = {
                let inner = lock_std(&self.inner);
                let f = &inner.folded;
                let conv = f
                    .conversation(conversation)
                    .ok_or(ModeError::ConversationNotFound(conversation))?
                    .clone();
                let candidates: Vec<_> = conv
                    .pending
                    .values()
                    .filter_map(|p| {
                        let owner = f.turn_owner(p.approval.turn_id)?;
                        if let ApprovalModeTarget::Member { assistant_id, .. } = target
                            && owner.assistant != assistant_id
                        {
                            return None;
                        }
                        if owner_alive(&f.owner_facts(p.approval.turn_id)).is_err() {
                            return None;
                        }
                        let keys = inner.bindings.get(&p.approval.id)?.keys.clone();
                        Some((p.approval.clone(), owner.assistant, keys))
                    })
                    .collect();
                (conv, f.live_rules(), candidates)
            };
            let modes = modes_after(&conv.modes, target, mode).expect("项目默认已在上面返回");
            let mut events = vec![changed.clone()];
            let mut released = Vec::new();
            for (approval, assistant, keys) in candidates {
                let Some(record) = approval.call.as_deref() else {
                    continue;
                };
                let new_mode = modes.effective(assistant);
                let call = NormalizedCall::from_record(record, &approval.action.raw_summary);
                let (now, _) = self
                    .evaluate(&keys, &call, &live, &conv.denial, new_mode)
                    .map_err(DecideError::from)?;
                if still_eligible(&now, Waiver::ModeSwitch(new_mode)).is_ok() {
                    events.extend(
                        plan_approval(&approval, ApprovalTrigger::ModeSwitchAllows, new_mode)
                            .map_err(DecideError::from)?,
                    );
                    released.push((approval.id, approval.turn_id));
                }
            }
            let released_ids: Vec<ApprovalId> = released.iter().map(|(id, _)| *id).collect();
            {
                let inner = lock_std(&self.inner);
                events.extend(
                    changes::plan_reviews_leaving_auto(&inner.folded, &conv, &modes, &released_ids)
                        .map_err(DecideError::from)?,
                );
            }
            match self
                .commit_under(&mut [&mut token], &by.0, events.clone())
                .await
            {
                Ok(_) => {}
                Err(StorageError::Projection { .. }) => continue,
                Err(error) => return Err(DecideError::from(error).into()),
            }
            let resumes = if self.catch_up_after_commit("switch_mode").await {
                released
                    .iter()
                    .filter_map(|(id, turn)| self.resume_turn_for(*id, *turn))
                    .collect::<BTreeSet<_>>()
                    .into_iter()
                    .collect()
            } else {
                Vec::new()
            };
            return Ok((events, resumes));
        }
        Err(DecideError::Conflict.into())
    }

    /// 回合作废计划（方案 6.10.6 第 7 条）：拿令牌、追平、对这个回合名下的待审批与 Pending 卡片算作废事件，
    /// 把令牌包成句柄交出去。不动内存；计划由编排和回合状态变化同一笔提交，之后追平折到。
    pub async fn void_plan(
        &self,
        conversation: ConversationId,
        turn: TurnId,
        reason: VoidReason,
    ) -> Result<(Vec<DomainEvent>, VoidHold), EngineError> {
        let token = self.acquire(conversation).await?;
        self.catch_up().await?;
        let events = {
            let inner = lock_std(&self.inner);
            let f = &inner.folded;
            match f.conversation(conversation) {
                Some(conv) => {
                    let targets = changes::targets_where(conv, reason, |t| t == turn);
                    changes::plan_voids(f, conv, &targets)
                }
                None => Vec::new(),
            }
        };
        Ok((events, Self::hold(token)))
    }
}
