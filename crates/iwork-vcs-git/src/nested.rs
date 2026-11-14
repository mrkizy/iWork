//! status 的目录遍历不认嵌套仓库之后，把结果整理回 git 的样子（T10a 方案 2.4a；第 2 轮审查 F-3）。
//!
//! gix 遍历未跟踪目录时会对每个目录调 `is_git(<目录>/.git)` 判断是不是嵌套仓库，它跟着链接与 gitfile 去读那一头的
//! `HEAD`、`commondir`，不经过布局检查。所以打开 `recurse_repositories`：不再探，嵌套目录当普通目录往下走；
//! 名叫 `.git` 的条目 gix 只按名字认成 `DotGit`、不读不进（gix-dir 0.32 `walk/classify.rs`），再用 `emit_pruned`
//! 交出来。这样遍历本身不再碰 `.git` 的内容，代价是结果和 git 不一样，这里收回来：
//! - 已跟踪的子模块目录被当成普通目录走了一遍，里面的文件会被列成未跟踪：落在暂存区 gitlink 路径下面的条目一律
//!   去掉（子模块指向的提交由 `Worktree::changes_gix_cannot_judge` 自己比）。表只取暂存区，不读子模块里的东西。
//! - 未跟踪的嵌套仓库（例如 clone 进来的 `vendor/dep`），git 只列一行 `vendor/dep/`：下面有名叫 `.git` 的条目、
//!   而暂存区里没有任何条目落在它下面的目录，收成一行未跟踪条目。只看名字存在，`.git` 是什么（目录、文件、链接、
//!   读不读得到）都不看，所以结果不随仓库外的东西变。暂存区里有条目的目录不收（git 不把有已跟踪文件的目录
//!   当嵌套仓库，第 3 轮 F-1），`.git` 条目丢掉，里面的文件照常逐个列。
//!   和 git 的差别（KA2）：未跟踪目录里的 `.git` 不是合法仓库时，git 照常列里面的文件，这里也收成一行，之后
//!   目录里再加、再改文件，status 还是那一行，`worktree_versions` 前后都是 `Absent`。
//!
//! 「是不是子模块」「暂存区里有没有条目落在这个目录下面」都经 [`IndexQuery`] 问 gix 自己的暂存区查找（第 4 轮
//! F-1）：大小写不敏感的仓库里，磁盘上的目录名和暂存区里的可以只差大小写，按字节比会判错。比较规则不在这里写。

use gix::bstr::{BStr, BString, ByteSlice};
use gix::dir::entry::{Kind, Property, Status};
use gix::status::Item;
use gix::status::index_worktree::Item as WorktreeItem;

/// [`fold_nested_repositories`] 对暂存区要问的两件事。它只凭暂存区判断，不看磁盘。
pub(crate) trait IndexQuery {
    /// 暂存区里 `path` 是子模块（gitlink）条目
    fn is_gitlink(&self, path: &BStr) -> bool;
    /// 暂存区里至少有一个条目落在目录 `dir` 下面
    fn is_tracked_dir(&self, dir: &BStr) -> bool;
}

/// 用 gix 的暂存区查找回答 [`IndexQuery`]，和 gix 遍历目录时判断「已跟踪」用的是同一套接口与规则
/// （gix-dir 0.32 `walk/classify.rs`：大小写不敏感时 `entry_by_path_icase`、
/// `entry_closest_to_directory_or_directory_icase`，否则 `entry_by_path`、`prefixed_entries_range`）。
/// `ignore_case` 取仓库的 `core.ignoreCase`，和 gix status 一样从 `filesystem_options()` 来。
pub(crate) struct GixIndex<'a> {
    index: &'a gix::index::State,
    /// `ignore_case` 为真时的大小写不敏感查找表
    icase: Option<gix::index::AccelerateLookup<'a>>,
}

impl<'a> GixIndex<'a> {
    pub(crate) fn new(index: &'a gix::index::State, ignore_case: bool) -> Self {
        Self {
            index,
            icase: ignore_case.then(|| index.prepare_icase_backing()),
        }
    }
}

