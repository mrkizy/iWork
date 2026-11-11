//! 审批引擎的有状态部分（T07 方案第 6 节、6.10；ADR-T07-c、ADR-T07-e）。
//!
//! 引擎内存就是已提交事件的折叠（[`fold::Folded`]），只由追平按 `seq` 推进；本地决定只产生要提交的事件。
//! 每条决定路径：拿令牌（含屏障）→ 追平 → 在视图上算计划 → 提交 → 追平（[`sync`]）。
//! 等待登记与会话绑定不在折叠里（[`waiting`]）。会牵动审批的外部命令经 [`ApprovalEngine::commit_with`]。
//!
//! 与回合编排的接缝：引擎为每条会话给出一个 [`SessionDecider`]，实现 `turn::PermissionDecider`。
//! 回合状态只由编排写；回合作废的计划经 `void_pending` 交给编排，与回合状态同一次提交。

mod changes;
mod context;
mod decisions;
mod external;
pub mod fold;
pub mod lock;
pub mod review;
mod runtime;
mod sync;
mod waiting;

pub use external::{Committed, Scope};
pub use sync::{CATCH_UP_BATCH, EngineError, REPLAN_ATTEMPTS, VoidHold};

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
    ApprovalTrigger, ReviewState, VoidReason,
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
    ApprovalId, AssistantId, CommandId, ConversationId, ProjectId, ReviewId, RuleId, RuntimeId,
    Seq, TurnId, UserActionId,
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
use iwork_ports::store::{CommitReceipt, CommitRequest, StorageError, StoragePort};

