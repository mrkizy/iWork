//! 读工作区：`baseline`、`status`、`diff`（gix，技术设计 8.1）。

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

use gix::bstr::{BStr, BString};
use gix::status::index_worktree::Item as WorktreeItem;
use gix::status::{Item, UntrackedFiles};
use gix::{ObjectId, Repository};
use iwork_ports::vcs::{
    BaselineRef, ChangeKind, ChangedPath, Diff, DiffRange, RepoRoot, VcsError, WorkspaceStatus,
};

use crate::patch::file_diff;

/// 当前基线：HEAD 指向的提交。还没有任何提交时用空树，这样相对它的差异就是全部文件。
pub(crate) fn baseline(repo_root: &RepoRoot) -> Result<BaselineRef, VcsError> {
    let repo = open(repo_root, "baseline")?;
    let id = match repo.head_id() {
        Ok(id) => id.detach(),
        Err(_) if head_is_unborn(&repo, "baseline")? => repo.empty_tree().id,
        Err(e) => return Err(failed("baseline", format!("读 HEAD 失败：{e}"))),
    };
    Ok(BaselineRef(id.to_string()))
}

/// 工作区相对 HEAD 的改动，暂存区与工作区合成一个结果，每个路径一条。
pub(crate) fn status(repo_root: &RepoRoot) -> Result<WorkspaceStatus, VcsError> {
    const OP: &str = "status";
    let repo = open(repo_root, OP)?;
    let mut by_path: BTreeMap<BString, (Option<ChangeKind>, Option<ChangeKind>)> = BTreeMap::new();
    for item in status_items(&repo, None, OP)? {
        match item {
            Item::TreeIndex(change) => {
                use gix::diff::index::Change as C;
                let kind = match &change {
                    C::Addition { .. } => ChangeKind::Added,
                    C::Deletion { .. } => ChangeKind::Deleted,
                    C::Modification { .. } => ChangeKind::Modified,
                    C::Rewrite { .. } => ChangeKind::Renamed,
                };
                by_path.entry(change.location().to_owned()).or_default().0 = Some(kind);
            }
            Item::IndexWorktree(item) => {
                let path = item.rela_path().to_owned();
                if let Some(kind) = worktree_kind(&item) {
                    by_path.entry(path).or_default().1 = Some(kind);
                }
            }
        }
    }
    let changed = by_path
        .into_iter()
        .filter_map(|(path, (index, worktree))| {
            combine(index, worktree).map(|change| ChangedPath {
                path: to_path(path.as_ref()),
                change,
            })
        })
        .collect();
    Ok(WorkspaceStatus { changed })
}

/// 按范围出差异。
///
/// 工作区一侧读磁盘上的原始字节，不过 git 的内容过滤（换行转换、LFS 等）。
pub(crate) fn diff(repo_root: &RepoRoot, range: &DiffRange) -> Result<Diff, VcsError> {
    const OP: &str = "diff";
    let repo = open(repo_root, OP)?;
    let mut files = Vec::new();
    match range {
        DiffRange::WorkspaceSince(base) => {
            let base_tree = tree_of(&repo, base, OP)?;
            let tree = repo
                .find_tree(base_tree)
                .map_err(|e| failed(OP, format!("读基线树失败：{e}")))?;
            // 以基线树作比较起点跑一遍 status，得到可能变了的路径；逐个比内容
            let mut candidates = BTreeSet::new();
            for item in status_items(&repo, Some(base_tree), OP)? {
                candidates.insert(item.location().to_owned());
            }
            let workdir = repo_root.0.as_path();
            for path in candidates {
                let rel = to_path(path.as_ref());
                let old = match tree
                    .lookup_entry_by_path(&rel)
                    .map_err(|e| failed(OP, format!("{}：{e}", rel.display())))?
                {
                    Some(entry) if entry.mode().is_blob_or_symlink() => {
                        Some(blob(&repo, entry.object_id(), OP)?)
                    }
                    _ => None,
                };
                let new = read_worktree_file(&workdir.join(&rel), OP)?;
                files.extend(file_diff(rel, old.as_deref(), new.as_deref()));
            }
        }
        DiffRange::Between { from, to } => {
            let from = repo
                .find_tree(tree_of(&repo, from, OP)?)
                .map_err(|e| failed(OP, e.to_string()))?;
            let to = repo
                .find_tree(tree_of(&repo, to, OP)?)
                .map_err(|e| failed(OP, e.to_string()))?;
            let changes = repo
                .diff_tree_to_tree(&from, &to, gix::diff::Options::default())
                .map_err(|e| failed(OP, format!("比较两棵树失败：{e}")))?;
            for change in changes {
                use gix::object::tree::diff::ChangeDetached as C;
                let (old, new) = match &change {
                    C::Addition { entry_mode, id, .. } => {
                        (None, entry_mode.is_blob_or_symlink().then_some(*id))
                    }
                    C::Deletion { entry_mode, id, .. } => {
                        (entry_mode.is_blob_or_symlink().then_some(*id), None)
                    }
                    C::Modification {
                        previous_entry_mode,
                        previous_id,
                        entry_mode,
                        id,
                        ..
                    } => (
                        previous_entry_mode
                            .is_blob_or_symlink()
                            .then_some(*previous_id),
                        entry_mode.is_blob_or_symlink().then_some(*id),
                    ),
                    // 没开改名跟踪，不会出现
                    C::Rewrite { .. } => continue,
                };
                let old = old.map(|id| blob(&repo, id, OP)).transpose()?;
                let new = new.map(|id| blob(&repo, id, OP)).transpose()?;
                files.extend(file_diff(
                    to_path(change.location()),
                    old.as_deref(),
                    new.as_deref(),
                ));
            }
        }
    }
    Ok(Diff { files })
}

