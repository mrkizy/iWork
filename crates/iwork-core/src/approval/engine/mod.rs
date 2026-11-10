//! 审批引擎的有状态部分（T07 方案第 6 节；ADR-T07-c）。
//!
//! 与回合编排的接缝（T05 已合入，supervisor 2026-11-10 确认）：引擎为每条会话给出一个
//! [`SessionDecider`]，实现 `turn::PermissionDecider`。回合状态（`PermissionAsked` /
//! `PermissionDecided`）只由编排写；引擎只提交审批流与审计流事件。回合级作废走
//! `void_pending`，返回的事件由编排与回合状态同一次提交；对话、助手删除走 [`ApprovalEngine::void_for_event`]。
//!
//! 每个对话一把决策锁（[`lock::AsyncLock`]）：求值、拒绝记忆的更新、一切放行答复与它们的提交都在锁下串行。
//! 非用户本人的放行路径（AI 审查通过、切档放行、一次性放行）答复前都用当前状态重判资格（[`still_eligible`]）。

mod changes;
pub mod lock;
pub mod review;

use std::collections::{BTreeMap, BTreeSet};
use std::future::{Future, poll_fn};
use std::pin::pin;
use std::sync::{Arc, Mutex};
use std::task::Poll;
use std::time::Duration;

use futures_channel::oneshot;
use iwork_domain::action::{CallShapeKind, CanonicalPath};
use iwork_domain::approval::{
    Approval, ApprovalApplyError, ApprovalCallRecord, ApprovalContext, ApprovalKind, ApprovalState,
    ApprovalTrigger, Decider, ReviewState, VoidReason,
};
use iwork_domain::approval_mode::{
    ApprovalMode, ModeInputs, ProjectDefaultMode, RememberScope, effective_mode,
};
use iwork_domain::common::{Actor, ModelSelection, ToolGroups};
use iwork_domain::event::{
    ApprovalDecided, ApprovalEvaluated, ApprovalModeChangeCause, ApprovalModeChanged,
    ApprovalModeTarget, ApprovalRequested, ApprovalReviewUpdated, ApprovalStaleResultDropped,
    ApprovalVoided, DomainEvent, RuleRemembered, UserActionProposed, UserActionStateChanged,
};
use iwork_domain::id::{
    ApprovalId, AssistantId, CommandId, ConversationId, ProjectId, ReviewId, RuleId, Seq, TurnId,
    UserActionId,
};
use iwork_domain::message::{Author, MessageBody};
use iwork_domain::project::AiReviewSettings;
use iwork_domain::rule::{DenyReason, RuleEntry, RuleLayer, RuleSet, Verdict};
use iwork_domain::session::SessionToolSet;
use iwork_domain::turn::TurnKind;
use iwork_domain::user_action::{UserActionCard, UserActionState, UserActionTrigger};
use iwork_ports::agent::{
    AgentPort, AgentSession, AgentUpdate, ContentBlock, PermissionDecision, PermissionHandler,
    PermissionRequest, SessionSpec, TurnOutcome, UpdateStream,
};
use iwork_ports::clock::{Clock, IdGenerator, Timer};
use iwork_ports::fs::PathResolver;
use iwork_ports::store::{CommitRequest, StorageError, StoragePort, StoredEvent};

use crate::approval::action::{
    CallShape, NormalizeContext, NormalizedCall, analyze, gather_path_facts, normalize,
};
use crate::approval::denial::{DenialEvent, DenialMemory};
use crate::approval::env::{EnvPaths, ScopeKeys};
use crate::approval::evaluate::proposal::proposal_hash;
use crate::approval::evaluate::remember::{offered_remember, remember_rule};
use crate::approval::evaluate::{CallEvaluation, EvalContext, evaluate_call};
use crate::approval::rules::{RuleSetError, build_ruleset, is_live_layer};
use crate::approval::session::SessionSnapshot;
use crate::turn::PermissionDecider;
use changes::{apply_approval_changes, plan_approval};
use lock::AsyncLock;
use review::{ReviewInput, ReviewOutcome, parse_review_output, review_prompt};

/// 回给运行时的拒绝说明（方案 5.5）。
pub const MSG_USER_DENIED: &str = "用户已拒绝，不要换方式重试";
pub const MSG_TOOL_DISABLED: &str = "该工具已在助手配置中禁用";
pub const MSG_NOT_IN_TOOL_SET: &str = "该工具不在本会话可用工具内";
pub const MSG_TURN_CANCELLED: &str = "回合已取消";
pub const MSG_USER_NOT_EXECUTED: &str = "用户未在 iWork 中执行";
pub const MSG_STORAGE_FAILED: &str = "iWork 无法记录审批，已拒绝";
pub const MSG_WRITE_TOOL_HINT: &str = "请改用 Edit 或一条命令（如 cat > 路径 <<'EOF'）表达";
/// 「失去执行者」放弃审查（Q9）的引擎侧 actor。
pub const ENGINE_ACTOR: &str = "system:approval";

/// 引擎用到的端口。
pub struct EnginePorts<C, G, S, A, T, R> {
    pub clock: C,
    pub ids: G,
    pub store: S,
    /// AI 审查回合的运行时
    pub reviewer: A,
    pub timer: T,
    pub fs: R,
}

/// 一条会话的身份与会话快照（T06 实际注入 CLI 的那份）。
#[derive(Debug, Clone)]
pub struct SessionKeys {
    pub conversation: ConversationId,
    pub assistant: AssistantId,
    pub project: Option<ProjectId>,
    pub turn_kind: TurnKind,
    pub snapshot: Arc<SessionSnapshot>,
    pub tool_set: SessionToolSet,
    pub disabled_groups: ToolGroups,
    pub cwd: CanonicalPath,
    pub paths: EnvPaths,
    pub ai_review: AiReviewSettings,
    /// AI 审查用的模型：项目设置没指定时与 iWork 相同
    pub review_model: ModelSelection,
}

impl SessionKeys {
    pub fn scope(&self) -> ScopeKeys {
        ScopeKeys {
            project: self.project,
            assistant: self.assistant,
            conversation: self.conversation,
        }
    }
}

