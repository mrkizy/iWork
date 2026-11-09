//! 版本控制端口（技术设计 8.1）。核心不写任何版本控制工具的特例，差异收在实现里。
//! 这里只有 M1 用到的方法；M2 的方法由需要它的卡加。

use std::future::Future;
use std::path::{Path, PathBuf};

/// 实现自报的能力。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct VcsCapabilities {
    /// 能否便宜地开独立工作区（M2 多成员时用）
    pub cheap_isolated_workspace: bool,
}

/// 仓库根目录。
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct RepoRoot(pub PathBuf);

/// 探测一个目录的结果。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Detection {
    NotVersionControlled,
    /// `shared_dir`：仓库共享目录的真实路径，项目去重按它判断（技术设计 8.2）
    Repository {
        root: RepoRoot,
        shared_dir: PathBuf,
    },
}

/// 基线：比较改动的起点，内容不透明。
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct BaselineRef(pub String);

/// 一个文件的改动种类。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ChangeKind {
    Added,
    Modified,
    Deleted,
    Renamed,
    Untracked,
}

/// 工作区里改动了的一个文件。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChangedPath {
    pub path: PathBuf,
    pub change: ChangeKind,
}

/// 工作区状态。
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct WorkspaceStatus {
    pub changed: Vec<ChangedPath>,
}

/// 比较范围。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DiffRange {
    /// 工作区相对某个基线
    WorkspaceSince(BaselineRef),
    Between {
        from: BaselineRef,
        to: BaselineRef,
    },
}

/// 一个文件的差异。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileDiff {
    pub path: PathBuf,
    pub insertions: u32,
    pub deletions: u32,
    /// 统一差异格式文本
    pub patch: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Diff {
    pub files: Vec<FileDiff>,
}

/// 克隆进度。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CloneProgress {
    pub phase: String,
    /// 0–100
    pub percent: u8,
    pub text: String,
}

/// 克隆进度的接收方（推成不落盘的 `CloneProgress` 临时事件）。
pub trait ProgressSink: Send + Sync {
    fn progress(&self, progress: CloneProgress);
}

/// 版本控制。
pub trait VcsProvider: Send + Sync {
    fn capabilities(&self) -> VcsCapabilities;
    fn detect(&self, path: &Path) -> impl Future<Output = Result<Detection, VcsError>> + Send;
    fn init(&self, path: &Path) -> impl Future<Output = Result<(), VcsError>> + Send;
    /// 不叫 `clone`：实现类型若同时实现 `Clone`，`x.clone(..)` 会撞名。
    fn clone_repository<S: ProgressSink + 'static>(
        &self,
        url: &str,
        dest: &Path,
        progress: S,
    ) -> impl Future<Output = Result<(), VcsError>> + Send;
    fn baseline(
        &self,
        repo: &RepoRoot,
    ) -> impl Future<Output = Result<BaselineRef, VcsError>> + Send;
    fn status(
        &self,
        repo: &RepoRoot,
    ) -> impl Future<Output = Result<WorkspaceStatus, VcsError>> + Send;
    fn diff(
        &self,
        repo: &RepoRoot,
        range: DiffRange,
    ) -> impl Future<Output = Result<Diff, VcsError>> + Send;
}

/// 版本控制错误。克隆地址可能带凭据，所以错误里一律不放地址。
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum VcsError {
    #[error("路径不存在：{path}")]
    PathMissing { path: PathBuf },
    #[error("不在版本控制下：{path}")]
    NotVersionControlled { path: PathBuf },
    #[error("目标目录不为空：{path}")]
    TargetNotEmpty { path: PathBuf },
    #[error("没有配置提交身份")]
    IdentityMissing,
    #[error("克隆失败：{message}")]
    CloneFailed { message: String },
    #[error("版本控制操作 {operation} 失败：{message}")]
    Failed {
        operation: &'static str,
        message: String,
    },
}

impl VcsError {
    /// 只有克隆失败（多半是网络）值得重试。
    pub fn retryable(&self) -> bool {
        matches!(self, Self::CloneFailed { .. })
    }
}
