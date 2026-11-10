//! `init`：`git init` + `git add -A` + 首次提交（技术设计 8.1、9.3）。

use std::path::Path;

use iwork_ports::vcs::{Detection, VcsError};

use crate::command::GitCommand;
use crate::detect::detect;

const OPERATION: &str = "init";
const INITIAL_COMMIT_MESSAGE: &str = "Initial commit";

/// git 在拿不到提交身份时的输出（`LC_ALL=C` 下）。
const IDENTITY_UNKNOWN_MARKERS: &[&str] = &["identity unknown", "Please tell me who you are"];

/// 在 `path` 初始化仓库并做首次提交。
///
/// - 只接受不在任何仓库里的已有目录：已在仓库里（包括是某个仓库的子目录）时 `git add -A` 会把用户
///   没提交的改动一起提交掉，所以拒绝。
/// - 提交沿用用户的 git 配置；加 `user.useConfigOnly=true` 不让 git 拿主机名猜一个邮箱，没配置身份时
///   返回 [`VcsError::IdentityMissing`]。
/// - 一律带 `--allow-empty`：空目录（或只有被忽略的文件）也要有首次提交。
/// - `git init` 之后任何一步失败，都删掉这次建的 `.git` 再返回错误，目录回到调用前的样子，可以直接重试。
pub(crate) async fn init(git: &GitCommand, path: &Path) -> Result<(), VcsError> {
    match detect(path)? {
        Detection::NotVersionControlled => {}
        Detection::Repository { root, .. } => {
            return Err(failed(format!(
                "{} 已在仓库 {} 里，不能再初始化",
                path.display(),
                root.0.display()
            )));
        }
    }
    let dot_git = path.join(".git");
    if dot_git.exists() {
        return Err(failed(format!(
            "{} 已存在但不是有效仓库，请先处理它",
            dot_git.display()
        )));
    }

    run(git, path, &["init", "--quiet"]).await?;
    let rest = async {
        run(git, path, &["add", "--all"]).await?;
        run(
            git,
            path,
            &[
                "-c",
                "user.useConfigOnly=true",
                "commit",
                "--quiet",
                "--allow-empty",
                "--message",
                INITIAL_COMMIT_MESSAGE,
            ],
        )
        .await
    }
    .await;
    if let Err(e) = rest {
        // `.git` 是上面这次 `git init` 建的（之前已确认它不存在），删掉才能让用户配好身份后直接重试
        if let Err(cleanup) = std::fs::remove_dir_all(&dot_git) {
            return Err(failed(format!(
                "{e}；清理 {} 也失败：{cleanup}",
                dot_git.display()
            )));
        }
        return Err(e);
    }
    Ok(())
}

async fn run(git: &GitCommand, path: &Path, args: &[&str]) -> Result<(), VcsError> {
    let finished = git.run(path, args).await.map_err(failed)?;
    if finished.success {
        return Ok(());
    }
    if IDENTITY_UNKNOWN_MARKERS
        .iter()
        .any(|m| finished.stderr.contains(m))
    {
        return Err(VcsError::IdentityMissing);
    }
    Err(failed(format!(
        "git {} 失败：{}",
        args.join(" "),
        finished.stderr.trim()
    )))
}

fn failed(message: String) -> VcsError {
    VcsError::Failed {
        operation: OPERATION,
        message,
    }
}
