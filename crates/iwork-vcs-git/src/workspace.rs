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

use crate::patch::{EntryMode, Side, file_diff};

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
    for item in status_items(&repo, None, Renames::AsConfigured, OP)? {
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
/// 两侧都换成 git 内部的表示再比较：工作区的普通文件过 gix 的内容过滤（换行转换、编码、配置的
/// clean 过滤器，与 `git diff` 一致；过滤器失败时报错，不生成错误的差异）。工作区一侧读文件时路径上
/// 任何一级都不跟随符号链接（`iwork-rootfs`）。不做改名检测：改名就是源路径删除加目标路径新增。
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
            // 以基线树作比较起点跑一遍 status，得到可能变了的路径；逐个比
            let mut candidates = BTreeSet::new();
            for item in status_items(&repo, Some(base_tree), Renames::Off, OP)? {
                if let Item::TreeIndex(gix::diff::index::Change::Rewrite {
                    source_location, ..
                }) = &item
                {
                    candidates.insert(source_location.clone().into_owned());
                }
                candidates.insert(item.location().to_owned());
            }
            let mut worktree = Worktree::new(&repo, repo_root, OP)?;
            for path in candidates {
                let rel = to_path(path.as_ref());
                let old = match tree
                    .lookup_entry_by_path(&rel)
                    .map_err(|e| failed(OP, format!("{}：{e}", rel.display())))?
                {
                    Some(entry) => tree_side(&repo, entry.mode().kind(), entry.object_id(), OP)?,
                    None => None,
                };
                let new = worktree.side(&rel, old.as_ref())?;
                files.extend(file_diff(rel, old.as_ref(), new.as_ref()));
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
                .diff_tree_to_tree(
                    &from,
                    &to,
                    gix::diff::Options::default().with_rewrites(None),
                )
                .map_err(|e| failed(OP, format!("比较两棵树失败：{e}")))?;
            for change in changes {
                use gix::object::tree::diff::ChangeDetached as C;
                // 关了改名检测，不会有 Rewrite；万一有，也拆成源路径删除加目标路径新增
                let pairs = match change {
                    C::Addition {
                        location,
                        entry_mode,
                        id,
                        ..
                    } => {
                        vec![(location, None, Some((entry_mode, id)))]
                    }
                    C::Deletion {
                        location,
                        entry_mode,
                        id,
                        ..
                    } => {
                        vec![(location, Some((entry_mode, id)), None)]
                    }
                    C::Modification {
                        location,
                        previous_entry_mode,
                        previous_id,
                        entry_mode,
                        id,
                    } => vec![(
                        location,
                        Some((previous_entry_mode, previous_id)),
                        Some((entry_mode, id)),
                    )],
                    C::Rewrite {
                        source_location,
                        source_entry_mode,
                        source_id,
                        location,
                        entry_mode,
                        id,
                        ..
                    } => vec![
                        (source_location, Some((source_entry_mode, source_id)), None),
                        (location, None, Some((entry_mode, id))),
                    ],
                };
                for (location, old, new) in pairs {
                    let side = |e: Option<(gix::object::tree::EntryMode, ObjectId)>| {
                        e.map_or(Ok(None), |(mode, id)| tree_side(&repo, mode.kind(), id, OP))
                    };
                    let (old, new) = (side(old)?, side(new)?);
                    files.extend(file_diff(
                        to_path(location.as_ref()),
                        old.as_ref(),
                        new.as_ref(),
                    ));
                }
            }
        }
    }
    Ok(Diff { files })
}

/// 树里一个条目在差异里的样子；目录返回 `None`（它的内容按各自的路径比较）。
fn tree_side(
    repo: &Repository,
    kind: gix::object::tree::EntryKind,
    id: ObjectId,
    op: &'static str,
) -> Result<Option<Side>, VcsError> {
    let Some(mode) = EntryMode::from_tree(kind) else {
        return Ok(None);
    };
    Ok(Some(match mode {
        EntryMode::Gitlink => Side::gitlink(id),
        mode => Side {
            mode,
            data: blob(repo, id, op)?,
        },
    }))
}

/// 读工作区一侧：不跟随链接，普通文件过内容过滤。
struct Worktree<'repo> {
    root: &'repo Path,
    filter: gix::filter::Pipeline<'repo>,
    index: gix::worktree::IndexPersistedOrInMemory,
    caps: gix::fs::Capabilities,
    op: &'static str,
}

impl<'repo> Worktree<'repo> {
    fn new(
        repo: &'repo Repository,
        root: &'repo RepoRoot,
        op: &'static str,
    ) -> Result<Self, VcsError> {
        let (filter, index) = repo
            .filter_pipeline(None)
            .map_err(|e| failed(op, format!("准备内容过滤失败：{e}")))?;
        let caps = repo
            .filesystem_options()
            .map_err(|e| failed(op, format!("读文件系统配置失败：{e}")))?;
        Ok(Self {
            root: root.0.as_path(),
            filter,
            index,
            caps,
            op,
        })
    }

