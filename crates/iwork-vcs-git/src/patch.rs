//! 一个文件前后两版内容 → 统一差异格式（纯函数，无 I/O）。

use std::path::PathBuf;

use gix::bstr::{BStr, BString, ByteSlice};
use gix::diff::blob::unified_diff::{ConsumeBinaryHunk, ContextSize};
use gix::diff::blob::{Algorithm, InternedInput, UnifiedDiff, diff_with_slider_heuristics};
use iwork_ports::vcs::FileDiff;

/// 与 git 一样：前 8000 字节里有 NUL 就当二进制。
const BINARY_SNIFF_LEN: usize = 8000;
const CONTEXT_LINES: u32 = 3;

/// 比较一个文件的前后两版；`None` 表示那一边没有这个文件。内容相同返回 `None`。
///
/// 二进制文件不出逐行差异，`patch` 只写一行说明，增删行数为 0。
pub(crate) fn file_diff(path: PathBuf, old: Option<&[u8]>, new: Option<&[u8]>) -> Option<FileDiff> {
    if old == new {
        return None;
    }
    let shown = path.to_string_lossy().replace('\\', "/");
    let old_name = old.map_or_else(|| "/dev/null".to_owned(), |_| format!("a/{shown}"));
    let new_name = new.map_or_else(|| "/dev/null".to_owned(), |_| format!("b/{shown}"));
    let (old, new) = (old.unwrap_or_default(), new.unwrap_or_default());

    if is_binary(old) || is_binary(new) {
        return Some(FileDiff {
            path,
            insertions: 0,
            deletions: 0,
            patch: format!("Binary files {old_name} and {new_name} differ\n"),
        });
    }

    let input = InternedInput::new(BStr::new(old), BStr::new(new));
    let diff = diff_with_slider_heuristics(Algorithm::Histogram, &input);
    let hunks = UnifiedDiff::new(
        &diff,
        &input,
        ConsumeBinaryHunk::new(BString::default(), "\n"),
        ContextSize::symmetrical(CONTEXT_LINES),
    )
    .consume()
    .expect("写进内存里的 BString 不会出 I/O 错误");
    Some(FileDiff {
        path,
        insertions: diff.count_additions(),
        deletions: diff.count_removals(),
        patch: format!("--- {old_name}\n+++ {new_name}\n{}", hunks.to_str_lossy()),
    })
}

fn is_binary(data: &[u8]) -> bool {
    data[..data.len().min(BINARY_SNIFF_LEN)].contains(&0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn modified_text_file_yields_unified_hunk_and_counts() {
        let d = file_diff(
            "src/a.txt".into(),
            Some(b"one\ntwo\nthree\n"),
            Some(b"one\n2\nthree\nfour\n"),
        )
        .expect("contents differ");
        assert_eq!((d.insertions, d.deletions), (2, 1));
        assert_eq!(
            d.patch,
            "--- a/src/a.txt\n+++ b/src/a.txt\n@@ -1,3 +1,4 @@\n one\n-two\n+2\n three\n+four\n"
        );
    }

    #[test]
    fn added_and_deleted_files_use_dev_null() {
        let added = file_diff("n".into(), None, Some(b"x\n")).expect("added");
        assert!(
            added.patch.starts_with("--- /dev/null\n+++ b/n\n"),
            "{}",
            added.patch
        );
        assert_eq!((added.insertions, added.deletions), (1, 0));
        let deleted = file_diff("n".into(), Some(b"x\ny\n"), None).expect("deleted");
        assert!(
            deleted.patch.starts_with("--- a/n\n+++ /dev/null\n"),
            "{}",
            deleted.patch
        );
        assert_eq!((deleted.insertions, deleted.deletions), (0, 2));
    }

    #[test]
    fn identical_contents_yield_nothing_and_binary_gets_a_note() {
        assert_eq!(file_diff("s".into(), Some(b"same"), Some(b"same")), None);
        let bin = file_diff("b.bin".into(), Some(b"a\0b"), Some(b"a\0c")).expect("differs");
        assert_eq!(bin.patch, "Binary files a/b.bin and b/b.bin differ\n");
        assert_eq!((bin.insertions, bin.deletions), (0, 0));
    }
}
