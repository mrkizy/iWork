//! 领域事件（方案 3.3、3.6、4.5）。
//!
//! 与 `proto/iwork/v1/events.proto` 的 `DomainEvent` 一一对应：变体名 = proto 事件 message 名，
//! [`DomainEvent::kind`] = proto oneof 字段名（也是 `events.type` 列的取值）。两份靠 `iwork-app`
//! 的穷举对拍测试绑在一起。字段尽量与 proto 同名；proto 用几个松散字段表达、这里能用类型收紧的
//! （例如克隆结果、审查子状态带 `review_id`），在各载荷的文档里写明对应关系。
//!
//! 状态变化一律经事件：事件只追加不修改。

use std::path::PathBuf;

use crate::action::Action;
use crate::approval::{Approval, ApprovalState, ReviewPhase, ReviewState, StaleReason, VoidReason};
use crate::approval_mode::{ApprovalMode, RememberScope};
use crate::assistant::{Assistant, AssistantConfig};
use crate::attachment::Attachment;
use crate::common::{ArtifactRef, ModelSelection, Timestamp, Usage};
use crate::conversation::Conversation;
use crate::id::{
    ApprovalId, AssistantId, AttachmentId, ConversationId, JobId, MessageId, ProjectId, ReviewId,
    RuleId, RuntimeId, Seq, TurnId, UserActionId,
};
use crate::message::{Message, MessageBody, MessageStatus};
use crate::project::{PathStatus, Project, ProjectSettings};
use crate::rule::{RuleEntry, RuleLayer, RuleSetVersion, Verdict};
use crate::runtime::{LocalConfigItem, QuotaWindow, Runtime};
use crate::turn::{Turn, TurnState};
use crate::user_action::{UserActionCard, UserActionState};

// ---------- 事件里用到、实体里没有的值对象 ----------

/// 工具调用 ID：运行时给的不透明字符串。
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct ToolCallId(pub String);

/// MCP 服务 ID：本机导入时后台给的稳定字符串。
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct McpServerId(pub String);

/// 克隆的结果。proto 用 `ok` + `error` + `path` 三个字段表达；这里收成一个枚举，
/// 「成功却没有路径」「失败却没有原因」写不出来。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CloneOutcome {
    Cloned { path: PathBuf },
    Failed(CloneFailure),
}

/// 克隆失败的原因（映射到 proto `Error`）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CloneFailure {
    pub reason: CloneFailureReason,
    /// 给人看的说明；不含凭据
    pub message: String,
}

/// 克隆失败的种类，对应 proto 的错误码 `TARGET_NOT_EMPTY` / `CLONE_FAILED`。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum CloneFailureReason {
    TargetNotEmpty,
    CloneFailed,
}

/// AGENTS.md 草稿卡片的处理结果。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum DraftDecision {
    Accept,
    Discard,
}

/// 档位切换的作用对象。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ApprovalModeTarget {
    Conversation(ConversationId),
    /// 项目对话里的某个成员（成员单独设置优先）
    Member {
        conversation_id: ConversationId,
        assistant_id: AssistantId,
    },
    Project(ProjectId),
}

/// 档位为什么变了。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ApprovalModeChangeCause {
    User,
    /// 私聊切换项目，自己的覆盖失效
    ProjectSwitch,
    NewConversation,
}

/// 回合产生的改动摘要（技术设计 9.7 第 2 条）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChangeSummary {
    pub files_changed: u32,
    pub insertions: u32,
    pub deletions: u32,
    pub paths: Vec<PathBuf>,
}

/// 删除对话或助手时连带删掉规则的原因。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum RulesDeletedCause {
    ConversationDeleted,
    AssistantDeleted,
}

// ---------- 载荷：项目 ----------

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProjectAdded {
    pub project: Project,
}

