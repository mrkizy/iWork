//! Agent 运行时端口（技术设计 6.1）。后台服务只经它和运行时对话，厂商差异全在适配器里。

use std::future::Future;
use std::path::PathBuf;

use iwork_domain::action::ToolRef;
use iwork_domain::common::{ArtifactRef, EffortLevel, ModelSelection, Usage};
use iwork_domain::event::{McpServerId, ToolCallId};
use iwork_domain::runtime::QuotaWindow;
use iwork_domain::session::{SessionConstraints, SessionToolSet};
use iwork_domain::turn::TurnKind;

/// 运行时自报的能力。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RuntimeCapabilities {
    /// 能否按 `runtime_session_id` 续接会话
    pub supports_resume: bool,
    /// 能否接收图片附件
    pub supports_image_input: bool,
    pub effort_levels: Vec<EffortLevel>,
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
    Quota(Vec<QuotaWindow>),
    Finished(TurnOutcome),
    Failed(AgentError),
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
    /// 中断当前回合；之后更新流以 `Finished(Cancelled)` 或结束收尾。
    fn cancel(&self) -> impl Future<Output = Result<(), AgentError>> + Send;
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
    #[error("与运行时通信出错：{message}")]
    Protocol { message: String },
    #[error("运行时进程意外退出：{message}")]
    ProcessExited { message: String },
}

impl AgentError {
    /// 只有模型过载、进程意外退出值得原样重试；其余要用户先处理。
    pub fn retryable(&self) -> bool {
        matches!(
            self,
            Self::ModelUnavailable {
                reason: ModelUnavailableReason::Overloaded,
                ..
            } | Self::ProcessExited { .. }
        )
    }
}
