//! 一个路径前后两版（类型、mode 与内容）→ 统一差异格式（纯函数，无 I/O）。

use std::fmt;
use std::path::PathBuf;

use gix::bstr::{BStr, BString, ByteSlice};
use gix::diff::blob::unified_diff::{ConsumeBinaryHunk, ContextSize};
use gix::diff::blob::{Algorithm, InternedInput, UnifiedDiff, diff_with_slider_heuristics};
use iwork_ports::vcs::FileDiff;

/// 与 git 一样：前 8000 字节里有 NUL 就当二进制。
const BINARY_SNIFF_LEN: usize = 8000;
const CONTEXT_LINES: u32 = 3;

/// 树里一个条目的类型（git 的 mode）。目录不出现在差异里，所以没有它。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum EntryMode {
    Regular,
    Executable,
    Symlink,
    /// 子模块引用
    Gitlink,
}

impl EntryMode {
    pub(crate) fn from_tree(mode: gix::object::tree::EntryKind) -> Option<Self> {
        use gix::object::tree::EntryKind as K;
        match mode {
            K::Blob => Some(Self::Regular),
            K::BlobExecutable => Some(Self::Executable),
            K::Link => Some(Self::Symlink),
            K::Commit => Some(Self::Gitlink),
            K::Tree => None,
        }
    }
}

impl fmt::Display for EntryMode {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Regular => "100644",
            Self::Executable => "100755",
            Self::Symlink => "120000",
            Self::Gitlink => "160000",
        })
    }
}

/// 一个路径在某一侧的样子。内容是 git 内部的表示：普通文件过了内容过滤（换行转换等），
/// 链接是链接目标文本，子模块引用是 `Subproject commit <id>\n`（与 git 的写法一致）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Side {
    pub mode: EntryMode,
    pub data: Vec<u8>,
}

impl Side {
    pub(crate) fn gitlink(id: impl fmt::Display) -> Self {
        Self {
            mode: EntryMode::Gitlink,
            data: format!("Subproject commit {id}\n").into_bytes(),
        }
    }
}

/// 比较一个路径的前后两版；`None` 表示那一边没有它。完全相同返回 `None`。
///
/// - 类型或 mode 变了先写 `old mode` / `new mode`（新增、删除写 `new file mode` / `deleted file mode`），
///   内容没变时差异只有这几行，增删行数为 0。
/// - 二进制文件不出逐行差异，只写一行说明，增删行数为 0。
pub(crate) fn file_diff(path: PathBuf, old: Option<&Side>, new: Option<&Side>) -> Option<FileDiff> {
    if old == new {
        return None;
    }
    let mut header = match (old, new) {
        (None, Some(n)) => format!("new file mode {}\n", n.mode),
        (Some(o), None) => format!("deleted file mode {}\n", o.mode),
        (Some(o), Some(n)) if o.mode != n.mode => {
            format!("old mode {}\nnew mode {}\n", o.mode, n.mode)
        }
        _ => String::new(),
    };
    let old_data = old.map(|s| s.data.as_slice());
    let new_data = new.map(|s| s.data.as_slice());
    if old.is_some() && new.is_some() && old_data == new_data {
        return Some(FileDiff {
            path,
            insertions: 0,
            deletions: 0,
            patch: header,
        });
    }

    let shown = path.to_string_lossy().replace('\\', "/");
    let old_name = old.map_or_else(|| "/dev/null".to_owned(), |_| format!("a/{shown}"));
    let new_name = new.map_or_else(|| "/dev/null".to_owned(), |_| format!("b/{shown}"));
    let (old_data, new_data) = (old_data.unwrap_or_default(), new_data.unwrap_or_default());

    if is_binary(old_data) || is_binary(new_data) {
        header.push_str(&format!("Binary files {old_name} and {new_name} differ\n"));
        return Some(FileDiff {
            path,
            insertions: 0,
            deletions: 0,
            patch: header,
        });
    }

    let input = InternedInput::new(BStr::new(old_data), BStr::new(new_data));
    let diff = diff_with_slider_heuristics(Algorithm::Histogram, &input);
    let hunks = UnifiedDiff::new(
        &diff,
        &input,
        ConsumeBinaryHunk::new(BString::default(), "\n"),
        ContextSize::symmetrical(CONTEXT_LINES),
    )
    .consume()
    .expect("写进内存里的 BString 不会出 I/O 错误");
    header.push_str(&format!(
        "--- {old_name}\n+++ {new_name}\n{}",
        hunks.to_str_lossy()
    ));
    Some(FileDiff {
        path,
        insertions: diff.count_additions(),
        deletions: diff.count_removals(),
        patch: header,
    })
}