impl IndexQuery for GixIndex<'_> {
    fn is_gitlink(&self, path: &BStr) -> bool {
        let entry = match &self.icase {
            Some(lookup) => self.index.entry_by_path_icase(path, true, lookup),
            None => self.index.entry_by_path(path),
        };
        entry.is_some_and(|e| e.mode.is_submodule())
    }

    fn is_tracked_dir(&self, dir: &BStr) -> bool {
        match &self.icase {
            Some(lookup) => self
                .index
                .entry_closest_to_directory_or_directory_icase(dir, true, lookup)
                .is_some(),
            None => {
                let mut prefix = dir.to_owned();
                prefix.push(b'/');
                self.index
                    .prefixed_entries_range(prefix.as_bstr())
                    .is_some()
            }
        }
    }
}

/// `path` 本身或它的某一级上级目录是子模块。
fn in_gitlink(path: &BStr, index: &impl IndexQuery) -> bool {
    let mut rest = path;
    loop {
        if index.is_gitlink(rest) {
            return true;
        }
        match rest.rfind_byte(b'/') {
            Some(slash) => rest = rest[..slash].as_bstr(),
            None => return false,
        }
    }
}

/// 整理 gix status 的条目：去掉子模块（暂存区 gitlink）下面的条目；下面有 `.git`、暂存区里又没有条目的目录收成一行。
pub(crate) fn fold_nested_repositories(items: Vec<Item>, index: &impl IndexQuery) -> Vec<Item> {
    let mut nested: Vec<BString> = items
        .iter()
        .filter_map(dot_git_parent)
        .filter(|dir| !in_gitlink(dir, index) && !index.is_tracked_dir(dir))
        .map(ToOwned::to_owned)
        .collect();
    nested.sort();
    nested.dedup();
    // 嵌套里再嵌套的只留最外层
    let outermost: Vec<BString> = nested
        .iter()
        .filter(|dir| !under_any(dir.as_bstr(), nested.iter()))
        .cloned()
        .collect();

    let mut out: Vec<Item> = items
        .into_iter()
        .filter(|item| match directory_entry(item) {
            Some(entry) => {
                entry.property != Some(Property::DotGit)
                    && !in_gitlink(entry.rela_path.as_bstr(), index)
                    && !under_any(entry.rela_path.as_bstr(), outermost.iter())
            }
            None => true,
        })
        .collect();
    out.extend(outermost.into_iter().map(|dir| {
        Item::IndexWorktree(WorktreeItem::DirectoryContents {
            entry: gix::dir::Entry {
                rela_path: dir,
                status: Status::Untracked,
                property: None,
                disk_kind: Some(Kind::Repository),
                index_kind: None,
                pathspec_match: None,
            },
            collapsed_directory_status: None,
        })
    }));
    out
}

fn directory_entry(item: &Item) -> Option<&gix::dir::Entry> {
    match item {
        Item::IndexWorktree(WorktreeItem::DirectoryContents { entry, .. }) => Some(entry),
        _ => None,
    }
}

/// `<目录>/.git` 这种 `DotGit` 条目所在的目录；工作区根的 `.git` 不算。
fn dot_git_parent(item: &Item) -> Option<&BStr> {
    let entry = directory_entry(item)?;
    if entry.property != Some(Property::DotGit) {
        return None;
    }
    let slash = entry.rela_path.rfind_byte(b'/')?;
    Some(entry.rela_path[..slash].as_bstr())
}

