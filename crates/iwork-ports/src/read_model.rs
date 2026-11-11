//! 查询用投影（方案 3.3 查询表）。方法与查询一一对应，返回 domain 读类型；应用服务再映射到 proto。

use std::future::Future;

use iwork_domain::approval::Approval;
use iwork_domain::assistant::Assistant;
use iwork_domain::common::{ArtifactRef, Timestamp};
use iwork_domain::conversation::Conversation;
use iwork_domain::id::{ConversationId, JobId, MessageId, ProjectId, RuntimeId, TurnId};
use iwork_domain::message::Message;
use iwork_domain::project::Project;
use iwork_domain::rule::RuleEntry;
use iwork_domain::runtime::{QuotaWindow, Runtime};
use iwork_domain::settings::GlobalSettings;
use iwork_domain::turn::{InterruptReason, Turn};
use iwork_domain::user_action::UserActionCard;

use crate::store::StorageError;

/// 键集分页：取 `before` 之前（不含）的 `limit` 条；`before` 为 `None` 取最新的。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MessagePage {
    pub before: Option<MessageId>,
    pub limit: u32,
}

/// 一个对话的可见窗口。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConversationView {
    pub conversation: Conversation,
    pub unread: u32,
    pub messages: Vec<Message>,
    pub turns: Vec<Turn>,
}

/// 对话列表的一行。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConversationSummary {
    pub conversation: Conversation,
    pub unread: u32,
}

/// 项目列表的一行。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProjectSummary {
    pub project: Project,
    pub unread: u32,
}

/// 添加项目后系统回合生成的 AGENTS.md 草稿卡片。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AgentsMdDraftCard {
    pub card_id: JobId,
    pub project_id: ProjectId,
    pub content: ArtifactRef,
    pub proposed_at: Timestamp,
}

/// 回合中断后的「继续 / 放弃」卡片。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResumeCard {
    pub turn_id: TurnId,
    pub conversation_id: ConversationId,
    pub reason: InterruptReason,
}

/// 「待你处理」的一项。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PendingItem {
    Approval(Approval),
    UserAction(UserActionCard),
    AgentsMdDraft(AgentsMdDraftCard),
    Resume(ResumeCard),
}

/// 一个运行时最近一次观测到的额度。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RuntimeQuota {
    pub runtime_id: RuntimeId,
    pub windows: Vec<QuotaWindow>,
    pub observed_at: Timestamp,
}

/// 上次运行没收尾的东西（T04 方案 2.5）。一个读事务里取，交给崩溃恢复的纯函数。
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Unsettled {
    /// 持有运行时进程的回合（`TurnState::holds_process`）。`Queued` 的回合一定没发过 prompt，不在这里。
    pub turns: Vec<Turn>,
    /// 这些回合名下还在流式的助手消息；正文已用检查点填好。
    pub streaming: Vec<Message>,
    /// 执行到一半的亲手执行卡片（转换表里能接 `ProcessLost` 的状态：启动中、执行中、停止中）。
    pub cards: Vec<UserActionCard>,
}

/// 查询模型。
pub trait ReadModelPort: Send + Sync {
    fn conversation(
        &self,
        id: ConversationId,
        page: MessagePage,
    ) -> impl Future<Output = Result<ConversationView, StorageError>> + Send;
    fn conversations(
        &self,
        project: Option<ProjectId>,
    ) -> impl Future<Output = Result<Vec<ConversationSummary>, StorageError>> + Send;
    fn projects(&self) -> impl Future<Output = Result<Vec<ProjectSummary>, StorageError>> + Send;
    fn assistants(&self) -> impl Future<Output = Result<Vec<Assistant>, StorageError>> + Send;
    fn pending_items(&self) -> impl Future<Output = Result<Vec<PendingItem>, StorageError>> + Send;
    fn quota(&self) -> impl Future<Output = Result<Vec<RuntimeQuota>, StorageError>> + Send;
    fn runtimes(&self) -> impl Future<Output = Result<Vec<Runtime>, StorageError>> + Send;
    fn rules(
        &self,
        project: Option<ProjectId>,
    ) -> impl Future<Output = Result<Vec<RuleEntry>, StorageError>> + Send;
    /// 三张安全清单的全部条目。清单条目就是规则条目，按 `RuleEntry::security_list()` 分清单，
    /// 内置看 `RuleEntry::is_builtin()`；不另存清单视图。
    fn security_lists(&self) -> impl Future<Output = Result<Vec<RuleEntry>, StorageError>> + Send;
    fn settings(&self) -> impl Future<Output = Result<GlobalSettings, StorageError>> + Send;
    /// 上次运行没收尾的回合、流式消息与卡片。各自按 id 升序。
    fn unsettled(&self) -> impl Future<Output = Result<Unsettled, StorageError>> + Send;
    /// `ReadArtifact` 的范围校验：引用是否被某条消息、工具调用、附件或卡片引用。
    fn artifact_is_referenced(
        &self,
        r: &ArtifactRef,
    ) -> impl Future<Output = Result<bool, StorageError>> + Send;
}