/// 只带变了的字段。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProjectUpdated {
    pub project_id: ProjectId,
    pub name: Option<String>,
    pub pinned: Option<bool>,
    pub last_active_at: Option<Timestamp>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProjectRelocated {
    pub project_id: ProjectId,
    pub new_path: PathBuf,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProjectPathStatusChanged {
    pub project_id: ProjectId,
    pub path_status: PathStatus,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProjectRemoved {
    pub project_id: ProjectId,
}

/// 带改动后的完整设置：只带变更字段时表达不了「清空默认模型」。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProjectSettingsUpdated {
    pub project_id: ProjectId,
    pub settings: ProjectSettings,
}

/// 此时项目还没登记，没有 ID，流按路径。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RepositoryInitialized {
    pub path: PathBuf,
}

/// proto 的 `ok` / `error` / `path` 三个字段对应这里的 `outcome`。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CloneFinished {
    pub job_id: JobId,
    pub outcome: CloneOutcome,
}

/// 草稿卡片的 `card_id` 用 [`JobId`]：草稿由一次后台作业（系统回合）生成。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AgentsMdDraftProposed {
    pub card_id: JobId,
    pub project_id: ProjectId,
    pub content_artifact: ArtifactRef,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AgentsMdDraftDecided {
    pub card_id: JobId,
    pub project_id: ProjectId,
    pub decision: DraftDecision,
}

// ---------- 载荷：助手 ----------

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AssistantCreated {
    pub assistant: Assistant,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AssistantUpdated {
    pub assistant_id: AssistantId,
    pub version: u32,
    pub config: AssistantConfig,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AssistantDeleted {
    pub assistant_id: AssistantId,
    pub name_snapshot: String,
}

/// 带重置后的完整配置：重建投影时不能去读「当前程序里的内置默认值」，那会随程序版本变。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AssistantReset {
    pub assistant_id: AssistantId,
    pub version: u32,
    pub config: AssistantConfig,
}

// ---------- 载荷：对话与消息 ----------

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConversationCreated {
    pub conversation: Conversation,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConversationDeleting {
    pub conversation_id: ConversationId,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConversationDeleted {
    pub conversation_id: ConversationId,
}

/// 界面的「已切换为 X」系统记录由此渲染，不另发消息事件。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConversationModelChanged {
    pub conversation_id: ConversationId,
    pub model: ModelSelection,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ApprovalModeChanged {
    pub target: ApprovalModeTarget,
    /// `None` 表示恢复跟随（成员不再单独设置）
    pub mode: Option<ApprovalMode>,
    pub cause: ApprovalModeChangeCause,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DmProjectSwitched {
    pub conversation_id: ConversationId,
    /// `None` 表示切回纯私聊
    pub project_id: Option<ProjectId>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConversationRead {
    pub conversation_id: ConversationId,
    pub last_read_seq: Seq,
}

/// 助手消息此时 `Streaming`、正文为空。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MessageAppended {
    pub message: Message,
}

/// 消息结束时写入完整内容；`status` 是 `Completed` / `Interrupted` / `Failed` 之一。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MessageCompleted {
    pub conversation_id: ConversationId,
    pub message_id: MessageId,
    pub body: MessageBody,
    pub status: MessageStatus,
}

// ---------- 载荷：回合 ----------

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TurnQueued {
    pub turn: Turn,
}

/// `from` / `to` 带着中断原因（[`TurnState::Interrupted`] 的载荷）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TurnStateChanged {
    pub turn_id: TurnId,
    pub from: TurnState,
    pub to: TurnState,
    /// 给人看的说明，如失败原因
    pub reason: Option<String>,
}

/// `celebrate`：正常结束且有改动时盖「搞定」章（技术设计 9.7 第 2 条）。
#[derive(Debug, Clone, PartialEq)]
pub struct TurnFinished {
    pub turn_id: TurnId,
    pub state: TurnState,
    pub usage: Usage,
    pub celebrate: bool,
    pub change_summary: Option<ChangeSummary>,
}

/// 运行时实际报告的模型，与请求的不一致时以它为准。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TurnModelReported {
    pub turn_id: TurnId,
    pub model: ModelSelection,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ToolCallStarted {
    pub turn_id: TurnId,
    pub tool_call_id: ToolCallId,
    pub tool_name: String,
    pub action: Action,
}

/// 大段输出只带制品引用。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ToolCallFinished {
    pub turn_id: TurnId,
    pub tool_call_id: ToolCallId,
    pub ok: bool,
    pub output_artifact: Option<ArtifactRef>,
}

// ---------- 载荷：审批与规则 ----------

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ApprovalRequested {
    pub approval: Approval,
}

/// 审查子状态的每一次变化。proto 的 `review_id` / `review` / `reason` 三个字段都在
/// [`ReviewState`] 的载荷里；这里不会出现 `NotReviewed`。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ApprovalReviewUpdated {
    pub approval_id: ApprovalId,
    pub review: ReviewState,
}