/// 同一路径上暂存区（相对 HEAD）与工作区（相对暂存区）的改动合成一个。
///
/// 暂存区的种类优先（它才是相对 HEAD 的）；只是工作区里又删掉了的，按删除算；
/// 新加进暂存区、工作区里又删掉的，相对 HEAD 等于没变。
fn combine(index: Option<ChangeKind>, worktree: Option<ChangeKind>) -> Option<ChangeKind> {
    match (index, worktree) {
        (Some(ChangeKind::Added), Some(ChangeKind::Deleted)) => None,
        (Some(_), Some(ChangeKind::Deleted)) => Some(ChangeKind::Deleted),
        (Some(kind), _) => Some(kind),
        (None, worktree) => worktree,
    }
}

fn worktree_kind(item: &WorktreeItem) -> Option<ChangeKind> {
    use gix::status::index_worktree::iter::Summary;
    match item.summary()? {
        Summary::Removed => Some(ChangeKind::Deleted),
        Summary::Added => Some(ChangeKind::Untracked),
        Summary::Renamed | Summary::Copied => Some(ChangeKind::Renamed),
        Summary::Modified | Summary::TypeChange | Summary::Conflict => Some(ChangeKind::Modified),
        Summary::IntentToAdd => Some(ChangeKind::Added),
    }
}

fn status_items(
    repo: &Repository,
    head_tree: Option<ObjectId>,
    op: &'static str,
) -> Result<Vec<Item>, VcsError> {
    let mut platform = repo
        .status(gix::progress::Discard)
        .map_err(|e| failed(op, e.to_string()))?
        .untracked_files(UntrackedFiles::Files)
        .index_worktree_rewrites(None);
    if let Some(tree) = head_tree {
        platform = platform.head_tree(tree);
    }
    platform
        .into_iter(None)
        .map_err(|e| failed(op, e.to_string()))?
        .map(|item| item.map_err(|e| failed(op, e.to_string())))
        .collect()
}

fn open(repo_root: &RepoRoot, op: &'static str) -> Result<Repository, VcsError> {
    if !repo_root.0.exists() {
        return Err(VcsError::PathMissing {
            path: repo_root.0.clone(),
        });
    }
    gix::open(&repo_root.0).map_err(|e| failed(op, format!("{}：{e}", repo_root.0.display())))
}

fn head_is_unborn(repo: &Repository, op: &'static str) -> Result<bool, VcsError> {
    let head = repo
        .head()
        .map_err(|e| failed(op, format!("读 HEAD 失败：{e}")))?;
    Ok(head.is_unborn())
}

/// 基线 → 树：提交取它的树，树就是它自己（空仓库的基线是空树）。
fn tree_of(repo: &Repository, base: &BaselineRef, op: &'static str) -> Result<ObjectId, VcsError> {
    let id = ObjectId::from_hex(base.0.as_bytes())
        .map_err(|e| failed(op, format!("基线 {} 格式不对：{e}", base.0)))?;
    if id == repo.empty_tree().id {
        return Ok(id);
    }
    let tree = repo
        .find_object(id)
        .and_then(|o| o.peel_to_tree())
        .map_err(|e| failed(op, format!("基线 {} 找不到对应的树：{e}", base.0)))?;
    Ok(tree.id)
}

fn blob(repo: &Repository, id: ObjectId, op: &'static str) -> Result<Vec<u8>, VcsError> {
    repo.find_blob(id)
        .map(|mut b| b.take_data())
        .map_err(|e| failed(op, format!("读对象 {id} 失败：{e}")))
}

/// 工作区里的文件内容；不存在或是目录（如子模块）时为 `None`，符号链接取链接目标本身。
fn read_worktree_file(path: &Path, op: &'static str) -> Result<Option<Vec<u8>>, VcsError> {
    let meta = match std::fs::symlink_metadata(path) {
        Ok(m) => m,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(failed(op, format!("{}：{e}", path.display()))),
    };
    let read = if meta.file_type().is_symlink() {
        std::fs::read_link(path).map(|t| t.to_string_lossy().into_owned().into_bytes())
    } else if meta.is_file() {
        std::fs::read(path)
    } else {
        return Ok(None);
    };
    read.map(Some)
        .map_err(|e| failed(op, format!("{}：{e}", path.display())))
}

/// 仓库里的相对路径 → 本机路径。git 的路径是字节串，非 UTF-8 的文件名在 Windows 上转不过来时按有损转换。
fn to_path(rela: &BStr) -> PathBuf {
    gix::path::from_bstr(rela).map_or_else(
        |_| PathBuf::from(String::from_utf8_lossy(rela).into_owned()),
        std::borrow::Cow::into_owned,
    )
}

fn failed(operation: &'static str, message: String) -> VcsError {
    VcsError::Failed { operation, message }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ChangeKind::*;

    #[test]
    fn combine_prefers_index_and_handles_later_deletion() {
        assert_eq!(combine(None, Some(Untracked)), Some(Untracked));
        assert_eq!(combine(Some(Modified), Some(Modified)), Some(Modified));
        assert_eq!(combine(Some(Added), Some(Modified)), Some(Added));
        assert_eq!(combine(Some(Modified), Some(Deleted)), Some(Deleted));
        assert_eq!(combine(Some(Added), Some(Deleted)), None);
        assert_eq!(combine(None, None), None);
    }
}
