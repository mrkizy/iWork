//! `detect`：一个目录属于哪个仓库（技术设计 8.1、8.2）。

use std::path::{Path, PathBuf};

use gix::discover::upwards::Error as Upwards;
use iwork_ports::vcs::{Detection, RepoRoot, VcsError};

/// 找 `path` 所在的仓库。
///
/// - 子目录归到仓库根；链接的工作树的根是它自己，共享目录是主仓库的 `.git`，去重靠共享目录归到同一仓库；
///   子模块是独立仓库，根是子模块目录。
/// - 根与共享目录都取真实路径：解析符号链接，并在大小写不敏感的盘上（macOS、Windows）换成磁盘上的
///   真实写法。不做小写化：在大小写敏感的盘上那会把两个不同的目录当成一个。
/// - 裸仓库没有工作区，成员没法在里面改文件，报错而不是当成项目。
pub(crate) fn detect(path: &Path) -> Result<Detection, VcsError> {
    if !path.exists() {
        return Err(VcsError::PathMissing {
            path: path.to_owned(),
        });
    }
    let repo = match gix::discover(path) {
        Ok(repo) => repo,
        Err(e) if is_no_repository(&e) => return Ok(Detection::NotVersionControlled),
        Err(e) => return Err(failed(format!("{}：{e}", path.display()))),
    };
    let Some(workdir) = repo.workdir() else {
        return Err(failed(format!(
            "{} 是裸仓库，没有工作区，不能作为项目",
            repo.git_dir().display()
        )));
    };
    Ok(Detection::Repository {
        root: RepoRoot(real_path(workdir)?),
        shared_dir: real_path(repo.common_dir())?,
    })
}

/// 往上一直没找到仓库（不是其他故障，例如权限或仓库损坏）。
fn is_no_repository(e: &gix::Error) -> bool {
    matches!(
        e.downcast_any_ref::<Upwards>(),
        Some(
            Upwards::NoGitRepository { .. }
                | Upwards::NoGitRepositoryWithinCeiling { .. }
                | Upwards::NoGitRepositoryWithinFs { .. }
        )
    )
}

/// 解析符号链接与大小写后的真实路径。
pub(crate) fn real_path(path: &Path) -> Result<PathBuf, VcsError> {
    dunce::canonicalize(path).map_err(|e| failed(format!("{}：{e}", path.display())))
}

fn failed(message: String) -> VcsError {
    VcsError::Failed {
        operation: "detect",
        message,
    }
}
