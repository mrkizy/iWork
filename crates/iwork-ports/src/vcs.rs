//! 版本控制端口（技术设计 8.1）。核心不写任何版本控制工具的特例，差异收在实现里。
//! 这里只有 M1 用到的方法；M2 的方法由需要它的卡加。

use std::future::Future;
use std::path::{Path, PathBuf};
use std::pin::Pin;

use iwork_domain::event::ChangeSummary;

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

/// 一个路径在工作区里此刻的版本（T10a 方案 D10）。两个版本相等就当这个路径没变，口径和 `diff` 判两版相同
/// 一致（要外部过滤程序的文件除外：不运行过滤程序，比的是文件原样的字节）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PathVersion {
    /// 工作区里没有它（含：上级目录是链接，不跟过去；设备、管道这类不算版本控制里的东西）
    Absent,
    /// 种类与执行位、内容的散列。内容：普通文件是文件字节，链接是目标文字，子模块引用是检出的提交
    Present { kind: PathKind, digest: [u8; 32] },
    /// 给不出可比的版本（超过上限不读全、读失败）
    Unknown,
}

/// 版本控制认得的条目种类。执行位变了也算变了。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum PathKind {
    File,
    ExecutableFile,
    Symlink,
    Submodule,
}

/// 一趟回合编排自己的改动探针（T10a 方案 D10）：开始时的样子已经拍在里面，收尾时问它这一趟期间变了什么。
/// 编排不知道版本控制：它只拿到一个摘要或者没有。
pub trait ChangeProbe: Send + Sync {
    /// 这一趟期间的改动摘要；给不出来（版本控制报错、读数额度用完）返回 `None`。调用方限时等，到时丢掉
    /// 这个 future，所以实现不能把要收尾的工作挂在它上面。
    fn summary(&self) -> Pin<Box<dyn Future<Output = Option<ChangeSummary>> + Send + '_>>;
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
    /// 这些路径（相对仓库根）在工作区里此刻的版本，按传入的次序一一对应（T10a 方案 D10）。不运行任何外部程序，
    /// 不跟随路径上任何一级目录链接：上级目录是链接的路径算 [`PathVersion::Absent`]。单个路径读不了记
    /// [`PathVersion::Unknown`]；整体出错（仓库打不开）才返回错误。
    fn worktree_versions(
        &self,
        repo: &RepoRoot,
        paths: &[PathBuf],
    ) -> impl Future<Output = Result<Vec<PathVersion>, VcsError>> + Send;
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
