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

use crate::patch::{EntryMode, Side, file_diff, unsupported_diff};

/// 当前基线：HEAD 指向的提交。还没有任何提交时用空树，这样相对它的差异就是全部文件。
pub(crate) fn baseline(repo_root: &RepoRoot) -> Result<BaselineRef, VcsError> {
    let repo = open(repo_root, "baseline")?.repo;
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
    let Opened {
        repo,
        external_filters,
    } = open(repo_root, OP)?;
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
    // gix 看不到或不能信的改动，按同一判定补上（与 diff 共用，见 changes_gix_cannot_judge）
    let mut worktree = Worktree::new(&repo, repo_root, external_filters, OP)?;
    for path in worktree.changes_gix_cannot_judge()? {
        by_path
            .entry(path)
            .or_default()
            .1
            .get_or_insert(ChangeKind::Modified);
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
/// 两侧都换成 git 内部的表示再比较：工作区的普通文件过 gix 进程内的内容转换（换行、编码、ident）。
/// 后台读操作不运行任何外部程序（见 [`open`]）：要靠外部过滤程序（`filter=<driver>`，例如 LFS）才能比较的
/// 文件只标注「不支持」。工作区一侧读文件时路径上任何一级都不跟随符号链接（`iwork-rootfs`）。
/// 不做改名检测：改名就是源路径删除加目标路径新增。
pub(crate) fn diff(repo_root: &RepoRoot, range: &DiffRange) -> Result<Diff, VcsError> {
    const OP: &str = "diff";
    let Opened {
        repo,
        external_filters,
    } = open(repo_root, OP)?;
    let mut files = Vec::new();
    match range {
        DiffRange::WorkspaceSince(base) => {
            let base_tree = tree_of(&repo, base, OP)?;
            let tree = repo
                .find_tree(base_tree)
                .map_err(|e| failed(OP, format!("读基线树失败：{e}")))?;
            let mut worktree = Worktree::new(&repo, repo_root, external_filters, OP)?;
            // 以基线树作比较起点跑一遍 status，得到可能变了的路径，再补上 gix 判断不了的；逐个比
            let mut candidates: BTreeSet<BString> =
                worktree.changes_gix_cannot_judge()?.into_iter().collect();
            for item in status_items(&repo, Some(base_tree), Renames::Off, OP)? {
                if let Item::TreeIndex(gix::diff::index::Change::Rewrite {
                    source_location, ..
                }) = &item
                {
                    candidates.insert(source_location.clone().into_owned());
                }
                candidates.insert(item.location().to_owned());
            }
            for path in candidates {
                let rel = to_path(path.as_ref());
                let old = match tree
                    .lookup_entry_by_path(&rel)
                    .map_err(|e| failed(OP, format!("{}：{e}", rel.display())))?
                {
                    Some(entry) => tree_side(&repo, entry.mode().kind(), entry.object_id(), OP)?,
                    None => None,
                };
                match worktree.side(&rel, old.as_ref())? {
                    WorktreeSide::Absent => files.extend(file_diff(rel, old.as_ref(), None)),
                    WorktreeSide::Present(new) => {
                        files.extend(file_diff(rel, old.as_ref(), Some(&new)));
                    }
                    WorktreeSide::NeedsExternalFilter { mode, driver } => {
                        files.push(unsupported_diff(rel, old.as_ref(), mode, driver.as_ref()));
                    }
                }
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

/// 工作区里一个路径的样子。
enum WorktreeSide {
    Absent,
    Present(Side),
    /// 普通文件，但要靠外部过滤程序 `driver` 才能比较内容
    NeedsExternalFilter {
        mode: EntryMode,
        driver: BString,
    },
}

/// 读工作区一侧：不跟随链接，普通文件过进程内的内容转换。
struct Worktree<'repo> {
    root: &'repo Path,
    filter: gix::filter::Pipeline<'repo>,
    index: gix::worktree::IndexPersistedOrInMemory,
    caps: gix::fs::Capabilities,
    stat_options: gix::index::entry::stat::Options,
    /// 配置过的外部过滤器名，和查 `filter` 属性用的属性栈；没有配置任何过滤器时为 `None`
    external: Option<(
        BTreeSet<BString>,
        gix::AttributeStack<'repo>,
        gix::attrs::search::Outcome,
    )>,
    op: &'static str,
}

impl<'repo> Worktree<'repo> {
    fn new(
        repo: &'repo Repository,
        root: &'repo RepoRoot,
        external_filters: BTreeSet<BString>,
        op: &'static str,
    ) -> Result<Self, VcsError> {
        let (filter, index) = repo
            .filter_pipeline(None)
            .map_err(|e| failed(op, format!("准备内容转换失败：{e}")))?;
        let caps = repo
            .filesystem_options()
            .map_err(|e| failed(op, format!("读文件系统配置失败：{e}")))?;
        let stat_options = repo
            .stat_options()
            .map_err(|e| failed(op, format!("读 stat 配置失败：{e}")))?;
        let external = if external_filters.is_empty() {
            None
        } else {
            let attrs = repo
                .attributes_only(
                    &index,
                    gix::worktree::stack::state::attributes::Source::WorktreeThenIdMapping,
                )
                .map_err(|e| failed(op, format!("读 .gitattributes 失败：{e}")))?;
            let outcome = attrs.selected_attribute_matches(["filter"]);
            Some((external_filters, attrs, outcome))
        };
        Ok(Self {
            root: root.0.as_path(),
            filter,
            index,
            caps,
            stat_options,
            external,
            op,
        })
    }

    /// `rel` 在工作区里的样子；`old` 是基线一侧，用来在不认执行位的文件系统上沿用原 mode。
    fn side(&mut self, rel: &Path, old: Option<&Side>) -> Result<WorktreeSide, VcsError> {
        let op = self.op;
        let kind = match iwork_rootfs::inspect(self.root, rel) {
            Ok(kind) => kind,
            // 上级目录被换成了链接：按 git 的看法，这个路径在工作区里已经不存在（链接本身另算一个路径）
            Err(iwork_rootfs::RootedError::ThroughSymlink { .. }) => {
                return Ok(WorktreeSide::Absent);
            }
            Err(e) => return Err(failed(op, e.to_string())),
        };
        match kind {
            iwork_rootfs::EntryKind::File { executable } => {
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
                if let Some(driver) = self.external_driver(rel)? {
                    return Ok(WorktreeSide::NeedsExternalFilter { mode, driver });
                }
                let raw = iwork_rootfs::read_file(self.root, rel)
                    .map_err(|e| failed(op, e.to_string()))?
                    .unwrap_or_default();
                Ok(WorktreeSide::Present(Side {
                    mode,
                    data: self.convert_to_git(rel, &raw)?,
                }))
            }
            iwork_rootfs::EntryKind::Symlink => {
                let target = iwork_rootfs::read_link(self.root, rel)
                    .map_err(|e| failed(op, e.to_string()))?;
                Ok(WorktreeSide::Present(Side {
                    mode: EntryMode::Symlink,
                    data: gix::path::into_bstr(target)
                        .map_err(|e| {
                            failed(op, format!("{} 的链接目标无法表示：{e}", rel.display()))
                        })?
                        .into_owned()
                        .into(),
                }))
            }
            iwork_rootfs::EntryKind::Dir | iwork_rootfs::EntryKind::Missing => {
                // 子模块引用以暂存区为准：没检出（未初始化、已 deinit、目录不在）时用暂存区里记的提交；
                // 暂存区里也没有这个条目，才是真的删掉了
                let Some(staged) = self.index_gitlink(rel) else {
                    return Ok(WorktreeSide::Absent);
                };
                let id = match self.checked_out_submodule_head(rel, kind)? {
                    Some(head) => head,
                    None => staged,
                };
                Ok(WorktreeSide::Present(Side::gitlink(id)))
            }
            iwork_rootfs::EntryKind::Other => Ok(WorktreeSide::Absent),
        }
    }

    /// gix 的 status 看不到或不能信的已跟踪路径（相对暂存区）。`status` 与 `diff` 都用它，判定只有这一份：
    ///
    /// - **需要外部过滤器的文件**：后台不运行外部程序，原始字节和存储内容相同也不能说明没改动（存储的是过滤后的
    ///   内容）。所以只看暂存区记的 stat（时间戳、大小等）：变了就列为「可能有改动」，差异里标「内容比较不支持」。
    /// - **子模块指向的提交**：gix 的子模块扫描已关掉（见 [`status_items`]，它会用子模块自己的配置跑 status），
    ///   这里只比较检出的提交与暂存区记录的提交，相当于 `--ignore-submodules=dirty`。
    fn changes_gix_cannot_judge(&mut self) -> Result<Vec<BString>, VcsError> {
        use gix::index::entry::{Mode, Stage, Stat};
        let op = self.op;
        let entries: Vec<(BString, Mode, Stat, ObjectId)> = self
            .index
            .entries()
            .iter()
            .filter(|e| e.stage() == Stage::Unconflicted)
            .map(|e| (e.path(&self.index).to_owned(), e.mode, e.stat, e.id))
            .collect();
        let mut out = Vec::new();
        for (path, mode, stat, id) in entries {
            let rel = to_path(path.as_ref());
            let kind = match iwork_rootfs::inspect(self.root, &rel) {
                Ok(kind) => kind,
                // 上级目录被换成链接：路径已不在，gix 会报删除
                Err(iwork_rootfs::RootedError::ThroughSymlink { .. }) => continue,
                Err(e) => return Err(failed(op, e.to_string())),
            };
            let changed = if mode == Mode::COMMIT {
                self.checked_out_submodule_head(&rel, kind)?
                    .is_some_and(|head| head != id)
            } else if matches!(kind, iwork_rootfs::EntryKind::File { .. })
                && (mode == Mode::FILE || mode == Mode::FILE_EXECUTABLE)
                && self.external_driver(&rel)?.is_some()
            {
                let on_disk = gix::index::fs::Metadata::from_path_no_follow(&self.root.join(&rel))
                    .map_err(|e| failed(op, format!("{}：{e}", rel.display())))?;
                let on_disk = Stat::from_fs(&on_disk)
                    .map_err(|e| failed(op, format!("{} 的时间戳无法表示：{e}", rel.display())))?;
                !stat.matches(&on_disk, self.stat_options)
            } else {
                false
            };
            if changed {
                out.push(path);
            }
        }
        Ok(out)
    }

    /// `rel` 的 `filter` 属性指向一个配置过的外部过滤器时返回它的名字。
    fn external_driver(&mut self, rel: &Path) -> Result<Option<BString>, VcsError> {
        let op = self.op;
        let Some((names, attrs, outcome)) = self.external.as_mut() else {
            return Ok(None);
        };
        attrs
            .at_path(rel, None)
            .map_err(|e| failed(op, format!("读 {} 的属性失败：{e}", rel.display())))?
            .matching_attributes(outcome);
        Ok(outcome
            .iter_selected()
            .find_map(|m| match m.assignment.state {
                gix::attrs::StateRef::Value(v) if names.contains(v.as_bstr()) => {
                    Some(v.as_bstr().to_owned())
                }
                _ => None,
            }))
    }

    fn convert_to_git(&mut self, rel: &Path, raw: &[u8]) -> Result<Vec<u8>, VcsError> {
        use gix::filter::plumbing::pipeline::convert::ToGitOutcome;
        use std::io::Read;
        let op = self.op;
        let outcome = self
            .filter
            .convert_to_git(raw, rel, &self.index)
            .map_err(|e| failed(op, format!("{} 内容转换失败：{e}", rel.display())))?;
        let mut out = Vec::new();
        match outcome {
            ToGitOutcome::Unchanged(mut r) => r.read_to_end(&mut out),
            ToGitOutcome::Buffer(b) => {
                out.extend_from_slice(b);
                Ok(b.len())
            }
            // 只有外部过滤程序才产出这种结果；open 已去掉全部外部过滤器，走到这里说明那道闸失效了
            ToGitOutcome::Process(_) => {
                return Err(failed(
                    op,
                    format!("{} 意外地经过了外部过滤程序，拒绝使用结果", rel.display()),
                ));
            }
        }
        .map_err(|e| failed(op, format!("{} 内容转换失败：{e}", rel.display())))?;
        Ok(out)
    }

    /// 暂存区里 `rel` 是子模块引用时返回它记录的提交。
    fn index_gitlink(&self, rel: &Path) -> Option<ObjectId> {
        // 转不成 git 路径的名字不可能在暂存区里
        let key = gix::path::into_bstr(rel).ok()?;
        let key = gix::path::to_unix_separators_on_windows(key);
        self.index
            .entry_by_path(key.as_ref())
            .filter(|e| e.mode == gix::index::entry::Mode::COMMIT)
            .map(|e| e.id)
    }

    /// 子模块已检出时返回它当前的提交；没检出（目录不在，或里面没有 `.git`）返回 `None`。
    fn checked_out_submodule_head(
        &self,
        rel: &Path,
        kind: iwork_rootfs::EntryKind,
    ) -> Result<Option<ObjectId>, VcsError> {
        let op = self.op;
        if kind != iwork_rootfs::EntryKind::Dir {
            return Ok(None);
        }
        let dot_git = rel.join(".git");
        match iwork_rootfs::inspect(self.root, &dot_git) {
            Ok(iwork_rootfs::EntryKind::Missing) => return Ok(None),
            Ok(_) => {}
            Err(e) => return Err(failed(op, e.to_string())),
        }
        // 与父仓库一样经 open 打开：子模块自己的配置也要先去掉外部过滤器，只读 HEAD
        let sub = open(&RepoRoot(self.root.join(rel)), op)?.repo;
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
        .index_worktree_rewrites(None)
        // 不扫描子模块内部：gix 会用子模块自己未清理的配置跑 status（可能运行外部过滤器）。子模块指向的
        // 提交变化由 Worktree::changes_gix_cannot_judge 自己比较
        .index_worktree_submodules(None);
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

/// 为后台读操作打开的仓库。
struct Opened {
    repo: Repository,
    /// 配置里有、但已从这个仓库实例里去掉的外部过滤器名
    external_filters: BTreeSet<BString>,
}

/// 打开仓库做读操作（`baseline`、`status`、`diff` 都经这里，配置只有这一份）。
///
/// 后台读操作绝不启动外部程序：仓库配置（含 `.git/config`，Agent 的脚本可能写过它）里的 `filter.<name>.clean`
/// / `smudge` / `process` 会让 gix 在比较内容时以后台服务的权限、在沙箱外运行任意程序。所以打开后从
/// **内存里的**配置删掉全部 `filter.*` 节（不改磁盘上的配置），只留进程内的换行、编码等转换；删掉的名字
/// 记下来，用于把需要它们的文件标成「不支持比较」。删完再确认过滤管线里确实没有外部驱动，否则报错（失败即关闭）。
fn open(repo_root: &RepoRoot, op: &'static str) -> Result<Opened, VcsError> {
    if !repo_root.0.exists() {
        return Err(VcsError::PathMissing {
            path: repo_root.0.clone(),
        });
    }
    let mut repo = gix::open(&repo_root.0)
        .map_err(|e| failed(op, format!("{}：{e}", repo_root.0.display())))?;
    let external_filters = remove_external_filters(&mut repo, op)?;
    Ok(Opened {
        repo,
        external_filters,
    })
}

fn remove_external_filters(
    repo: &mut Repository,
    op: &'static str,
) -> Result<BTreeSet<BString>, VcsError> {
    let mut names = BTreeSet::new();
    let mut config = repo.config_snapshot_mut();
    let ids: Vec<_> = config
        .sections_and_ids_by_name("filter")
        .into_iter()
        .flatten()
        .map(|(section, id)| {
            if let Some(name) = section.header().subsection_name() {
                names.insert(name.to_owned());
            }
            id
        })
        .collect();
    for id in ids {
        config.remove_section_by_id(id);
    }
    config
        .commit()
        .map_err(|e| failed(op, format!("去掉外部过滤器配置失败：{e}")))?;
    let left = gix::filter::Pipeline::options(repo)
        .map_err(|e| failed(op, format!("读过滤配置失败：{e}")))?
        .drivers;
    if !left.is_empty() {
        return Err(failed(
            op,
            format!("仍有 {} 个外部过滤器没能去掉，拒绝读取", left.len()),
        ));
    }
    Ok(names)
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
