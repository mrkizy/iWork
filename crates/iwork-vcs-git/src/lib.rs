//! 基础设施层：`VcsProvider` 的 git 实现（技术设计第 8 节）。
//!
//! 读操作（`detect`、`baseline`、`status`、`diff`）在进程内用 gitoxide 求快，放到阻塞线程池里跑；
//! 写操作（`init`、`clone`）调本机 `git` 求正确，沿用用户的 hooks、凭据助手与配置。
//! 本 crate 不读、不存、不转发任何凭据。

mod clone;
mod command;
mod detect;
mod init;
mod patch;
mod progress;
mod workspace;

use std::path::{Path, PathBuf};

use iwork_ports::vcs::{
    BaselineRef, Detection, Diff, DiffRange, ProgressSink, RepoRoot, VcsCapabilities, VcsError,
    VcsProvider, WorkspaceStatus,
};

pub use command::GitCommand;

/// git 的 `VcsProvider`。
#[derive(Debug, Clone, Default)]
pub struct GitVcs {
    git: GitCommand,
}

impl GitVcs {
    /// 用指定的 git 启动方式；生产用 [`GitVcs::default`]。
    pub fn with_command(git: GitCommand) -> Self {
        Self { git }
    }
}

/// 把阻塞的 gix 读操作放到阻塞线程池。
async fn blocking<T: Send + 'static>(
    operation: &'static str,
    f: impl FnOnce() -> Result<T, VcsError> + Send + 'static,
) -> Result<T, VcsError> {
    tokio::task::spawn_blocking(f)
        .await
        .map_err(|e| VcsError::Failed {
            operation,
            message: format!("后台线程异常结束：{e}"),
        })?
}

impl VcsProvider for GitVcs {
    /// git worktree 能便宜地开独立工作区（M2 用；M1 只声明，不实现）。
    fn capabilities(&self) -> VcsCapabilities {
        VcsCapabilities {
            cheap_isolated_workspace: true,
        }
    }

    async fn detect(&self, path: &Path) -> Result<Detection, VcsError> {
        let path = path.to_owned();
        blocking("detect", move || detect::detect(&path)).await
    }

    async fn init(&self, path: &Path) -> Result<(), VcsError> {
        let git = self.git.clone();
        let path: PathBuf = path.to_owned();
        init::init(&git, &path).await
    }

    async fn clone_repository<S: ProgressSink + 'static>(
        &self,
        url: &str,
        dest: &Path,
        progress: S,
    ) -> Result<(), VcsError> {
        clone::clone(&self.git, url, dest, &progress).await
    }

    async fn baseline(&self, repo: &RepoRoot) -> Result<BaselineRef, VcsError> {
        let repo = repo.clone();
        blocking("baseline", move || workspace::baseline(&repo)).await
    }

    async fn status(&self, repo: &RepoRoot) -> Result<WorkspaceStatus, VcsError> {
        let repo = repo.clone();
        blocking("status", move || workspace::status(&repo)).await
    }

    async fn diff(&self, repo: &RepoRoot, range: DiffRange) -> Result<Diff, VcsError> {
        let repo = repo.clone();
        blocking("diff", move || workspace::diff(&repo, &range)).await
    }
}