    /// `rel` 在工作区里的样子；`old` 是基线一侧，用来在不认执行位的文件系统上沿用原 mode、
    /// 判断目录是不是子模块。
    fn side(&mut self, rel: &Path, old: Option<&Side>) -> Result<Option<Side>, VcsError> {
        let op = self.op;
        let kind = match iwork_rootfs::inspect(self.root, rel) {
            Ok(kind) => kind,
            // 上级目录被换成了链接：按 git 的看法，这个路径在工作区里已经不存在（链接本身另算一个路径）
            Err(iwork_rootfs::RootedError::ThroughSymlink { .. }) => return Ok(None),
            Err(e) => return Err(failed(op, e.to_string())),
        };
        match kind {
            iwork_rootfs::EntryKind::File { executable } => {
                let raw = iwork_rootfs::read_file(self.root, rel)
                    .map_err(|e| failed(op, e.to_string()))?
                    .unwrap_or_default();
                let mode = if self.caps.executable_bit {
                    if executable {
                        EntryMode::Executable
                    } else {
                        EntryMode::Regular
                    }
                } else {
                    match old.map(|s| s.mode) {
                        Some(m @ (EntryMode::Regular | EntryMode::Executable)) => m,
                        _ => EntryMode::Regular,
                    }
                };
                Ok(Some(Side {
                    mode,
                    data: self.convert_to_git(rel, &raw)?,
                }))
            }
            iwork_rootfs::EntryKind::Symlink => {
                let target = iwork_rootfs::read_link(self.root, rel)
                    .map_err(|e| failed(op, e.to_string()))?;
                Ok(Some(Side {
                    mode: EntryMode::Symlink,
                    data: gix::path::into_bstr(target)
                        .map_err(|e| {
                            failed(op, format!("{} 的链接目标无法表示：{e}", rel.display()))
                        })?
                        .into_owned()
                        .into(),
                }))
            }
            iwork_rootfs::EntryKind::Dir
                if old.is_some_and(|s| s.mode == EntryMode::Gitlink)
                    || self.is_gitlink_in_index(rel) =>
            {
                self.submodule_head(rel).map(|id| id.map(Side::gitlink))
            }
            iwork_rootfs::EntryKind::Dir
            | iwork_rootfs::EntryKind::Missing
            | iwork_rootfs::EntryKind::Other => Ok(None),
        }
    }

    fn convert_to_git(&mut self, rel: &Path, raw: &[u8]) -> Result<Vec<u8>, VcsError> {
        use std::io::Read;
        let op = self.op;
        let outcome = self
            .filter
            .convert_to_git(raw, rel, &self.index)
            .map_err(|e| failed(op, format!("{} 内容过滤失败：{e}", rel.display())))?;
        let mut out = Vec::new();
        match outcome {
            gix::filter::plumbing::pipeline::convert::ToGitOutcome::Unchanged(mut r) => {
                r.read_to_end(&mut out)
            }
            gix::filter::plumbing::pipeline::convert::ToGitOutcome::Process(mut r) => {
                r.read_to_end(&mut out)
            }
            gix::filter::plumbing::pipeline::convert::ToGitOutcome::Buffer(b) => {
                out.extend_from_slice(b);
                Ok(b.len())
            }
        }
        .map_err(|e| failed(op, format!("{} 内容过滤失败：{e}", rel.display())))?;
        Ok(out)
    }

    fn is_gitlink_in_index(&self, rel: &Path) -> bool {
        let Ok(key) = gix::path::into_bstr(rel) else {
            // 转不成 git 路径的名字不可能在暂存区里
            return false;
        };
        let key = gix::path::to_unix_separators_on_windows(key);
        self.index
            .entry_by_path(key.as_ref())
            .is_some_and(|e| e.mode == gix::index::entry::Mode::COMMIT)
    }

    /// 子模块当前检出的提交；子模块还没初始化（空目录）时为 `None`。
    fn submodule_head(&self, rel: &Path) -> Result<Option<ObjectId>, VcsError> {
        let op = self.op;
        let dir = self.root.join(rel);
        let sub = match gix::open(&dir) {
            Ok(sub) => sub,
            Err(_)
                if std::fs::read_dir(&dir)
                    .map(|mut d| d.next().is_none())
                    .unwrap_or(false) =>
            {
                return Ok(None);
            }
            Err(e) => {
                return Err(failed(
                    op,
                    format!("打开子模块 {} 失败：{e}", rel.display()),
                ));
            }
        };
        sub.head_id()
            .map(|id| Some(id.detach()))
            .map_err(|e| failed(op, format!("读子模块 {} 的 HEAD 失败：{e}", rel.display())))
    }
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

/// 是否做改名检测：`status` 要报改名，`diff` 要把改名拆成删除加新增。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Renames {
    AsConfigured,
    Off,
}

fn status_items(
    repo: &Repository,
    head_tree: Option<ObjectId>,
    renames: Renames,
    op: &'static str,
) -> Result<Vec<Item>, VcsError> {
    let mut platform = repo
        .status(gix::progress::Discard)
        .map_err(|e| failed(op, e.to_string()))?
        .untracked_files(UntrackedFiles::Files)
        .index_worktree_rewrites(None);
    if renames == Renames::Off {
        platform =
            platform.tree_index_track_renames(gix::status::tree_index::TrackRenames::Disabled);
    }
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