/// `state` 是 `Approved { decider }` 或 `Denied`（拒绝的一定是用户）；`remember` 只有用户允许时才有。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ApprovalDecided {
    pub approval_id: ApprovalId,
    pub state: ApprovalState,
    pub remember: Option<RememberScope>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ApprovalVoided {
    pub approval_id: ApprovalId,
    pub reason: VoidReason,
}

/// 每次求值写一条，放行也记（技术设计 7.4）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ApprovalEvaluated {
    pub turn_id: TurnId,
    pub action: Action,
    pub matched_rule_ids: Vec<RuleId>,
    pub ruleset_version: RuleSetVersion,
    pub verdict: Verdict,
    pub layer_hit: RuleLayer,
    pub duration_ms: u32,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RuleRemembered {
    pub rule: RuleEntry,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RuleRevoked {
    pub rule_id: RuleId,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RulesDeleted {
    pub rule_ids: Vec<RuleId>,
    pub cause: RulesDeletedCause,
}

/// 迟到或对不上号的 AI 审查结果被丢弃（audit 流）。投影不读它、不改审批表。
/// `result` 是被丢弃的结果：`Passed` / `Failed` / `Errored` / `TimedOut` 之一。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ApprovalStaleResultDropped {
    pub approval_id: ApprovalId,
    pub review_id: ReviewId,
    pub result: ReviewPhase,
    pub why: StaleReason,
}

// ---------- 载荷：亲手执行 ----------

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UserActionProposed {
    pub card: UserActionCard,
}

/// proto 的 `state` / `exit` / `error` / `void_reason` 都在 [`UserActionState`] 的载荷里。
/// 不记进程身份：崩溃后不核对、不停止。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UserActionStateChanged {
    pub card_id: UserActionId,
    pub state: UserActionState,
    /// 进入 `Done` / `Unknown` 时带上已收到的输出，只给用户
    pub output_artifact: Option<ArtifactRef>,
}

// ---------- 载荷：安全清单 ----------

/// 清单条目就是规则条目，`layer` 标出属于哪张清单（见 [`crate::security_list`]）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SecurityListEntryAdded {
    pub rule: RuleEntry,
}

/// `entry_id` 是被删清单条目的规则 ID。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SecurityListEntryRemoved {
    pub entry_id: RuleId,
}

// ---------- 载荷：附件 ----------

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AttachmentStored {
    pub attachment: Attachment,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AttachmentConfirmed {
    pub conversation_id: ConversationId,
    pub attachment_id: AttachmentId,
}

// ---------- 载荷：运行时与额度 ----------

/// 登录信息只含 [`crate::runtime::LoginInfo`] 的四个字段。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RuntimeUpdated {
    pub runtime: Runtime,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct QuotaObserved {
    pub runtime_id: RuntimeId,
    pub windows: Vec<QuotaWindow>,
    pub observed_at: Timestamp,
}

#[derive(Debug, Clone, PartialEq)]
pub struct UsageReported {
    pub runtime_id: RuntimeId,
    pub turn_id: TurnId,
    pub usage: Usage,
}

// ---------- 载荷：导入 ----------

/// 记下「已提示」，之后不再提示。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LocalImportPrompted {
    pub runtime_id: RuntimeId,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LocalConfigImported {
    pub runtime_id: RuntimeId,
    pub items: Vec<LocalConfigItem>,
}

/// 只带键名，不带值：密钥直通钥匙串，不进事件、不进日志。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct McpSecretSet {
    pub mcp_server_id: McpServerId,
    pub key_name: String,
}

