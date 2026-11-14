//! status 的目录遍历不认嵌套仓库之后，把结果整理回 git 的样子（T10a 方案 2.4a；第 2 轮审查 F-3）。
//!
//! gix 遍历未跟踪目录时会对每个目录调 `is_git(<目录>/.git)` 判断是不是嵌套仓库，它跟着链接与 gitfile 去读那一头的
//! `HEAD`、`commondir`，不经过布局检查。所以打开 `recurse_repositories`：不再探，嵌套目录当普通目录往下走；
//! 名叫 `.git` 的条目 gix 只按名字认成 `DotGit`、不读不进（gix-dir 0.32 `walk/classify.rs`），再用 `emit_pruned`
//! 交出来。这样遍历本身不再碰 `.git` 的内容，代价是结果和 git 不一样，这里收回来：
//! - 已跟踪的子模块目录被当成普通目录走了一遍，里面的文件会被列成未跟踪：落在暂存区 gitlink 路径下面的条目一律
//!   去掉（子模块指向的提交由 `Worktree::changes_gix_cannot_judge` 自己比）。表只取暂存区，不读子模块里的东西。
//! - 未跟踪的嵌套仓库（例如 clone 进来的 `vendor/dep`），git 只列一行 `vendor/dep/`：凡是下面有名叫 `.git` 的
//!   条目的目录，收成一行未跟踪条目。只看名字存在，`.git` 是什么（目录、文件、链接、读不读得到）都不看，
//!   所以结果不随仓库外的东西变。和 git 的差别：`.git` 不是合法仓库时，git 照常列里面的文件，这里也收成一行。

use std::collections::BTreeSet;

use gix::bstr::{BStr, BString, ByteSlice};
use gix::dir::entry::{Kind, Property, Status};
use gix::status::Item;
use gix::status::index_worktree::Item as WorktreeItem;

/// 整理 gix status 的条目：去掉 `gitlinks`（暂存区里的子模块路径）下面的条目；下面有 `.git` 的未跟踪目录收成一行。
pub(crate) fn fold_nested_repositories(
    items: Vec<Item>,
    gitlinks: &BTreeSet<BString>,
) -> Vec<Item> {
    let mut nested: Vec<BString> = items
        .iter()
        .filter_map(dot_git_parent)
        .filter(|dir| !gitlinks.contains(*dir) && !under_any(dir, gitlinks.iter()))
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
                    && !under_any(entry.rela_path.as_bstr(), gitlinks.iter())
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

/// `path` 严格在 `dirs` 里某一个目录下面。
fn under_any<'a>(path: &BStr, mut dirs: impl Iterator<Item = &'a BString>) -> bool {
    dirs.any(|dir| {
        path.len() > dir.len() && path.starts_with(dir.as_slice()) && path[dir.len()] == b'/'
    })
}

#[cfg(test)]
mod tests {
    use super::*;

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
        let gitlinks = BTreeSet::from([BString::from("deps/lib")]);

        let out = fold_nested_repositories(items, &gitlinks);

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
        let out = fold_nested_repositories(items, &BTreeSet::new());
        assert_eq!(paths(&out), ["a", "b/c"]);
    }
}