use crate::approval::action::{
    CallShape, NormalizeContext, NormalizedCall, analyze, gather_path_facts, normalize,
};
use crate::approval::denial::DenialMemory;
use crate::approval::env::{EnvPaths, ScopeKeys};
use crate::approval::evaluate::proposal::proposal_hash;
use crate::approval::evaluate::remember::{offered_remember, remember_rule};
use crate::approval::evaluate::{CallEvaluation, EvalContext, evaluate_call};
use crate::approval::rules::{RuleSetError, build_ruleset};
use crate::approval::session::SessionSnapshot;
use crate::turn::{PermissionDecider, VoidPlanError};
use changes::{modes_after, plan_approval};
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
    /// 审查运行时端口对应的身份；未指定审查运行时时沿用它。
    pub runtime_id: RuntimeId,
    pub assistant_version: u32,
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
    /// 已处理和从没有过，对用户是一回事
    #[error("审批 {0} 已不在待审批")]
    NotPending(ApprovalId),
    #[error("审批 {id} 没有提供记住范围 {remember:?}")]
    RememberNotOffered {
        id: ApprovalId,
        remember: RememberScope,
    },
    #[error("审批 {0} 的动作已被规则拒绝，不能放行")]
    NoLongerAllowed(ApprovalId),
    /// 审批所属的回合、对话或助手已经不在（孤儿，等清孤儿作废）：只能拒绝
    #[error("审批 {0} 所属的对话或助手已删除，不能放行")]
    OwnerGone(ApprovalId),
    /// 没有这条审批的会话快照（重启后给不出 `SessionKeys`）：重新求值要用它，没有就不放行
    #[error("审批 {0} 的会话已不可用，不能放行")]
    SessionUnavailable(ApprovalId),
    /// 提交在追平后重算 3 次仍被拒（可重试）
    #[error("审批状态刚被改过，请重试")]
    Conflict,
    #[error(transparent)]
    Storage(#[from] StorageError),
    #[error(transparent)]
    Fold(#[from] fold::FoldError),
    #[error(transparent)]
    Apply(#[from] ApprovalApplyError),
    #[error(transparent)]
    Rules(#[from] RuleSetError),
}

impl From<EngineError> for DecideError {
    fn from(error: EngineError) -> Self {
        match error {
            EngineError::Storage(e) => Self::Storage(e),
            EngineError::Fold(e) => Self::Fold(e),
            EngineError::Conflict => Self::Conflict,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ModeError {
    #[error("项目默认档位不能设为全开")]
    FullAccessNotAllowedAsDefault,
    #[error("只有成员目标可以恢复跟随主管")]
    FollowOnlyForMember,
    #[error("找不到对话 {0}")]
    ConversationNotFound(ConversationId),
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
///
/// 「必问 / 解析不了是不是新出现的」不用请求时的求值比：审查只在两者都没有时才启动，所以审查通过时出现
/// 即新出现；切档本来就要求全部子动作放行；一次性放行比对的是同一次调用（方案 6.10.2）。
pub fn still_eligible(now: &CallEvaluation, waiver: Waiver) -> Result<(), NotEligible> {
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
    if waiver != Waiver::GrantOnce && (now.constraints.must_ask || now.constraints.unparsable) {
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
    mcp_read_only: BTreeSet<String>,
    /// 折叠、等待登记、会话绑定。std 锁，从不跨 `await`
    inner: Mutex<waiting::Inner>,
    /// 每个对话一把决策令牌
    tokens: Mutex<BTreeMap<ConversationId, Arc<AsyncLock<sync::Token>>>>,
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
    /// 唯一的构造函数（方案 6.10.6 第 10 条）：从 seq 0 折到 head，对每条待审批用 `keys_for` 填会话绑定，
    /// 返回引擎与恢复计划（失去执行者的审查 → 放弃；孤儿 → 作废），调用方在接受连接之前提交它。
    pub async fn start(
        ports: EnginePorts<C, G, S, A, T, R>,
        mcp_read_only: BTreeSet<String>,
        keys_for: impl Fn(ConversationId, TurnId) -> Option<SessionKeys>,
    ) -> Result<(Arc<Self>, Vec<DomainEvent>), EngineError> {
        let engine = Arc::new(Self {
            ports,
            mcp_read_only,
            inner: Mutex::default(),
            tokens: Mutex::default(),
        });
        engine.catch_up().await?;
        let plan = {
            let mut inner = lock_std(&engine.inner);
            let mut bindings = BTreeMap::new();
            let mut plan = Vec::new();
            let f = &inner.folded;
            for (cid, conv) in f.conversations() {
                let mut shared: BTreeMap<TurnId, Option<Arc<SessionKeys>>> = BTreeMap::new();
                for (id, p) in &conv.pending {
                    let turn = p.approval.turn_id;
                    if let Some(keys) = shared
                        .entry(turn)
                        .or_insert_with(|| keys_for(*cid, turn).map(Arc::new))
                    {
                        bindings.insert(
                            *id,
                            waiting::Binding {
                                turn,
                                conversation: *cid,
                                keys: keys.clone(),
                            },
                        );
                    }
                }
            }
            // 恢复计划和运行中的清理是同一条常设义务：孤儿作废、失去执行者的审查放弃（启动时没有执行者）
            for cid in inner.due_conversations() {
                plan.extend(inner.due(cid));
            }
            inner.bindings = bindings;
            plan
        };
        Ok((engine, plan))
    }

    /// 一条会话的判定方，交给 `SessionLine::new`。
    pub fn decider_for(self: &Arc<Self>, keys: SessionKeys) -> SessionDecider<C, G, S, A, T, R> {
        SessionDecider {
            engine: self.clone(),
            keys: Arc::new(keys),
        }
    }

    fn new_id(&self) -> iwork_domain::id::RawUlid {
        self.ports.ids.next()
    }

    fn request(&self, actor: &str, events: Vec<DomainEvent>) -> CommitRequest {
        CommitRequest {
            command_id: CommandId::from_raw(self.new_id()),
            actor: Actor(actor.into()),
            events,
            outbox: Vec::new(),
        }
    }

    /// 不在令牌下的提交（审查回合自己的流水、项目默认档位）。
    async fn commit_raw(
        &self,
        actor: &str,
        events: Vec<DomainEvent>,
    ) -> Result<CommitReceipt, StorageError> {
        self.ports.store.commit(self.request(actor, events)).await
    }

    async fn commit(&self, actor: &str, events: Vec<DomainEvent>) -> Result<(), StorageError> {
        self.commit_raw(actor, events).await.map(|_| ())
    }

    // ---------- 读（都读折叠此刻的值，不追平；要最新值先调 `sync` 或 `catch_up`） ----------

    /// 折叠的一份拷贝（测试比对「内存等于折叠」用）。
    pub fn folded(&self) -> fold::Folded {
        lock_std(&self.inner).folded.clone()
    }

    pub fn effective_mode(
        &self,
        conversation: ConversationId,
        assistant: AssistantId,
    ) -> ApprovalMode {
        lock_std(&self.inner)
            .folded
            .conversation(conversation)
            .map_or(ApprovalMode::Manual, |c| c.effective_mode(assistant))
    }

    /// 最后一条会让会话快照过期的事件；T10 拿它和「生成这份会话快照之前读到的 head」比。
    pub fn snapshot_inputs_seq(&self) -> Seq {
        lock_std(&self.inner).folded.snapshot_inputs_seq()
    }

    /// R1 的「要求续接」：有一次性放行、回合在 `Interrupted` 的回合。
    pub fn turns_to_resume(&self) -> Vec<TurnId> {
        lock_std(&self.inner).folded.turns_to_resume()
    }

    pub fn pending_approval(&self, id: ApprovalId) -> Option<Approval> {
        lock_std(&self.inner)
            .folded
            .pending_approval(id)
            .map(|p| p.approval.clone())
    }

    pub fn grant(&self, id: ApprovalId) -> Option<GrantOnce> {
        lock_std(&self.inner).folded.grant(id).cloned()
    }

    pub fn denial_phase(
        &self,
        conversation: ConversationId,
        fp: &iwork_domain::action::ActionFingerprint,
    ) -> Option<crate::approval::denial::DenialPhase> {
        lock_std(&self.inner)
            .folded
            .conversation(conversation)
            .and_then(|c| c.denial.get(fp))
    }

    /// 这条审批绑定的会话键（测试核对「各用各的快照」用）。
    pub fn binding_keys(&self, id: ApprovalId) -> Option<Arc<SessionKeys>> {
        lock_std(&self.inner)
            .bindings
            .get(&id)
            .map(|b| b.keys.clone())
    }

    /// 会话绑定的条数。
    pub fn binding_count(&self) -> usize {
        lock_std(&self.inner).bindings.len()
    }

    /// 仅测试：对调两条审批的会话绑定（`f5_wrong_binding_is_caught` 证明放行结果取决于绑定）。
    /// 只在 `test-hooks` 特性下编出来；正式构建里没有这个入口。
    #[cfg(feature = "test-hooks")]
    #[doc(hidden)]
    pub fn swap_bindings_for_test(&self, a: ApprovalId, b: ApprovalId) {
        let mut inner = lock_std(&self.inner);
        let ka = inner.bindings.get(&a).map(|x| x.keys.clone());
        let kb = inner.bindings.get(&b).map(|x| x.keys.clone());
        if let (Some(ka), Some(kb)) = (ka, kb) {
            inner.bindings.get_mut(&a).expect("刚取过").keys = kb;
            inner.bindings.get_mut(&b).expect("刚取过").keys = ka;
        }
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

impl<C, G, S, A, T, R> SessionDecider<C, G, S, A, T, R> {
    /// 这条会话线的身份与快照。
    pub fn keys(&self) -> &Arc<SessionKeys> {
        &self.keys
    }
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
    type VoidHold = VoidHold;

    async fn decide(&self, turn_id: TurnId, req: &PermissionRequest) -> crate::turn::Decided {
        self.engine.on_permission(&self.keys, turn_id, req).await
    }

    /// 只是提醒「有新事件」：追平，失败在里面记日志。
    async fn committed(&self, _events: &[DomainEvent]) {
        self.engine.nudge().await;
    }

    /// 拿令牌、追平、算作废计划，把令牌包成句柄交出去；编排那笔提交返回或到期才丢。
    async fn void_pending(
        &self,
        turn_id: TurnId,
        reason: VoidReason,
    ) -> Result<(Vec<DomainEvent>, VoidHold), VoidPlanError> {
        self.engine
            .void_plan(self.keys.conversation, turn_id, reason)
            .await
            .map_err(|error| match error {
                EngineError::Storage(e) => VoidPlanError::Storage(e),
                EngineError::Fold(e) => VoidPlanError::Unavailable(e.to_string()),
                EngineError::Conflict => VoidPlanError::Unavailable(error.to_string()),
            })
    }
}