// ---------- 载荷：设置与系统 ----------

/// 只带变了的字段。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GlobalSettingsChanged {
    pub playful_ui: Option<bool>,
    pub autostart: Option<bool>,
    pub projects_root: Option<PathBuf>,
}

/// 崩溃恢复完成。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RecoveryCompleted {
    pub interrupted_turn_ids: Vec<TurnId>,
}

// ---------- 事件枚举 ----------

/// 从同一份清单生成 `DomainEvent` 枚举、[`DomainEvent::KINDS`] 与 [`DomainEvent::kind`]，
/// 三者不会漏对。加一个事件 = 清单加一行 + 写载荷 + 在 [`DomainEvent::stream`] 里补一条
/// （那里是穷举 `match`，漏了编译不过）。
macro_rules! domain_events {
    ($( $variant:ident => $kind:literal ),* $(,)?) => {
        /// 持久领域事件。每个变体一个同名载荷 struct。
        #[derive(Debug, Clone, PartialEq)]
        pub enum DomainEvent {
            $( $variant($variant) ),*
        }

        impl DomainEvent {
            /// 全部事件的蛇形名，按 proto oneof 的声明顺序；也是 `events.type` 列的取值。
            pub const KINDS: &'static [&'static str] = &[$($kind),*];

            /// 与 [`Self::KINDS`] 同序的变体名，单测用来核对蛇形名是由变体名机械转换来的。
            #[cfg(test)]
            const VARIANT_NAMES: &'static [&'static str] = &[$(stringify!($variant)),*];

            /// 本事件的蛇形名（`events.type` 列）。
            pub fn kind(&self) -> &'static str {
                match self {
                    $( Self::$variant(_) => $kind ),*
                }
            }
        }

        $(
            impl From<$variant> for DomainEvent {
                fn from(payload: $variant) -> Self {
                    Self::$variant(payload)
                }
            }
        )*
    };
}

domain_events! {
    ProjectAdded => "project_added",
    ProjectUpdated => "project_updated",
    ProjectRelocated => "project_relocated",
    ProjectPathStatusChanged => "project_path_status_changed",
    ProjectRemoved => "project_removed",
    ProjectSettingsUpdated => "project_settings_updated",
    RepositoryInitialized => "repository_initialized",
    CloneFinished => "clone_finished",
    AgentsMdDraftProposed => "agents_md_draft_proposed",
    AgentsMdDraftDecided => "agents_md_draft_decided",
    AssistantCreated => "assistant_created",
    AssistantUpdated => "assistant_updated",
    AssistantDeleted => "assistant_deleted",
    AssistantReset => "assistant_reset",
    ConversationCreated => "conversation_created",
    ConversationDeleting => "conversation_deleting",
    ConversationDeleted => "conversation_deleted",
    ConversationModelChanged => "conversation_model_changed",
    ApprovalModeChanged => "approval_mode_changed",
    DmProjectSwitched => "dm_project_switched",
    ConversationRead => "conversation_read",
    MessageAppended => "message_appended",
    MessageCompleted => "message_completed",
    TurnQueued => "turn_queued",
    TurnStateChanged => "turn_state_changed",
    TurnFinished => "turn_finished",
    TurnModelReported => "turn_model_reported",
    ToolCallStarted => "tool_call_started",
    ToolCallFinished => "tool_call_finished",
    ApprovalRequested => "approval_requested",
    ApprovalReviewUpdated => "approval_review_updated",
    ApprovalDecided => "approval_decided",
    ApprovalVoided => "approval_voided",
    ApprovalEvaluated => "approval_evaluated",
    RuleRemembered => "rule_remembered",
    RuleRevoked => "rule_revoked",
    RulesDeleted => "rules_deleted",
    ApprovalStaleResultDropped => "approval_stale_result_dropped",
    UserActionProposed => "user_action_proposed",
    UserActionStateChanged => "user_action_state_changed",
    SecurityListEntryAdded => "security_list_entry_added",
    SecurityListEntryRemoved => "security_list_entry_removed",
    AttachmentStored => "attachment_stored",
    AttachmentConfirmed => "attachment_confirmed",
    RuntimeUpdated => "runtime_updated",
    QuotaObserved => "quota_observed",
    UsageReported => "usage_reported",
    LocalImportPrompted => "local_import_prompted",
    LocalConfigImported => "local_config_imported",
    McpSecretSet => "mcp_secret_set",
    GlobalSettingsChanged => "global_settings_changed",
    RecoveryCompleted => "recovery_completed",
}

