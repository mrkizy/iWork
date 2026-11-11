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

/// 拼续接摘要用的原料（T06 方案 5.2a）：一条会话线上、某一轮请求之前的对话正文。只有用户消息与助手回复；
/// 工具输出、审批与卡片、系统消息、附件内容都不在这里。
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct SummarySource {
    /// 这条会话线上的第一条用户消息；它就是这一轮的请求时为 `None`
    pub first_user: Option<SummaryLine>,
    /// `first_user` 之后、这一轮的请求之前的用户消息与助手回复，**从最新往前排**，最多 `recent_limit` 条
    pub earlier: Vec<SummaryLine>,
    /// `first_user` 与 `earlier` 之间还有多少条没取（超出 `recent_limit` 的那些）
    pub unfetched: u32,
    /// 这一轮用户的请求；认不出（这一轮不是由用户消息触发的）时为 `None`
    pub request: Option<SummaryLine>,
}

/// 摘要里的一条消息。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SummaryLine {
    pub who: SummaryWho,
    pub text: SummaryText,
    /// 附件只留文件名，不带内容
    pub attachments: Vec<String>,
}

/// 谁说的。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SummaryWho {
    User,
    /// `name`：发言时的名字
    Assistant {
        name: String,
    },
}

/// 消息正文。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SummaryText {
    Inline(String),
    /// 正文太长、存成了制品：摘要不去读它
    Stored,
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
    /// 续接失败、用摘要新开会话时取这一轮之前的对话（T06 方案 5.2a）。只取回合 `turn` 所在对话里
    /// `context_project_id == line` 的消息（助手私聊按「助手 × 项目」各是一条会话线）。「这一轮的请求」是
    /// 和这个回合的 `TurnQueued` 同一次提交写下的那条用户消息。回合不存在时报 `NotFound`。
    fn summary_source(
        &self,
        turn: TurnId,
        line: Option<ProjectId>,
        recent_limit: u32,
    ) -> impl Future<Output = Result<SummarySource, StorageError>> + Send;
    /// `ReadArtifact` 的范围校验：引用是否被某条消息、工具调用、附件或卡片引用。
    fn artifact_is_referenced(
        &self,
        r: &ArtifactRef,
    ) -> impl Future<Output = Result<bool, StorageError>> + Send;
}
