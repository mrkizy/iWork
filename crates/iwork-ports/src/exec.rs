//! 以用户身份执行（技术设计 7.2a）：亲手执行卡片由用户点「执行」后走这里，不经 Agent、不进沙箱。

use std::future::Future;
use std::path::PathBuf;

use iwork_domain::action::CanonicalPath;
use iwork_domain::id::UserActionId;
use iwork_domain::user_action::ExitStatus;

/// 执行一条命令的请求。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UserExecRequest {
    pub card_id: UserActionId,
    pub command: String,
    pub cwd: CanonicalPath,
}

/// 应用一个补丁的请求。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UserPatchRequest {
    pub card_id: UserActionId,
    pub path: CanonicalPath,
    pub old: String,
    pub new: String,
}

/// 输出的接收方。输出只给用户，Agent 只得知是否执行与退出码。
pub trait OutputSink: Send + Sync {
    fn output(&self, chunk: &[u8]);
}

/// 执行后端。
pub trait ExecutionBackend: Send + Sync {
    type Handle: ExecutionHandle;

    /// 起进程并立即返回句柄；起不来返回 `SpawnFailed`（卡片转 `Failed`）。
    fn run_as_user<S: OutputSink + 'static>(
        &self,
        req: UserExecRequest,
        out: S,
    ) -> impl Future<Output = Result<Self::Handle, ExecError>> + Send;
    fn apply_patch_as_user(
        &self,
        patch: UserPatchRequest,
    ) -> impl Future<Output = Result<(), ExecError>> + Send;
}

/// 执行中进程的句柄。
///
/// 可克隆（内部共享），等待与停止可以同时进行：一个任务持有克隆在 `wait`，另一个克隆上调
/// `stop`，`wait` 随之返回。句柄只在本次后台服务进程里有效、不落盘；崩溃后没有句柄，卡片转
/// `Unknown` 由用户确认。不暴露进程号：没有用它的地方。
pub trait ExecutionHandle: Clone + Send + Sync {
    /// 幂等：已结束的进程上调用返回 `Ok`。先请求结束，3 秒后强制结束整个进程组。
    fn stop(&self) -> impl Future<Output = Result<(), ExecError>> + Send;
    /// 等进程结束。借用、不拿走句柄；可多次调用，结束后再调直接返回同一结果。
    fn wait(&self) -> impl Future<Output = Result<ExitStatus, ExecError>> + Send;
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ExecError {
    /// 明确知道没起来
    #[error("无法启动 {program}：{message}")]
    SpawnFailed { program: String, message: String },
    /// 拿不到进程状态：卡片转 `Unknown`
    #[error("等待进程失败：{message}")]
    WaitFailed { message: String },
    #[error("停止进程失败：{message}")]
    StopFailed { message: String },
    /// 文件内容已与补丁的 `old` 不符
    #[error("补丁无法应用到 {path}：内容已变")]
    PatchConflict { path: PathBuf },
    #[error("读写 {path} 失败：{message}")]
    Io { path: PathBuf, message: String },
}

impl ExecError {
    /// 一律不可重试：执行是用户的一次性动作，结果不确定时不静默重放（需求 12）。
    pub fn retryable(&self) -> bool {
        false
    }
}