/// 事件所属流的种类（技术设计 5.3 `events.stream_kind`）。存储按流补发，订阅按流过滤。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum StreamKind {
    Project,
    Assistant,
    Conversation,
    Turn,
    Approval,
    /// 审计：求值记录、被丢弃的迟到审查结果。投影不读
    Audit,
    Rule,
    Runtime,
    Settings,
    System,
}

/// 流内的实体标识（`events.stream_id`），一般是实体 ID 的文本。
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct StreamId(pub String);

impl StreamId {
    fn of(id: impl core::fmt::Display) -> Option<Self> {
        Some(Self(id.to_string()))
    }
}

impl DomainEvent {
    /// 事件所属的流。`None` 表示整类一条流（设置、系统、批量删除规则）。
    pub fn stream(&self) -> (StreamKind, Option<StreamId>) {
        use StreamKind as K;
        match self {
            Self::ProjectAdded(e) => (K::Project, StreamId::of(e.project.id)),
            Self::ProjectUpdated(e) => (K::Project, StreamId::of(e.project_id)),
            Self::ProjectRelocated(e) => (K::Project, StreamId::of(e.project_id)),
            Self::ProjectPathStatusChanged(e) => (K::Project, StreamId::of(e.project_id)),
            Self::ProjectRemoved(e) => (K::Project, StreamId::of(e.project_id)),
            Self::ProjectSettingsUpdated(e) => (K::Project, StreamId::of(e.project_id)),
            Self::RepositoryInitialized(e) => (K::Project, StreamId::of(e.path.display())),
            Self::CloneFinished(e) => (K::Project, StreamId::of(e.job_id)),
            Self::AgentsMdDraftProposed(e) => (K::Project, StreamId::of(e.project_id)),
            Self::AgentsMdDraftDecided(e) => (K::Project, StreamId::of(e.project_id)),
            Self::AssistantCreated(e) => (K::Assistant, StreamId::of(e.assistant.id)),
            Self::AssistantUpdated(e) => (K::Assistant, StreamId::of(e.assistant_id)),
            Self::AssistantDeleted(e) => (K::Assistant, StreamId::of(e.assistant_id)),
            Self::AssistantReset(e) => (K::Assistant, StreamId::of(e.assistant_id)),
            Self::ConversationCreated(e) => (K::Conversation, StreamId::of(e.conversation.id)),
            Self::ConversationDeleting(e) => (K::Conversation, StreamId::of(e.conversation_id)),
            Self::ConversationDeleted(e) => (K::Conversation, StreamId::of(e.conversation_id)),
            Self::ConversationModelChanged(e) => (K::Conversation, StreamId::of(e.conversation_id)),
            Self::ApprovalModeChanged(e) => match e.target {
                ApprovalModeTarget::Conversation(id)
                | ApprovalModeTarget::Member {
                    conversation_id: id,
                    ..
                } => (K::Conversation, StreamId::of(id)),
                ApprovalModeTarget::Project(id) => (K::Project, StreamId::of(id)),
            },
            Self::DmProjectSwitched(e) => (K::Conversation, StreamId::of(e.conversation_id)),
            Self::ConversationRead(e) => (K::Conversation, StreamId::of(e.conversation_id)),
            Self::MessageAppended(e) => (K::Conversation, StreamId::of(e.message.conversation_id)),
            Self::MessageCompleted(e) => (K::Conversation, StreamId::of(e.conversation_id)),
            Self::TurnQueued(e) => (K::Turn, StreamId::of(e.turn.id)),
            Self::TurnStateChanged(e) => (K::Turn, StreamId::of(e.turn_id)),
            Self::TurnFinished(e) => (K::Turn, StreamId::of(e.turn_id)),
            Self::TurnModelReported(e) => (K::Turn, StreamId::of(e.turn_id)),
            Self::ToolCallStarted(e) => (K::Turn, StreamId::of(e.turn_id)),
            Self::ToolCallFinished(e) => (K::Turn, StreamId::of(e.turn_id)),
            Self::ApprovalRequested(e) => (K::Approval, StreamId::of(e.approval.id)),
            Self::ApprovalReviewUpdated(e) => (K::Approval, StreamId::of(e.approval_id)),
            Self::ApprovalDecided(e) => (K::Approval, StreamId::of(e.approval_id)),
            Self::ApprovalVoided(e) => (K::Approval, StreamId::of(e.approval_id)),
            Self::ApprovalEvaluated(e) => (K::Audit, StreamId::of(e.turn_id)),
            Self::RuleRemembered(e) => (K::Rule, StreamId::of(e.rule.id)),
            Self::RuleRevoked(e) => (K::Rule, StreamId::of(e.rule_id)),
            Self::RulesDeleted(_) => (K::Rule, None),
            Self::ApprovalStaleResultDropped(e) => (K::Audit, StreamId::of(e.approval_id)),
            Self::UserActionProposed(e) => (K::Approval, StreamId::of(e.card.id)),
            Self::UserActionStateChanged(e) => (K::Approval, StreamId::of(e.card_id)),
            Self::SecurityListEntryAdded(e) => (K::Rule, StreamId::of(e.rule.id)),
            Self::SecurityListEntryRemoved(e) => (K::Rule, StreamId::of(e.entry_id)),
            Self::AttachmentStored(e) => {
                (K::Conversation, StreamId::of(e.attachment.conversation_id))
            }
            Self::AttachmentConfirmed(e) => (K::Conversation, StreamId::of(e.conversation_id)),
            Self::RuntimeUpdated(e) => (K::Runtime, StreamId::of(e.runtime.id)),
            Self::QuotaObserved(e) => (K::Runtime, StreamId::of(e.runtime_id)),
            Self::UsageReported(e) => (K::Runtime, StreamId::of(e.runtime_id)),
            Self::LocalImportPrompted(e) => (K::Runtime, StreamId::of(e.runtime_id)),
            Self::LocalConfigImported(e) => (K::Runtime, StreamId::of(e.runtime_id)),
            Self::McpSecretSet(e) => (K::Runtime, StreamId::of(&e.mcp_server_id.0)),
            Self::GlobalSettingsChanged(_) => (K::Settings, None),
            Self::RecoveryCompleted(_) => (K::System, None),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::DomainEvent;

    /// 驼峰变体名机械转成蛇形名，作为 KINDS 的独立期望值。
    fn snake(name: &str) -> String {
        let mut out = String::new();
        for (i, ch) in name.chars().enumerate() {
            if ch.is_ascii_uppercase() {
                if i > 0 {
                    out.push('_');
                }
                out.push(ch.to_ascii_lowercase());
            } else {
                out.push(ch);
            }
        }
        out
    }

    #[test]
    fn event_kinds_are_snake_case_of_variant_names() {
        assert_eq!(DomainEvent::KINDS.len(), DomainEvent::VARIANT_NAMES.len());
        // 对照：清单非空，证明下面的逐条比较真的比了东西
        assert!(DomainEvent::KINDS.len() > 50);
        for (kind, variant) in DomainEvent::KINDS.iter().zip(DomainEvent::VARIANT_NAMES) {
            assert_eq!(*kind, snake(variant), "事件 {variant} 的蛇形名写错了");
        }
    }

    #[test]
    fn event_kinds_are_unique() {
        let mut kinds = DomainEvent::KINDS.to_vec();
        kinds.sort_unstable();
        kinds.dedup();
        assert_eq!(kinds.len(), DomainEvent::KINDS.len());
    }
}