fn is_binary(data: &[u8]) -> bool {
    data[..data.len().min(BINARY_SNIFF_LEN)].contains(&0)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn regular(data: &[u8]) -> Side {
        Side {
            mode: EntryMode::Regular,
            data: data.to_vec(),
        }
    }

    #[test]
    fn modified_text_file_yields_unified_hunk_and_counts() {
        let d = file_diff(
            "src/a.txt".into(),
            Some(&regular(b"one\ntwo\nthree\n")),
            Some(&regular(b"one\n2\nthree\nfour\n")),
        )
        .expect("contents differ");
        assert_eq!((d.insertions, d.deletions), (2, 1));
        assert_eq!(
            d.patch,
            "--- a/src/a.txt\n+++ b/src/a.txt\n@@ -1,3 +1,4 @@\n one\n-two\n+2\n three\n+four\n"
        );
    }

    #[test]
    fn added_and_deleted_files_name_their_mode_and_use_dev_null() {
        let added = file_diff("n".into(), None, Some(&regular(b"x\n"))).expect("added");
        assert!(
            added
                .patch
                .starts_with("new file mode 100644\n--- /dev/null\n+++ b/n\n"),
            "{}",
            added.patch
        );
        assert_eq!((added.insertions, added.deletions), (1, 0));
        let deleted = file_diff("n".into(), Some(&regular(b"x\ny\n")), None).expect("deleted");
        assert!(
            deleted
                .patch
                .starts_with("deleted file mode 100644\n--- a/n\n+++ /dev/null\n"),
            "{}",
            deleted.patch
        );
        assert_eq!((deleted.insertions, deleted.deletions), (0, 2));
    }

    #[test]
    fn mode_only_change_is_reported_without_hunks() {
        let exe = Side {
            mode: EntryMode::Executable,
            data: b"x\n".to_vec(),
        };
        let d = file_diff("run".into(), Some(&regular(b"x\n")), Some(&exe)).expect("mode differs");
        assert_eq!(d.patch, "old mode 100644\nnew mode 100755\n");
        assert_eq!((d.insertions, d.deletions), (0, 0));
    }

    #[test]
    fn gitlink_change_shows_both_commits() {
        let d = file_diff(
            "sub".into(),
            Some(&Side::gitlink("aaa")),
            Some(&Side::gitlink("bbb")),
        )
        .expect("differs");
        assert!(
            d.patch
                .ends_with("-Subproject commit aaa\n+Subproject commit bbb\n"),
            "{}",
            d.patch
        );
    }

    #[test]
    fn identical_sides_yield_nothing_and_binary_gets_a_note() {
        assert_eq!(
            file_diff("s".into(), Some(&regular(b"same")), Some(&regular(b"same"))),
            None
        );
        let bin = file_diff(
            "b.bin".into(),
            Some(&regular(b"a\0b")),
            Some(&regular(b"a\0c")),
        )
        .expect("differs");
        assert_eq!(bin.patch, "Binary files a/b.bin and b/b.bin differ\n");
        assert_eq!((bin.insertions, bin.deletions), (0, 0));
    }
}