/// `path` 严格在 `dirs` 里某一个目录下面。`dirs` 与 `path` 都出自同一次遍历，写法一致，按字节比就行。
fn under_any<'a>(path: &BStr, mut dirs: impl Iterator<Item = &'a BString>) -> bool {
    dirs.any(|dir| {
        path.len() > dir.len() && path.starts_with(dir.as_slice()) && path[dir.len()] == b'/'
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeSet;

    /// 按字节比的假暂存区：只用来测整理的逻辑，比较规则由 [`GixIndex`] 交给 gix（集成用例覆盖）。
    #[derive(Default)]
    struct Fake {
        gitlinks: BTreeSet<String>,
        tracked_dirs: BTreeSet<String>,
    }

    impl Fake {
        fn new(gitlinks: &[&str], tracked_dirs: &[&str]) -> Self {
            Self {
                gitlinks: gitlinks.iter().map(|s| s.to_string()).collect(),
                tracked_dirs: tracked_dirs.iter().map(|s| s.to_string()).collect(),
            }
        }
    }

    impl IndexQuery for Fake {
        fn is_gitlink(&self, path: &BStr) -> bool {
            self.gitlinks.contains(&path.to_string())
        }
        fn is_tracked_dir(&self, dir: &BStr) -> bool {
            self.tracked_dirs.contains(&dir.to_string())
        }
    }

    fn entry(path: &str, status: Status, property: Option<Property>) -> Item {
        Item::IndexWorktree(WorktreeItem::DirectoryContents {
            entry: gix::dir::Entry {
                rela_path: path.into(),
                status,
                property,
                disk_kind: None,
                index_kind: None,
                pathspec_match: None,
            },
            collapsed_directory_status: None,
        })
    }

    fn untracked(path: &str) -> Item {
        entry(path, Status::Untracked, None)
    }

    fn dot_git(path: &str) -> Item {
        entry(path, Status::Pruned, Some(Property::DotGit))
    }

    fn paths(items: &[Item]) -> Vec<String> {
        let mut out: Vec<String> = items
            .iter()
            .filter_map(directory_entry)
            .map(|e| e.rela_path.to_string())
            .collect();
        out.sort();
        out
    }

    #[test]
    fn drops_entries_under_gitlinks_and_folds_nested_repositories() {
        let items = vec![
            dot_git(".git"),
            untracked("plain/p"),
            // 子模块（暂存区里是 gitlink）：里面的东西全去掉
            dot_git("deps/lib/.git"),
            untracked("deps/lib/l"),
            untracked("deps/lib/x/y"),
            // 名字只是前缀相同的兄弟目录不受影响
            untracked("deps/library/z"),
            // 嵌套仓库，里面再嵌套一个：收成最外层一行
            dot_git("vendor/dep/.git"),
            untracked("vendor/dep/n1"),
            dot_git("vendor/dep/inner/.git"),
            untracked("vendor/dep/inner/n2"),
        ];
        let index = Fake::new(&["deps/lib"], &["deps"]);

        let out = fold_nested_repositories(items, &index);

        assert_eq!(paths(&out), ["deps/library/z", "plain/p", "vendor/dep"]);
        let folded = out
            .iter()
            .filter_map(directory_entry)
            .find(|e| e.rela_path == "vendor/dep")
            .unwrap();
        assert_eq!(folded.status, Status::Untracked);
    }

    #[test]
    fn nothing_to_fold_leaves_items_alone() {
        let items = vec![dot_git(".git"), untracked("a"), untracked("b/c")];
        let out = fold_nested_repositories(items, &Fake::default());
        assert_eq!(paths(&out), ["a", "b/c"]);
    }

    /// 暂存区里有条目落在下面的目录不收（git 不把它当嵌套仓库）：`.git` 条目丢掉，新文件照常列。
    #[test]
    fn tracked_directory_is_not_folded() {
        let items = vec![
            dot_git("t/.git"),
            untracked("t/new"),
            untracked("t/sub/new2"),
            dot_git("deep/t/.git"),
            untracked("deep/t/new"),
            dot_git("u/.git"),
            untracked("u/f"),
        ];
        let index = Fake::new(&[], &["t", "deep", "deep/t", "deep/t/x"]);

        let out = fold_nested_repositories(items, &index);

        assert_eq!(paths(&out), ["deep/t/new", "t/new", "t/sub/new2", "u"]);
    }
}
