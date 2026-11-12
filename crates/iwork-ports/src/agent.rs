//! Agent 运行时端口（技术设计 6.1）。后台服务只经它和运行时对话，厂商差异全在适配器里。

use std::future::Future;
use std::path::PathBuf;
use std::time::Duration;

use iwork_domain::action::ToolRef;
use iwork_domain::common::{ArtifactRef, EffortLevel, ModelId, ModelSelection, Timestamp, Usage};
use iwork_domain::event::{McpServerId, ToolCallId};
use iwork_domain::runtime::QuotaWindow;
use iwork_domain::session::{SessionConstraints, SessionLostReason, SessionToolSet};
use iwork_domain::turn::TurnKind;

/// 运行时自报的能力。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RuntimeCapabilities {
    /// 能否按 `runtime_session_id` 续接会话
    pub supports_resume: bool,
    /// 能否接收图片附件
    pub supports_image_input: bool,
    pub effort_levels: Vec<EffortLevel>,
    /// 这个平台上运行时有没有沙箱可用。只说平台能力，不代表某条会话的沙箱真的起来了（见
    /// [`AgentSession::sandbox`]）
    pub sandbox_platform: SandboxPlatform,
    /// 运行时报的可选模型；不报的运行时为空
    pub model_options: Vec<ModelOption>,
}

/// 平台上运行时有没有沙箱可用。运行时没报时按 `Unsupported`：不替它声称有沙箱。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SandboxPlatform {
    Supported,
    Unsupported,
}

/// 一条会话的沙箱实际起没起来。运行时没报时按 `Unavailable`。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SandboxStatus {
    Enforced,
    Unavailable,
}

/// 运行时报的一个可选模型。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ModelOption {
    pub id: ModelId,
    /// 给人看的名字
    pub label: String,
}

/// 会话要挂的 MCP 服务（只限用户导入过的，技术设计 9.5）。密钥由适配器按需向 SecretStore 取，不随这里传。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct McpServerRef {
    pub id: McpServerId,
    pub name: String,
}

/// 开一条会话所需的全部输入。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionSpec {
    pub cwd: PathBuf,
    /// 显式传入：配置隔离后不传就是运行时内置默认，不允许这种状态
    pub model: ModelSelection,
    pub system_prompt: String,
    pub mcp: Vec<McpServerRef>,
    pub constraints: SessionConstraints,
    pub tool_set: SessionToolSet,
    /// 启动时就不提供的工具：核心按会话工具集与永久禁用表算好（技术设计 7.1、7.2b），适配器原样转交
    pub disabled_tools: Vec<ToolRef>,
    /// 按会话加载的插件目录（导入区下的绝对路径，技术设计 9.5）
    pub plugin_dirs: Vec<PathBuf>,
    /// 续接的运行时会话 ID
    pub resume: Option<String>,
    pub kind: TurnKind,
}

/// prompt 的一块内容。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ContentBlock {
    Text(String),
    Attachment {
        artifact: ArtifactRef,
        mime: String,
        name: String,
    },
}

/// 工具调用的原始输入（运行时给的 JSON 文本），不透明；归一成动作是核心的事。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RawToolInput(pub String);

/// 回合怎么结束的。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TurnOutcome {
    Completed,
    Cancelled,
}

/// 会话里流出来的更新。
#[derive(Debug, Clone, PartialEq)]
pub enum AgentUpdate {
    /// 流式正文；`offset` 是这段文字在整条消息里的起始位置（UTF-8 字节），客户端据此去重
    Text {
        offset: u64,
        text: String,
    },
    ToolCallStarted {
        tool_call_id: ToolCallId,
        tool: ToolRef,
        raw_input: RawToolInput,
    },
    ToolCallFinished {
        tool_call_id: ToolCallId,
        ok: bool,
        output: String,
    },
    /// 运行时实际报告的模型，与请求的不一致时以它为准
    ModelReported(ModelSelection),
    Usage(Usage),
    /// 额度窗口的读数；观测时间由编排在收到时记
    Quota(Vec<QuotaReading>),
    Finished(TurnOutcome),
    Failed(AgentError),
}

/// 运行时报的一个额度窗口（不带观测时间）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct QuotaReading {
    /// 窗口类型，运行时给的不透明字符串
    pub window: String,
    /// 使用率百分比，0–100
    pub used_pct: u8,
    pub resets_at: Option<Timestamp>,
}

impl QuotaReading {
    /// 记上观测时间，变成领域里的额度窗口。观测时间由收到读数的一方（编排）按自己的时钟给。
    pub fn observed_at(self, at: Timestamp) -> QuotaWindow {
        QuotaWindow {
            window: self.window,
            used_pct: self.used_pct,
            resets_at: self.resets_at,
            observed_at: at,
        }
    }
}

/// 一次 prompt 的更新流。返回 `None` 表示流结束。
pub trait UpdateStream: Send {
    fn next(&mut self) -> impl Future<Output = Option<AgentUpdate>> + Send;
}

/// 运行时发来的权限请求。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PermissionRequest {
    pub tool: ToolRef,
    pub raw_input: RawToolInput,
    /// 运行时请求绕过沙箱
    pub bypass_sandbox: bool,
    /// 这次请求之前，适配器在本回合更新流里已交出的更新条数。编排处理完这么多条更新才受理
    /// 这次请求，「工具调用开始」因此一定排在「等待审批」之前，不管更新流里积压了多少
    pub updates_before: u64,
    /// 运行时报告的这次调用的工作目录；`None` = 运行时没给
    pub cwd: Option<PathBuf>,
}