/// 一个对话的档位输入（方案 6.5）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ConversationModes {
    Project {
        project_default: ProjectDefaultMode,
        conversation_setting: Option<ApprovalMode>,
        member_overrides: BTreeMap<AssistantId, ApprovalMode>,
    },
    Dm {
        own_setting: Option<ApprovalMode>,
        selected_project_default: Option<ProjectDefaultMode>,
    },
}

impl Default for ConversationModes {
    /// 不知道的对话按私聊、未选项目：手动（最严）
    fn default() -> Self {
        Self::Dm {
            own_setting: None,
            selected_project_default: None,
        }
    }
}

impl ConversationModes {
    pub fn inputs(&self, assistant: AssistantId) -> ModeInputs {
        match self {
            Self::Project {
                project_default,
                conversation_setting,
                member_overrides,
            } => ModeInputs::ProjectConversation {
                member_override: member_overrides.get(&assistant).copied(),
                conversation_setting: *conversation_setting,
                project_default: *project_default,
            },
            Self::Dm {
                own_setting,
                selected_project_default,
            } => ModeInputs::AssistantDm {
                own_setting: *own_setting,
                selected_project_default: *selected_project_default,
            },
        }
    }

    pub fn effective(&self, assistant: AssistantId) -> ApprovalMode {
        effective_mode(&self.inputs(assistant))
    }
}

/// 一条等待中的询问。
struct PendingAsk {
    approval: Approval,
    /// 重判资格用；没有调用记录的旧审批为 `None`
    call: Option<NormalizedCall>,
    eval_at_request: Option<CallEvaluation>,
    keys: Arc<SessionKeys>,
    /// 回答通道；`None` 或接收端已丢弃 = 等待上限中断过（detached）或崩溃恢复
    reply: Option<oneshot::Sender<PermissionDecision>>,
}

/// 等待上限中断后批准的一次性放行（方案 6.2）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GrantOnce {
    pub approval_id: ApprovalId,
    pub turn: TurnId,
    pub record: ApprovalCallRecord,
}

impl GrantOnce {
    /// 同一次调用：同一回合；工具、cwd、形状、沙箱位置、清单外重试相等；两个摘要都相等。
    pub fn matches(&self, turn: TurnId, record: &ApprovalCallRecord) -> bool {
        let r = &self.record;
        self.turn == turn
            && r.tool == record.tool
            && r.cwd == record.cwd
            && r.shape == record.shape
            && r.placement == record.placement
            && r.bypass_sandbox_requested == record.bypass_sandbox_requested
            && r.raw_input_digest == record.raw_input_digest
            && r.normalized_digest == record.normalized_digest
    }
}

struct PendingCard {
    card: UserActionCard,
    reply: Option<oneshot::Sender<String>>,
}

/// 一个对话的全部易变状态，在一把锁下。
#[derive(Default)]
struct ConversationState {
    modes: ConversationModes,
    denial: DenialMemory,
    pending: BTreeMap<ApprovalId, PendingAsk>,
    grants: BTreeMap<ApprovalId, GrantOnce>,
    cards: BTreeMap<UserActionId, PendingCard>,
    last_user_message: String,
    rebuild_session_needed: bool,
}

#[derive(Debug, Default, Clone)]
struct LiveRules {
    entries: Vec<RuleEntry>,
    seq: Seq,
    versions: BTreeMap<RuleId, Seq>,
}

/// 用户的决定。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UserDecision {
    Allow { remember: RememberScope },
    Deny,
}

