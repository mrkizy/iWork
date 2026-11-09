//! 项目身份文件与新建项目目录（技术设计 8.2、9.3；ADR-0006）。
//!
//! 与版本控制无关，所以不放在 `VcsProvider` 里：换一种版本控制工具，项目身份规则不变。

use std::future::Future;
use std::path::{Path, PathBuf};

use iwork_domain::id::ProjectId;

/// 仓库里的项目标记 `.iwork/project.toml`：首次添加时写入项目 ID，此后靠它识别项目，而不是路径。
pub trait ProjectMarkerStore: Send + Sync {
    /// 读 `root` 下的项目标记。文件不存在返回 `Ok(None)`；文件在但读不出项目 ID 是错误。
    fn read(
        &self,
        root: &Path,
    ) -> impl Future<Output = Result<Option<ProjectId>, ProjectFilesError>> + Send;

    /// 在 `root` 下新建项目标记（需要时连 `.iwork/` 一起建）。
    /// 文件已存在时返回 [`ProjectFilesError::MarkerExists`]，绝不覆盖：别处登记过的 ID 不能被悄悄换掉。
    ///
    /// 项目 ID 只属于本机这份工作副本、不随仓库共享（技术设计 8.2）：实现要同时让版本控制忽略这个
    /// 标记（git 实现写 `.iwork/.gitignore`），原有忽略规则保留，重复写不重复追加。
    fn write(
        &self,
        root: &Path,
        id: ProjectId,
    ) -> impl Future<Output = Result<(), ProjectFilesError>> + Send;
}

/// 新建项目时准备目录（需求 FR19）。
pub trait ProjectDirectories: Send + Sync {
    /// 在 `parent` 下准备名为 `name` 的空目录并返回它的路径：父目录不存在就建；目标不存在就建，
    /// 已存在且为空就用它；已存在且非空（或是文件）返回 [`ProjectFilesError::TargetNotEmpty`]，不动它。
    /// `name` 必须是单独一级目录名（不含路径分隔符，不是 `.`、`..`），否则返回
    /// [`ProjectFilesError::InvalidName`]。
    fn create_project_dir(
        &self,
        parent: &Path,
        name: &str,
    ) -> impl Future<Output = Result<PathBuf, ProjectFilesError>> + Send;
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ProjectFilesError {
    #[error("项目标记 {path} 读不出项目 ID：{message}")]
    MarkerMalformed { path: PathBuf, message: String },
    #[error("项目标记 {path} 已存在，不覆盖")]
    MarkerExists { path: PathBuf },
    #[error("目标目录不为空：{path}")]
    TargetNotEmpty { path: PathBuf },
    #[error("项目名 {name:?} 不能用作目录名：{reason}")]
    InvalidName { name: String, reason: String },
    /// 项目标记目录、标记文件或忽略文件是符号链接：读写会被带出项目根，一律拒绝
    #[error("{path} 是符号链接（或经过符号链接），拒绝读写")]
    SymlinkRefused { path: PathBuf },
    #[error("读写 {path} 失败：{message}")]
    Io { path: PathBuf, message: String },
}

impl ProjectFilesError {
    /// 都是要用户处理的状况，原样重试不会变好。
    pub fn retryable(&self) -> bool {
        false
    }
}