/// 对权限请求的答复。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PermissionDecision {
    Allow,
    /// 放行，但用改写后的输入执行（如沙箱外执行）
    AllowRewritten {
        new_input: RawToolInput,
    },
    /// `message` 回给模型
    Deny {
        message: String,
    },
}

/// 权限请求的处理方：编排核心实现，适配器只转发、不判断。
pub trait PermissionHandler: Send + Sync {
    fn on_permission(
        &self,
        req: PermissionRequest,
    ) -> impl Future<Output = PermissionDecision> + Send;
}

/// 一条运行时会话。
pub trait AgentSession: Send + Sync {
    type Updates: UpdateStream;

    /// 运行时自己的会话 ID，续接用；会话还没建立时为 `None`。
    fn runtime_session_id(&self) -> Option<&str>;
    fn prompt(
        &self,
        blocks: Vec<ContentBlock>,
    ) -> impl Future<Output = Result<Self::Updates, AgentError>> + Send;
    /// 运行时等一个权限请求最多多久；`None` = 不限。编排必须在这之前给出结论或中断回合，
    /// 否则运行时可能按它自己的超时处理（Claude Code 的 hook 超时即放行）。
    fn permission_wait_limit(&self) -> Option<Duration>;
    /// 这条会话的沙箱实际起没起来。
    fn sandbox(&self) -> SandboxStatus;
    /// 中断当前回合；之后更新流以 `Finished(Cancelled)` 或结束收尾。回合已经自己结束时什么都不做。
    fn cancel(&self) -> impl Future<Output = Result<(), AgentError>> + Send;
    /// 结束这条会话名下的整棵进程树并确认清空；确认不了返回 `Err(ProcessExited)`。
    /// 不看有没有回合，只管树；可以重复调，已经清空就立刻返回 `Ok`。
    fn terminate(&self) -> impl Future<Output = Result<(), AgentError>> + Send;
    fn close(self) -> impl Future<Output = Result<(), AgentError>> + Send;
}

/// Agent 运行时。
pub trait AgentPort: Send + Sync {
    type Session: AgentSession;

    fn capabilities(&self) -> impl Future<Output = Result<RuntimeCapabilities, AgentError>> + Send;
    fn open_session<H: PermissionHandler + 'static>(
        &self,
        spec: SessionSpec,
        on_permission: H,
    ) -> impl Future<Output = Result<Self::Session, AgentError>> + Send;
}

/// 模型为什么不可用：三种原因要能区分（技术设计 9.6 第 7 条）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ModelUnavailableReason {
    NoPermission,
    Retired,
    Overloaded,
    Unknown,
}

/// 隔离失效是哪一种（T06 方案 3.5 的核对与 3.1b）：权限模式不是选定配置、多出来一个插件、插件清单读不了、
/// 自动记忆目录不是给出去的那个。运行时没说、或说了不认识的为 `Unknown`。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IsolationKind {
    PermissionMode,
    Plugin,
    PluginsUnreadable,
    MemoryPath,
    Unknown,
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum AgentError {
    #[error("找不到运行时程序")]
    RuntimeNotFound,
    #[error("运行时未登录")]
    NotLoggedIn,
    #[error("运行时版本不兼容：{version}")]
    Incompatible { version: String },
    #[error("模型 {model} 不可用：{reason:?}")]
    ModelUnavailable {
        model: String,
        reason: ModelUnavailableReason,
    },
    /// 这个运行时版本正在本机验证（T06 方案 10.1），过一会儿再试
    #[error("运行时 {version} 正在本机验证")]
    Verifying { version: String },
    /// 续接时运行时自己的会话记录用不了，三类分得开（需求第 12 章 v1.78）。只在带着旧会话 id 开会话时有意义：
    /// 编排据此用摘要新开一条会话线（T06 方案 5.2a），不靠原样重试解决
    #[error("运行时的会话记录用不了，无法续接：{reason:?}")]
    SessionLost { reason: SessionLostReason },
    /// 用户的配置进入了成员会话（隔离失效，T06 方案 3.5、2.6 第 7 条表下）：会话已停止。`plugin` 是多出来的那个
    /// 插件的名字，只有 `kind` 是 `Plugin` 时才可能有，形状卡死（字母、数字与 `. _ - @`，≤ 80 字符），不带路径。
    /// 显示文字固定，不带名字
    #[error("用户配置进入了会话，会话已停止")]
    IsolationBroken {
        kind: IsolationKind,
        plugin: Option<String>,
    },
    #[error("与运行时通信出错：{message}")]
    Protocol { message: String },
    #[error("运行时进程意外退出：{message}")]
    ProcessExited { message: String },
}

impl AgentError {
    /// 模型过载、进程意外退出、版本还在本机验证，值得原样重试；其余要用户先处理。
    pub fn retryable(&self) -> bool {
        matches!(
            self,
            Self::ModelUnavailable {
                reason: ModelUnavailableReason::Overloaded,
                ..
            } | Self::ProcessExited { .. }
                | Self::Verifying { .. }
        )
    }
}