/// `decide` 的结果：已提交的事件，以及要续接的回合（等待上限中断过、用户允许时）。
#[derive(Debug, Clone, PartialEq)]
pub struct DecideOutcome {
    pub events: Vec<DomainEvent>,
    pub resume_turn: Option<TurnId>,
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum DecideError {
    #[error("找不到待审批 {0}")]
    NotFound(ApprovalId),
    #[error("审批 {0} 已不在待审批")]
    NotPending(ApprovalId),
    #[error("审批 {id} 没有提供记住范围 {remember:?}")]
    RememberNotOffered {
        id: ApprovalId,
        remember: RememberScope,
    },
    #[error("审批 {0} 的动作已被规则拒绝，不能放行")]
    NoLongerAllowed(ApprovalId),
    #[error(transparent)]
    Storage(#[from] StorageError),
    #[error(transparent)]
    Apply(#[from] ApprovalApplyError),
    #[error(transparent)]
    Rules(#[from] RuleSetError),
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ModeError {
    #[error("项目默认档位不能设为全开")]
    FullAccessNotAllowedAsDefault,
    #[error("只有成员目标可以恢复跟随主管")]
    FollowOnlyForMember,
    #[error(transparent)]
    Storage(#[from] StorageError),
    #[error(transparent)]
    Decision(#[from] DecideError),
}

/// 非用户放行路径为什么不合格（方案 6.1）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum NotEligible {
    Denied(String),
    ForcedAsk,
    Changed,
    NotAllAllowed,
}

impl NotEligible {
    fn reason(&self) -> String {
        match self {
            Self::Denied(r) => r.clone(),
            Self::ForcedAsk => "用户已拒绝过同一动作，需重新确认".into(),
            Self::Changed => "规则已变化".into(),
            Self::NotAllAllowed => "新档位下仍需询问".into(),
        }
    }
}

/// 谁在放行。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Waiver {
    User,
    AiReviewPassed,
    ModeSwitch(ApprovalMode),
    GrantOnce,
}

/// 用当前状态重判一条调用的放行资格（方案 6.1 表）。纯函数。
pub fn still_eligible(
    now: &CallEvaluation,
    at_request: Option<&CallEvaluation>,
    waiver: Waiver,
) -> Result<(), NotEligible> {
    if let Some(d) = now.per_action.iter().find_map(|e| match &e.verdict {
        Verdict::Deny(r) => Some(r.clone()),
        _ => None,
    }) {
        return Err(NotEligible::Denied(match d {
            DenyReason::DenialMemory(_) => "同一动作已被用户拒绝".into(),
            DenyReason::DenyRule(_) => "已被规则拒绝".into(),
            _ => "规则已变化".into(),
        }));
    }
    if waiver == Waiver::User {
        return Ok(());
    }
    if now.constraints.forced_ask {
        return Err(NotEligible::ForcedAsk);
    }
    let was = at_request.map(|e| e.constraints).unwrap_or_default();
    if (now.constraints.must_ask && !was.must_ask)
        || (now.constraints.unparsable && !was.unparsable)
    {
        return Err(NotEligible::Changed);
    }
    if let Waiver::ModeSwitch(_) = waiver
        && !now.per_action.iter().all(|e| e.verdict == Verdict::Allow)
    {
        return Err(NotEligible::NotAllAllowed);
    }
    Ok(())
}

/// 审批引擎。
pub struct ApprovalEngine<C, G, S, A, T, R> {
    ports: EnginePorts<C, G, S, A, T, R>,
    live: Mutex<LiveRules>,
    conversations: Mutex<BTreeMap<ConversationId, Arc<AsyncLock<ConversationState>>>>,
    approvals: Mutex<BTreeMap<ApprovalId, ConversationId>>,
    cards: Mutex<BTreeMap<UserActionId, ConversationId>>,
    project_defaults: Mutex<BTreeMap<ProjectId, ProjectDefaultMode>>,
    mcp_read_only: BTreeSet<String>,
}

fn lock_std<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|p| p.into_inner())
}

fn deny(message: impl Into<String>) -> PermissionDecision {
    PermissionDecision::Deny {
        message: message.into(),
    }
}

/// 按判定结果给运行时的拒绝说明（方案 5.5）。
fn deny_message(reason: &DenyReason, rules: &RuleSet, call: &NormalizedCall) -> String {
    let label = |id: &RuleId| {
        rules
            .entries
            .iter()
            .find(|e| e.id == *id)
            .map_or_else(|| id.to_string(), |e| e.label.clone())
    };
    match reason {
        DenyReason::DenialMemory(_) => MSG_USER_DENIED.into(),
        DenyReason::Baseline(id) => {
            let hint = if call.tool.0 == "Write" || call.tool.0 == "NotebookEdit" {
                format!("；{MSG_WRITE_TOOL_HINT}")
            } else {
                String::new()
            };
            format!(
                "iWork 系统底线：{}；需要改动请让用户亲手执行{hint}",
                label(id)
            )
        }
        DenyReason::ToolDisabled => MSG_TOOL_DISABLED.into(),
        DenyReason::ToolSetExcluded | DenyReason::SystemTurnOutOfToolSet => {
            MSG_NOT_IN_TOOL_SET.into()
        }
        DenyReason::DenyRule(id) => format!("被规则拒绝：{}", label(id)),
    }
}

/// 卡片到终态后回给 Agent 的话（方案 5.5）：只有是否执行与退出码，不含输出。
pub fn card_settled_message(state: &UserActionState) -> Option<String> {
    match state {
        UserActionState::Done { exit } => Some(format!("用户已执行，退出码 {}", exit_code(exit))),
        UserActionState::Failed { error } => Some(format!("用户执行失败：{error}")),
        UserActionState::Unknown { .. } => Some("用户已执行，结果未知".into()),
        UserActionState::Dismissed => Some(MSG_USER_NOT_EXECUTED.into()),
        UserActionState::Voided { .. } => Some(MSG_TURN_CANCELLED.into()),
        UserActionState::Ended => Some("用户已执行，结果未知".into()),
        UserActionState::Pending
        | UserActionState::Starting
        | UserActionState::Executing
        | UserActionState::Stopping => None,
    }
}

fn exit_code(exit: &iwork_domain::user_action::ExitStatus) -> String {
    match exit {
        iwork_domain::user_action::ExitStatus::Code(c) => c.to_string(),
        iwork_domain::user_action::ExitStatus::Signal(s) => format!("信号 {s}"),
    }
}

/// 两个 future 谁先完成用谁；`a` 优先。
async fn race<A: Future, B: Future>(a: A, b: B) -> Result<A::Output, B::Output> {
    let mut a = pin!(a);
    let mut b = pin!(b);
    poll_fn(|cx| {
        if let Poll::Ready(x) = a.as_mut().poll(cx) {
            return Poll::Ready(Ok(x));
        }
        if let Poll::Ready(y) = b.as_mut().poll(cx) {
            return Poll::Ready(Err(y));
        }
        Poll::Pending
    })
    .await
}

/// AI 审查的结果。
#[derive(Debug, Clone, PartialEq, Eq)]
enum ReviewResult {
    Passed(String),
    Failed(String),
    Errored,
    TimedOut,
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
    pub fn new(ports: EnginePorts<C, G, S, A, T, R>, mcp_read_only: BTreeSet<String>) -> Self {
        Self {
            ports,
            live: Mutex::default(),
            conversations: Mutex::default(),
            approvals: Mutex::default(),
            cards: Mutex::default(),
            project_defaults: Mutex::default(),
            mcp_read_only,
        }
    }

    /// 一条会话的判定方，交给 `SessionLine::new`。
    pub fn decider_for(self: &Arc<Self>, keys: SessionKeys) -> SessionDecider<C, G, S, A, T, R> {
        SessionDecider {
            engine: self.clone(),
            keys: Arc::new(keys),
        }
    }

    fn conversation(&self, id: ConversationId) -> Arc<AsyncLock<ConversationState>> {
        lock_std(&self.conversations)
            .entry(id)
            .or_insert_with(|| Arc::new(AsyncLock::new(ConversationState::default())))
            .clone()
    }

    /// 设置一个对话的档位输入（对话打开或恢复时由 daemon 给出）。
    pub async fn set_modes(&self, conversation: ConversationId, modes: ConversationModes) {
        self.conversation(conversation).lock().await.modes = modes;
    }

    pub async fn effective_mode(
        &self,
        conversation: ConversationId,
        assistant: AssistantId,
    ) -> ApprovalMode {
        self.conversation(conversation)
            .lock()
            .await
            .modes
            .effective(assistant)
    }

    /// 清单或登记的项目、助手变化后，进行中的会话要重建才能用上新快照（方案 6.9）。
    pub async fn rebuild_session_needed(&self, conversation: ConversationId) -> bool {
        self.conversation(conversation)
            .lock()
            .await
            .rebuild_session_needed
    }

    /// 会话重建后清掉标记。
    pub async fn session_rebuilt(&self, conversation: ConversationId) {
        self.conversation(conversation)
            .lock()
            .await
            .rebuild_session_needed = false;
    }

    /// 活规则与它的游标（审计用）。
    fn live_rules(&self) -> LiveRules {
        lock_std(&self.live).clone()
    }

    fn new_id(&self) -> iwork_domain::id::RawUlid {
        self.ports.ids.next()
    }

    async fn commit(&self, actor: &str, events: Vec<DomainEvent>) -> Result<(), StorageError> {
        self.commit_seq(actor, events).await.map(|_| ())
    }

    /// 提交并返回最后一条事件的序号（空提交返回 `None`）。
    async fn commit_seq(
        &self,
        actor: &str,
        events: Vec<DomainEvent>,
    ) -> Result<Option<Seq>, StorageError> {
        if events.is_empty() {
            return Ok(None);
        }
        self.ports
            .store
            .commit(CommitRequest {
                command_id: CommandId::from_raw(self.new_id()),
                actor: Actor(actor.into()),
                events,
                outbox: Vec::new(),
            })
            .await
            .map(|r| Some(r.last_seq))
    }

    fn evaluate_now(
        &self,
        keys: &SessionKeys,
        call: &NormalizedCall,
        state: &ConversationState,
        mode: ApprovalMode,
    ) -> Result<(CallEvaluation, RuleSet, Seq), RuleSetError> {
        let live = self.live_rules();
        let rules = build_ruleset(&keys.snapshot.rules, &live.entries, &keys.scope())?;
        let ctx = EvalContext {
            mode,
            turn_kind: keys.turn_kind,
            tool_set: &keys.tool_set,
            disabled_groups: &keys.disabled_groups,
            denial: &state.denial,
        };
        Ok((
            evaluate_call(call, &rules, &ctx, &self.ports.fs),
            rules,
            live.seq,
        ))
    }

    /// 一次权限请求（方案 6.1 的顺序）。
    pub async fn on_permission(
        self: &Arc<Self>,
        keys: &Arc<SessionKeys>,
        turn: TurnId,
        req: &PermissionRequest,
    ) -> PermissionDecision {
        let t0 = self.ports.clock.monotonic();
        // 取事实（I/O）在锁外
        let analysis = analyze(req, &keys.snapshot.boundaries, &self.mcp_read_only);
        let facts = gather_path_facts(&analysis, &keys.cwd, &self.ports.fs);
        let ctx = NormalizeContext {
            paths: &keys.paths,
            boundaries: &keys.snapshot.boundaries,
            mcp_read_only: &self.mcp_read_only,
            cwd: &keys.cwd,
        };
        let call = normalize(&analysis, &ctx, &facts, &self.ports.fs);

        let conv = self.conversation(keys.conversation);
        let mut state = conv.lock().await;
        let mode = state.modes.effective(keys.assistant);
        let (mut eval, rules, live_seq) = match self.evaluate_now(keys, &call, &state, mode) {
            Ok(evaluated) => evaluated,
            Err(error) => return deny(format!("iWork 审批规则无效：{error}")),
        };

        // 一次性放行：只有「这次本来要问」才查，命中即消费，仍过资格复查
        if matches!(
            eval.evaluation.verdict,
            Verdict::Ask(_) | Verdict::OutsideSandbox(_)
        ) {
            let record = call.record(eval.representative);
            let hit = state
                .grants
                .iter()
                .find(|(_, g)| g.matches(turn, &record))
                .map(|(id, _)| *id);
            if let Some(id) = hit {
                state.grants.remove(&id);
                // 绑定的是同一次调用，必问 / 解析不了不算「新出现」；只看拒绝与强制询问
                if still_eligible(&eval, Some(&eval), Waiver::GrantOnce).is_ok() {
                    eval.evaluation.verdict = Verdict::Allow;
                    eval.evaluation.layer_hit = RuleLayer::Remembered;
                    eval.evaluation.matched = Vec::new();
                }
            }
        }

        let elapsed = self.ports.clock.monotonic().0.saturating_sub(t0.0);
        let audit: DomainEvent = ApprovalEvaluated {
            turn_id: turn,
            action: call.actions[eval.representative].clone(),
            matched_rule_ids: eval.evaluation.matched.clone(),
            ruleset_version: eval.evaluation.ruleset_version.clone(),
            verdict: eval.evaluation.verdict.clone(),
            layer_hit: eval.evaluation.layer_hit,
            duration_ms: u32::try_from(elapsed / 1_000_000).unwrap_or(u32::MAX),
            snapshot_lists_seq: keys.snapshot.lists_seq,
            live_rules_seq: live_seq,
            sub_actions: call.actions.clone(),
            call_shape: Some(call.shape.kind()),
        }
        .into();
        let actor = format!("agent:{}", keys.assistant);

        match eval.evaluation.verdict.clone() {
            Verdict::Allow => match self.commit(&actor, vec![audit]).await {
                Ok(()) => PermissionDecision::Allow,
                Err(_) => deny(MSG_STORAGE_FAILED),
            },
            Verdict::Deny(reason) => {
                let _ = self.commit(&actor, vec![audit]).await;
                deny(deny_message(&reason, &rules, &call))
            }
            Verdict::UserAction(proposal) => {
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
                let (tx, rx) = oneshot::channel();
                if self
                    .commit(
                        &actor,
                        vec![audit, UserActionProposed { card: card.clone() }.into()],
                    )
                    .await
                    .is_err()
                {
                    return deny(MSG_STORAGE_FAILED);
                }
                lock_std(&self.cards).insert(card.id, keys.conversation);
                state.cards.insert(
                    card.id,
                    PendingCard {
                        card,
                        reply: Some(tx),
                    },
                );
                drop(state);
                rx.await.map_or_else(|_| deny(MSG_TURN_CANCELLED), deny)
            }
            Verdict::Ask(_) | Verdict::OutsideSandbox(_) => {
                let (offered, label) = offered_remember(&call, &eval, &keys.scope());
                let approval = Approval {
                    id: ApprovalId::from_raw(self.new_id()),
                    turn_id: turn,
                    conversation_id: keys.conversation,
                    kind: if eval.constraints.outside_sandbox {
                        ApprovalKind::OutsideSandbox
                    } else {
                        ApprovalKind::Ask
                    },
                    action: call.actions[eval.representative].clone(),
                    call: Some(Box::new(call.record(eval.representative))),
                    matched_rules: eval.evaluation.matched.clone(),
                    ruleset_version: eval.evaluation.ruleset_version.clone(),
                    state: ApprovalState::Pending,
                    review: ReviewState::NotReviewed,
                    offered_remember: offered,
                    remember_label: label,
                    requested_at: self.ports.clock.now(),
                };
                let review = mode == ApprovalMode::Auto
                    && eval.constraints.review_eligible()
                    && keys.ai_review.enabled
                    && keys.turn_kind == TurnKind::Assistant;
                let (tx, rx) = oneshot::channel();
                let id = approval.id;
                if self
                    .commit(
                        &actor,
                        vec![
                            audit,
                            ApprovalRequested {
                                approval: approval.clone(),
                            }
                            .into(),
                        ],
                    )
                    .await
                    .is_err()
                {
                    return deny(MSG_STORAGE_FAILED);
                }
                lock_std(&self.approvals).insert(id, keys.conversation);
                let task_goal = state.last_user_message.clone();
                state.pending.insert(
                    id,
                    PendingAsk {
                        approval,
                        call: Some(call.clone()),
                        eval_at_request: Some(eval),
                        keys: keys.clone(),
                        reply: Some(tx),
                    },
                );
                if review {
                    let review_id = ReviewId(TurnId::from_raw(self.new_id()));
                    let started = self.plan_change(
                        &state,
                        id,
                        ApprovalTrigger::AiReviewStarted { review_id },
                        mode,
                    );
                    let Ok(started) = started else {
                        return deny(MSG_STORAGE_FAILED);
                    };
                    if self.commit(ENGINE_ACTOR, started.clone()).await.is_err() {
                        return deny(MSG_STORAGE_FAILED);
                    }
                    apply_approval_changes(&mut state, &started);
                    drop(state);
                    let input = ReviewInput {
                        actions: call
                            .actions
                            .iter()
                            .map(|a| format!("{:?} {:?}", a.kind, a.target))
                            .collect(),
                        command_text: call.raw_summary.clone(),
                        task_goal,
                        recent_context: String::new(),
                    };
                    // 审查与用户的答复并行：用户先决定（或作废、切档放行）时审查作废，结果不再施加
                    let review = async {
                        let result = self.run_review(keys, input).await;
                        if let Err(error) = self.settle_review(keys, id, review_id, result).await {
                            return deny(format!("iWork 无法记录审查结果：{error}"));
                        }
                        std::future::pending::<PermissionDecision>().await
                    };
                    return match race(rx, review).await {
                        Ok(answer) => answer.unwrap_or_else(|_| deny(MSG_TURN_CANCELLED)),
                        Err(answer) => answer,
                    };
                }
                drop(state);
                rx.await.unwrap_or_else(|_| deny(MSG_TURN_CANCELLED))
            }
        }
    }

    fn plan_change(
        &self,
        state: &ConversationState,
        id: ApprovalId,
        trigger: ApprovalTrigger,
        mode_now: ApprovalMode,
    ) -> Result<Vec<DomainEvent>, DecideError> {
        let p = state.pending.get(&id).ok_or(DecideError::NotPending(id))?;
        Ok(plan_approval(&p.approval, trigger, mode_now)?)
    }

    /// 放行一条待审批：回答通道还在就答复，否则（detached）记一次性放行，返回要续接的回合。
    fn release(state: &mut ConversationState, id: ApprovalId) -> Option<TurnId> {
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

    async fn run_review(&self, keys: &Arc<SessionKeys>, input: ReviewInput) -> ReviewResult {
        let (system_prompt, user) = review_prompt(&input);
        let spec = SessionSpec {
            cwd: keys.paths.workspace_root.0.clone(),
            model: keys
                .ai_review
                .model
                .clone()
                .unwrap_or_else(|| keys.review_model.clone()),
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
        let session = match self.ports.reviewer.open_session(spec, DenyAll).await {
            Ok(s) => s,
            Err(_) => return ReviewResult::Errored,
        };
        let timeout = Duration::from_secs(u64::from(keys.ai_review.timeout_secs.max(1)));
        let run = async {
            let mut updates = match session.prompt(vec![ContentBlock::Text(user)]).await {
                Ok(u) => u,
                Err(_) => return ReviewResult::Errored,
            };
            let mut text = String::new();
            while let Some(u) = updates.next().await {
                match u {
                    AgentUpdate::Text { text: t, .. } => text.push_str(&t),
                    AgentUpdate::Finished(TurnOutcome::Completed) => break,
                    AgentUpdate::Finished(TurnOutcome::Cancelled) | AgentUpdate::Failed(_) => {
                        return ReviewResult::Errored;
                    }
                    _ => {}
                }
            }
            match parse_review_output(&text) {
                Ok(ReviewOutcome { pass: true, reason }) => ReviewResult::Passed(reason),
                Ok(ReviewOutcome {
                    pass: false,
                    reason,
                }) => ReviewResult::Failed(reason),
                Err(_) => ReviewResult::Errored,
            }
        };
        let result = match race(run, self.ports.timer.sleep(timeout)).await {
            Ok(r) => r,
            Err(()) => {
                let _ = session.cancel().await;
                ReviewResult::TimedOut
            }
        };
        let _ = session.close().await;
        result
    }

    /// 审查结果回来：锁下先在克隆上判 Stale → 通过时复查资格 → 只施加一次（方案 6.6 ⑤）。
    async fn settle_review(
        &self,
        keys: &Arc<SessionKeys>,
        id: ApprovalId,
        review_id: ReviewId,
        result: ReviewResult,
    ) -> Result<(), DecideError> {
        let conv = self.conversation(keys.conversation);
        let mut state = conv.lock().await;
        let mode = state.modes.effective(keys.assistant);
        let Some(p) = state.pending.get(&id) else {
            return Ok(());
        };
        let raw_trigger = match &result {
            ReviewResult::Passed(_) => ApprovalTrigger::AiReviewPassed { review_id },
            ReviewResult::Failed(reason) => ApprovalTrigger::AiReviewFailed {
                review_id,
                reason: reason.clone(),
            },
            ReviewResult::Errored => ApprovalTrigger::AiReviewErrored { review_id },
            ReviewResult::TimedOut => ApprovalTrigger::AiReviewTimedOut { review_id },
        };
        let mut probe = p.approval.clone();
        if let Err(ApprovalApplyError::Stale { .. }) =
            probe.apply(raw_trigger.clone(), &ApprovalContext { mode_now: mode })
        {
            let events = self.plan_change(&state, id, raw_trigger, mode)?;
            self.commit(ENGINE_ACTOR, events).await?;
            return Ok(());
        }
        let trigger = if let ReviewResult::Passed(_) = result {
            let (call, at) = (p.call.clone(), p.eval_at_request.clone());
            let eligibility = match &call {
                Some(call) => match self.evaluate_now(keys, call, &state, mode) {
                    Ok((now, _, _)) => still_eligible(&now, at.as_ref(), Waiver::AiReviewPassed),
                    Err(error) => Err(NotEligible::Denied(format!("审批规则无效：{error}"))),
                },
                None => Err(NotEligible::Changed),
            };
            match eligibility {
                Ok(()) => raw_trigger,
                Err(why) => ApprovalTrigger::AiReviewFailed {
                    review_id,
                    reason: why.reason(),
                },
            }
        } else {
            raw_trigger
        };
        let passed = matches!(trigger, ApprovalTrigger::AiReviewPassed { .. });
        let events = self.plan_change(&state, id, trigger, mode)?;
        self.commit(ENGINE_ACTOR, events.clone()).await?;
        apply_approval_changes(&mut state, &events);
        if passed {
            Self::release(&mut state, id);
        }
        Ok(())
    }

    /// 用户对待审批的决定（`DecideApproval` 的核心，T10 调用）。
    pub async fn decide(
        &self,
        id: ApprovalId,
        decision: UserDecision,
        by: Actor,
    ) -> Result<DecideOutcome, DecideError> {
        let conv_id = lock_std(&self.approvals)
            .get(&id)
            .copied()
            .ok_or(DecideError::NotFound(id))?;
        let conv = self.conversation(conv_id);
        let mut state = conv.lock().await;
        let p = state.pending.get(&id).ok_or(DecideError::NotPending(id))?;
        let keys = p.keys.clone();
        let mode = state.modes.effective(keys.assistant);
        let fingerprints = p.approval.fingerprints();
        let mut events = Vec::new();
        match decision {
            UserDecision::Allow { remember } => {
                if !p.approval.offered_remember.contains(&remember) {
                    return Err(DecideError::RememberNotOffered { id, remember });
                }
                if let Some(call) = p.call.clone() {
                    let (now, _, _) = self.evaluate_now(&keys, &call, &state, mode)?;
                    if still_eligible(&now, None, Waiver::User).is_err() {
                        return Err(DecideError::NoLongerAllowed(id));
                    }
                }
                let approval = p.approval.clone();
                events.extend(self.plan_change(
                    &state,
                    id,
                    ApprovalTrigger::UserAllow { remember },
                    mode,
                )?);
                let rule = remember_rule(
                    &approval,
                    remember,
                    &keys.scope(),
                    by.clone(),
                    self.ports.clock.now(),
                    RuleId::from_raw(self.new_id()),
                );
                if let Some(rule) = &rule {
                    events.push(RuleRemembered { rule: rule.clone() }.into());
                }
                let seq = self.commit_seq(&by.0, events.clone()).await?;
                apply_approval_changes(&mut state, &events);
                if rule.is_some() {
                    if let Some(seq) = seq {
                        let mut live = lock_std(&self.live);
                        for event in &events {
                            live.apply(seq, event);
                        }
                    }
                }
                state.denial.apply(&DenialEvent::UserAllowed(fingerprints));
                let resume_turn = Self::release(&mut state, id);
                Ok(DecideOutcome {
                    events,
                    resume_turn,
                })
            }
            UserDecision::Deny => {
                events.extend(self.plan_change(&state, id, ApprovalTrigger::UserDeny, mode)?);
                self.commit(&by.0, events.clone()).await?;
                apply_approval_changes(&mut state, &events);
                state.denial.apply(&DenialEvent::UserDenied(fingerprints));
                if let Some(mut p) = state.pending.remove(&id)
                    && let Some(tx) = p.reply.take()
                {
                    let _ = tx.send(deny(MSG_USER_DENIED));
                }
                Ok(DecideOutcome {
                    events,
                    resume_turn: None,
                })
            }
        }
    }

    /// 切换档位（`SetApprovalMode` 的核心，T10 调用；方案 6.5）。`mode = None` 表示成员恢复跟随主管。
    /// 返回已提交的事件与要续接的回合。
    pub async fn switch_mode(
        &self,
        target: ApprovalModeTarget,
        mode: Option<ApprovalMode>,
        by: Actor,
    ) -> Result<(Vec<DomainEvent>, Vec<TurnId>), ModeError> {
        let conv_id = match target {
            ApprovalModeTarget::Project(p) => {
                let m = match mode {
                    Some(ApprovalMode::FullAccess) => {
                        return Err(ModeError::FullAccessNotAllowedAsDefault);
                    }
                    Some(ApprovalMode::Auto) => ProjectDefaultMode::Auto,
                    _ => ProjectDefaultMode::Manual,
                };

                let events: Vec<DomainEvent> = vec![
                    ApprovalModeChanged {
                        target,
                        mode,
                        cause: ApprovalModeChangeCause::User,
                    }
                    .into(),
                ];
                self.commit(&by.0, events.clone()).await?;
                lock_std(&self.project_defaults).insert(p, m);
                // 项目默认只对新对话生效
                return Ok((events, Vec::new()));
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
        let conv = self.conversation(conv_id);
        let mut state = conv.lock().await;
        let mut modes = state.modes.clone();
        match (&mut modes, target) {
            (
                ConversationModes::Project {
                    conversation_setting,
                    ..
                },
                ApprovalModeTarget::Conversation(_),
            ) => {
                *conversation_setting = mode;
            }
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
            (_, ApprovalModeTarget::Project(_)) => unreachable!("上面已返回"),
        }
        let mut events: Vec<DomainEvent> = vec![
            ApprovalModeChanged {
                target,
                mode,
                cause: ApprovalModeChangeCause::User,
            }
            .into(),
        ];
        let mut released = Vec::new();
        let ids: Vec<ApprovalId> = state.pending.keys().copied().collect();
        for id in ids {
            let (keys, call, at, review) = {
                let p = &state.pending[&id];
                (
                    p.keys.clone(),
                    p.call.clone(),
                    p.eval_at_request.clone(),
                    p.approval.review.clone(),
                )
            };
            let new_mode = modes.effective(keys.assistant);
            if let ApprovalModeTarget::Member { assistant_id, .. } = target
                && keys.assistant != assistant_id
            {
                continue;
            }
            if let Some(call) = &call {
                let (now, _, _) = self
                    .evaluate_now(&keys, call, &state, new_mode)
                    .map_err(DecideError::from)?;
                if still_eligible(&now, at.as_ref(), Waiver::ModeSwitch(new_mode)).is_ok() {
                    events.extend(self.plan_change(
                        &state,
                        id,
                        ApprovalTrigger::ModeSwitchAllows,
                        new_mode,
                    )?);
                    released.push(id);
                    continue;
                }
            }
            if new_mode == ApprovalMode::Manual && matches!(review, ReviewState::InProgress { .. })
            {
                events.extend(self.plan_change(
                    &state,
                    id,
                    ApprovalTrigger::ModeChangedToManual,
                    new_mode,
                )?);
            }
        }
        self.commit(&by.0, events.clone()).await?;
        state.modes = modes;
        apply_approval_changes(&mut state, &events);
        let resumes = released
            .into_iter()
            .filter_map(|id| Self::release(&mut state, id))
            .collect();
        Ok((events, resumes))
    }

    /// 回合结束等待（取消、失败、中断后放弃）：作废它挂着的审批与卡片，返回事件由编排同一次提交。
    pub async fn void_turn(
        &self,
        conversation: ConversationId,
        turn: TurnId,
        reason: VoidReason,
    ) -> Vec<DomainEvent> {
        let conv = self.conversation(conversation);
        let state = conv.lock().await;
        self.void_where(&state, reason, |t| t == turn)
    }

    fn void_where(
        &self,
        state: &ConversationState,
        reason: VoidReason,
        pick: impl Fn(TurnId) -> bool,
    ) -> Vec<DomainEvent> {
        let mut events = Vec::new();
        for p in state.pending.values().filter(|p| pick(p.approval.turn_id)) {
            events.extend(
                plan_approval(
                    &p.approval,
                    ApprovalTrigger::Void { reason },
                    state.modes.effective(p.keys.assistant),
                )
                .expect("等待表只保留 Pending 审批，Void 转换必定合法"),
            );
        }
        for c in state
            .cards
            .values()
            .filter(|c| pick(c.card.turn_id) && c.card.state == UserActionState::Pending)
        {
            let mut card = c.card.clone();
            card.apply(UserActionTrigger::Void { reason })
                .expect("Pending 卡片可以作废");
            events.push(
                UserActionStateChanged {
                    card_id: card.id,
                    state: card.state,
                    output_artifact: None,
                }
                .into(),
            );
        }
        events
    }

    /// 外部事务提交成功后应用审批计划。提交失败时不调用，等待表保持原样。
    pub async fn committed(&self, events: &[DomainEvent]) {
        let convs: Vec<_> = lock_std(&self.conversations).values().cloned().collect();
        for conv in convs {
            apply_approval_changes(&mut *conv.lock().await, events);
        }
    }

    /// 对话删除、助手删除：作废全部待审批与卡片（方案 6.7），返回事件由调用方（T10）提交。
    pub async fn void_for_event(&self, event: &DomainEvent) -> Vec<DomainEvent> {
        match event {
            DomainEvent::ConversationDeleting(e) => {
                let conv = self.conversation(e.conversation_id);
                let mut state = conv.lock().await;
                self.void_where(&mut state, VoidReason::ConversationDeleted, |_| true)
            }
            DomainEvent::AssistantDeleted(e) => {
                let convs: Vec<_> = lock_std(&self.conversations).values().cloned().collect();
                let mut events = Vec::new();
                for conv in convs {
                    let mut state = conv.lock().await;
                    let turns: BTreeSet<TurnId> = state
                        .pending
                        .values()
                        .filter(|p| p.keys.assistant == e.assistant_id)
                        .map(|p| p.approval.turn_id)
                        .collect();
                    events.extend(
                        self.void_where(&mut state, VoidReason::AssistantDeleted, |t| {
                            turns.contains(&t)
                        }),
                    );
                }
                events
            }
            _ => Vec::new(),
        }
    }

    /// 亲手执行卡片到终态：把结果回给等待中的 hook（方案 5.5）。
    pub async fn on_user_action_settled(&self, card: UserActionId, state_now: &UserActionState) {
        let Some(message) = card_settled_message(state_now) else {
            return;
        };
        let Some(conv_id) = lock_std(&self.cards).get(&card).copied() else {
            return;
        };
        let conv = self.conversation(conv_id);
        let mut state = conv.lock().await;
        if let Some(mut c) = state.cards.remove(&card)
            && let Some(tx) = c.reply.take()
        {
            let _ = tx.send(message);
        }
    }

    /// 订阅的事件（已提交，带序号）：用户消息翻转拒绝记忆、活规则、清单变化、档位来源。
    pub async fn on_event(&self, seq: Seq, event: &DomainEvent) {
        match event {
            DomainEvent::MessageAppended(e) if e.message.author == Author::User => {
                let conv = self.conversation(e.message.conversation_id);
                let mut state = conv.lock().await;
                state.denial.apply(&DenialEvent::UserMessage);
                if let MessageBody::Inline(text) = &e.message.body {
                    state.last_user_message = text.clone();
                }
            }
            DomainEvent::RuleRemembered(_)
            | DomainEvent::RuleRevoked(_)
            | DomainEvent::RulesDeleted(_) => {
                lock_std(&self.live).apply(seq, event);
            }
            DomainEvent::SecurityListEntryAdded(_)
            | DomainEvent::SecurityListEntryRemoved(_)
            | DomainEvent::ProjectAdded(_)
            | DomainEvent::ProjectRemoved(_)
            | DomainEvent::AssistantCreated(_)
            | DomainEvent::AssistantDeleted(_) => {
                let convs: Vec<_> = lock_std(&self.conversations).values().cloned().collect();
                for conv in convs {
                    conv.lock().await.rebuild_session_needed = true;
                }
            }
            DomainEvent::ProjectSettingsUpdated(e) => {
                let m = e.settings.default_approval_mode;
                lock_std(&self.project_defaults).insert(e.project_id, m);
            }
            DomainEvent::DmProjectSwitched(e) => {
                let default = e
                    .project_id
                    .and_then(|p| lock_std(&self.project_defaults).get(&p).copied());
                let conv = self.conversation(e.conversation_id);
                conv.lock().await.modes = ConversationModes::Dm {
                    own_setting: None,
                    selected_project_default: e.project_id.map(|_| default.unwrap_or_default()),
                };
            }
            DomainEvent::TurnFinished(e) => {
                let convs: Vec<_> = lock_std(&self.conversations).values().cloned().collect();
                for conv in convs {
                    conv.lock().await.grants.retain(|_, g| g.turn != e.turn_id);
                }
            }
            _ => {}
        }
    }

    /// 启动恢复（方案 E1、E4）：按事件重建待审批（全部 detached）、拒绝记忆与活规则；
    /// 进行中的审查失去执行者 → 放弃、转问用户。返回放弃事件，由调用方在恢复事务里提交。
    pub async fn recover(
        &self,
        events: &[StoredEvent],
        keys_for: impl Fn(ConversationId) -> Option<SessionKeys>,
    ) -> Vec<DomainEvent> {
        let mut approvals: BTreeMap<ApprovalId, Approval> = BTreeMap::new();
        let mut denial: BTreeMap<ConversationId, Vec<DenialEvent>> = BTreeMap::new();
        for stored in events {
            match &stored.event {
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
                    denial
                        .entry(e.message.conversation_id)
                        .or_default()
                        .push(DenialEvent::UserMessage);
                }
                other => self.on_event(stored.seq, other).await,
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
            let Some(keys) = keys_for(approval.conversation_id) else {
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
        out
    }

    /// 等待中的审批（测试与恢复核对用）。
    pub async fn pending_approval(&self, id: ApprovalId) -> Option<Approval> {
        let conv_id = lock_std(&self.approvals).get(&id).copied()?;
        let conv = self.conversation(conv_id);
        let state = conv.lock().await;
        state.pending.get(&id).map(|p| p.approval.clone())
    }

    /// 一次性放行（测试核对用）。
    pub async fn grant(&self, conversation: ConversationId, id: ApprovalId) -> Option<GrantOnce> {
        self.conversation(conversation)
            .lock()
            .await
            .grants
            .get(&id)
            .cloned()
    }

    /// 拒绝记忆里某个指纹的阶段（测试核对用）。
    pub async fn denial_phase(
        &self,
        conversation: ConversationId,
        fp: &iwork_domain::action::ActionFingerprint,
    ) -> Option<crate::approval::denial::DenialPhase> {
        self.conversation(conversation).lock().await.denial.get(fp)
    }
}

impl NormalizedCall {
    /// 由持久化的调用记录重建（崩溃恢复、资格复查用）。亲手执行与说明文字不需要，留空。
    pub fn from_record(r: &ApprovalCallRecord, raw_summary: &str) -> Self {
        Self {
            tool: r.tool.clone(),
            actions: r.actions.clone(),
            shape: match r.shape {
                CallShapeKind::Tool => CallShape::Tool,
                CallShapeKind::SingleSimpleCommand => CallShape::SingleSimpleCommand {
                    command_text: raw_summary.to_owned(),
                },
                CallShapeKind::Compound => CallShape::Compound,
                CallShapeKind::Unparsable => CallShape::Unparsable { why: String::new() },
            },
            placement: r.placement,
            bypass_sandbox_requested: r.bypass_sandbox_requested,
            cwd: r.cwd.clone(),
            description: None,
            raw_summary: raw_summary.to_owned(),
            edit: None,
            raw_input_digest: r.raw_input_digest,
        }
    }
}

/// AI 审查回合的权限处理方：工具集为空，任何工具调用都拒绝（不递归起审查）。
struct DenyAll;

impl PermissionHandler for DenyAll {
    async fn on_permission(&self, _req: PermissionRequest) -> PermissionDecision {
        deny(MSG_NOT_IN_TOOL_SET)
    }
}

/// 一条会话的判定方：`turn::PermissionDecider` 的实现。
pub struct SessionDecider<C, G, S, A, T, R> {
    engine: Arc<ApprovalEngine<C, G, S, A, T, R>>,
    keys: Arc<SessionKeys>,
}

impl<C, G, S, A, T, R> PermissionDecider for SessionDecider<C, G, S, A, T, R>
where
    C: Clock + 'static,
    G: IdGenerator + 'static,
    S: StoragePort + 'static,
    A: AgentPort + 'static,
    T: Timer + 'static,
    R: PathResolver + 'static,
{
    async fn decide(&self, turn_id: TurnId, req: &PermissionRequest) -> PermissionDecision {
        self.engine.on_permission(&self.keys, turn_id, req).await
    }

    async fn committed(&self, events: &[DomainEvent]) {
        self.engine.committed(events).await;
    }

    async fn void_pending(&self, turn_id: TurnId, reason: VoidReason) -> Vec<DomainEvent> {
        self.engine
            .void_turn(self.keys.conversation, turn_id, reason)
            .await
    }
}
