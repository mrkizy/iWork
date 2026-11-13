//! 查询用投影（方案 3.3 查询表）。方法与查询一一对应，返回 domain 读类型；应用服务再映射到 proto。

use std::future::Future;

use iwork_domain::approval::{Approval, OwnerFacts};
use iwork_domain::approval_mode::ApprovalMode;
use iwork_domain::assistant::Assistant;
use iwork_domain::common::{ArtifactRef, Timestamp};
use iwork_domain::conversation::Conversation;
use iwork_domain::id::{
    AssistantId, ConversationId, JobId, MessageId, ProjectId, RuntimeId, Seq, TurnId,
};
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

/// 回合宿主对账要读的全部事实（T10a 方案 2.2），一个读事务。`head` 是这个事务看到的最新 seq。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TurnWork {
    pub head: Seq,
    /// 有没到终态的回合的对话，加上在删的对话（删除要接着做），按对话 id 升序
    pub conversations: Vec<ConversationWork>,
}

/// 一个有活干的对话。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConversationWork {
    /// 含状态（`Active` / `Deleting`）
    pub conversation: Conversation,
    /// 没到终态的回合，按排进来的先后（`TurnQueued` 的 seq）升序：第一个就是「最老的没到终态的回合」
    pub open_turns: Vec<OpenTurn>,
    /// 名下还有没到终态的卡片（结果未知的也算没到）：删除在等它
    pub cards_open: bool,
}

/// 一个没到终态的回合，连同宿主判断要用的几个 seq。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OpenTurn {
    pub turn: Turn,
    /// 这个回合的归属事实（对话在不在删、它的助手在不在）。项目对话里各回合的助手可以不同，所以按回合给
    pub owner: OwnerFacts,
    /// 一趟编排的起点：最近一次从 `Queued` 或已中断进到 `Running` 的那笔提交
    pub run_started_seq: Option<Seq>,
    /// 进入现在这次中断的 seq；不在中断为 `None`
    pub interrupted_seq: Option<Seq>,
    /// 最近一条取消意图
    pub cancel_intent_seq: Option<Seq>,
    /// 这次中断之后最近一条继续意图（进入中断时清空）
    pub resume_intent_seq: Option<Seq>,
}

/// 退出前要问的两个数（E1）。卡片那个数由 T10e 加。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct QuitCounts {
    /// 占着运行时进程的回合（`TurnState::holds_process`）
    pub running_turns: u32,
    /// 排着的回合
    pub queued_turns: u32,
}

/// 算一个对话的会话快照要的已提交输入（D8），一个读事务。路径的解析不在这里（要读磁盘），由调用方做。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SnapshotInputs {
    pub conversation: Conversation,
    /// 这个对话里各成员单独设置的档位
    pub member_modes: Vec<(AssistantId, ApprovalMode)>,
    /// 全部项目：本对话的项目（或私聊所选项目）在里面，别的项目的根目录进快照的临时敏感目录
    pub projects: Vec<Project>,
    /// 全部助手：成员的工具配置在里面，别的助手的私聊目录进快照的临时敏感目录
    pub assistants: Vec<Assistant>,
    /// 三张安全清单的全部条目（同 `security_lists`）
    pub security_lists: Vec<RuleEntry>,
    /// 读清单时 `rule` 流的最新 seq（审计的 `snapshot_lists_seq`）
    pub lists_seq: Seq,
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
    /// `context_project_id == line` 的消息（助手私聊按「助手 × 项目」各是一条会话线），并且只取排这个回合
    /// （`TurnQueued`）的那次提交**之前**写下的：这一轮的请求和它同一次提交，不在里面；这一轮之后的也不在。
    /// 回合不存在时报 `NotFound`。
    fn summary_source(
        &self,
        turn: TurnId,
        line: Option<ProjectId>,
        recent_limit: u32,
    ) -> impl Future<Output = Result<SummarySource, StorageError>> + Send;
    /// 回合宿主对账（T10a D1、D5–D7、X2–X4）。
    fn turn_work(&self) -> impl Future<Output = Result<TurnWork, StorageError>> + Send;
    /// 这个对话全部 `TurnSessionBound` 里 seq 最大的那一条的会话编号（D3a）；一条都没有为 `None`。
    /// 读事件，不读投影：投影上只有每个回合最后一个值。
    fn last_session_binding(
        &self,
        conversation: ConversationId,
    ) -> impl Future<Output = Result<Option<String>, StorageError>> + Send;
    /// 这个对话全部 `TurnSessionBound` 里出现过的会话编号（含 `replaced` 里的旧编号），去重，按第一次出现的
    /// 先后（X4）。读事件。
    fn session_ids_of(
        &self,
        conversation: ConversationId,
    ) -> impl Future<Output = Result<Vec<String>, StorageError>> + Send;
    /// 退出前要问的数（E1）。
    fn quit_counts(&self) -> impl Future<Output = Result<QuitCounts, StorageError>> + Send;
    /// 受理回合命令要读的（T10a 方案 A5）：回合与它所在的对话，一个读事务。回合不存在为 `None`。
    fn turn_with_conversation(
        &self,
        turn: TurnId,
    ) -> impl Future<Output = Result<Option<(Turn, Conversation)>, StorageError>> + Send;
    /// 算会话快照的已提交输入（D8）。对话不存在时报 `NotFound`。
    fn snapshot_inputs(
        &self,
        conversation: ConversationId,
    ) -> impl Future<Output = Result<SnapshotInputs, StorageError>> + Send;
    /// `ReadArtifact` 的范围校验：引用是否被某条消息、工具调用、附件或卡片引用。
    fn artifact_is_referenced(
        &self,
        r: &ArtifactRef,
    ) -> impl Future<Output = Result<bool, StorageError>> + Send;
}
