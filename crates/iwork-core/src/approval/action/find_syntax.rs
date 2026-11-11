//! `find` 的两套规则（方案 3.4「`find` 按它自己的语法切」、3.6「`-delete` 的约束不随 `Unparsable` 丢掉」）。
//!
//! 精确与保守的分界（方案 3.3）：按 `find` 自己的语法精确切（[`split`]）只用在整条都是字面、`parse` 成功
//! 的调用上，这时 `find` 那条简单命令的每个词都在，按参数个数跳不会跳错；结构一丢（`parse` 放弃的扁平词、
//! 再解析队列里收来的词）就只用不数参数的保守规则（[`flat_delete`]）。三张登记表（前置选项、谓词、
//! 执行类谓词）在 `commands.rs`，加一种 = 加一行。

use super::commands::{
    FIND_EXEC_PREDICATES, FIND_FLAG_PREDICATES, FIND_LEADING_LETTERS, FIND_LEADING_VALUE,
    FIND_PATH_PREDICATES, FIND_VALUE_PREDICATES, program_id,
};

/// `find` 的程序标识。
pub const FIND: &str = "find";

/// 一条 `find` 按它自己的语法切出来的结果。
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct FindCall<'a> {
    /// 起点：前置选项之后、第一个表达式词之前的词，加上 `-f` 的值。为空时起点是 cwd
    pub roots: Vec<&'a str>,
    /// 拿不准的：没登记的词后面紧跟的非 `-` 词，可能是它的值也可能是起点，只往严里算
    pub maybe_roots: Vec<&'a str>,
    /// `-newer FILE` 这类谓词读的路径
    pub read_paths: Vec<&'a str>,
    /// 表达式位置上有 `-delete` 记号（`-exec` 段里、谓词参数里的不算）
    pub delete: bool,
    /// 出现了没登记的词或执行类谓词的段：不是安全形态
    pub unregistered: bool,
}

impl<'a> FindCall<'a> {
    /// 删除的范围：起点（没有则 cwd）与拿不准的起点。
    pub fn delete_targets(&self) -> Vec<&'a str> {
        let mut out = if self.roots.is_empty() {
            vec!["."]
        } else {
            self.roots.clone()
        };
        out.extend(&self.maybe_roots);
        out
    }

    /// 读的范围：起点（没有则 cwd）与谓词读的路径。
    pub fn read_targets(&self) -> Vec<&'a str> {
        let mut out = if self.roots.is_empty() {
            vec!["."]
        } else {
            self.roots.clone()
        };
        out.extend(&self.read_paths);
        out
    }
}

fn is_expression_word(w: &str) -> bool {
    w.starts_with('-') || matches!(w, "(" | ")" | "!" | ",")
}

/// 不带值的前置选项：可以合写的字母组（`-HL`），或粘着级别的 `-O3`。
fn is_leading_flag(w: &str) -> bool {
    let letters = w
        .strip_prefix('-')
        .filter(|l| !l.is_empty() && !l.starts_with('-'));
    (w.starts_with("-O") && w.len() > 2)
        || letters.is_some_and(|l| l.chars().all(|c| FIND_LEADING_LETTERS.contains(c)))
}

/// 按 `find` 的语法切 `find` 后面的字面词（精确规则）。
pub fn split<'a>(args: &[&'a str]) -> FindCall<'a> {
    let ends = SegmentEnds::new(args);
    let mut call = FindCall::default();
    let mut i = 0;
    // 1. 前置选项
    while let Some(&w) = args.get(i) {
        if w == "--" {
            i += 1;
            break;
        }
        if w == "-f" {
            if let Some(&v) = args.get(i + 1) {
                call.roots.push(v);
            }
            i += 2;
        } else if FIND_LEADING_VALUE.contains(&w) {
            i += 2;
        } else if is_leading_flag(w) {
            i += 1;
        } else {
            break;
        }
    }
    // 2. 起点
    while let Some(&w) = args.get(i) {
        if is_expression_word(w) {
            break;
        }
        call.roots.push(w);
        i += 1;
    }
    // 3. 表达式
    while let Some(&w) = args.get(i) {
        if FIND_EXEC_PREDICATES.contains(&w) {
            // 找不到结束符的段不吞到底，落到下面按没登记的词处理
            if let Some(end) = ends.after(i + 1) {
                call.unregistered = true;
                i = end + 1;
                continue;
            }
        }
        if w == "-delete" {
            call.delete = true;
            i += 1;
        } else if w == "-f" {
            // 前置选项阶段被没登记的词提前结束时，后面的 `-f <路径>` 仍可能是起点（BSD 下以 `-` 开头的起点
            // 只能这样给）：只往严里算，记进拿不准的起点
            call.unregistered = true;
            if let Some(&v) = args.get(i + 1) {
                call.maybe_roots.push(v);
            }
            i += 2;
        } else if FIND_FLAG_PREDICATES.contains(&w) {
            i += 1;
        } else if FIND_VALUE_PREDICATES.contains(&w) {
            i += 2;
        } else if FIND_PATH_PREDICATES.contains(&w) {
            if let Some(&v) = args.get(i + 1) {
                call.read_paths.push(v);
            }
            i += 2;
        } else {
            // 4. 拿不准的只往严里算
            call.unregistered = true;
            if !w.starts_with('-') {
                call.maybe_roots.push(w);
                i += 1;
            } else {
                match args.get(i + 1) {
                    Some(&next) if !is_expression_word(next) => {
                        call.maybe_roots.push(next);
                        i += 2;
                    }
                    _ => i += 1,
                }
            }
        }
    }
    call
}

/// `-exec` 段的结束位置：`;`，或紧跟在 `{}` 后面的 `+`（`{}` 也在段里）。一遍预先算好每个位置之后最近的
/// 结束符，每次查询常数时间：没闭合的段不再每次扫到末尾，`split` 整体线性（6/F-4）。
struct SegmentEnds {
    /// `semicolon[j]`：下标不小于 `j` 的第一个 `;`
    semicolon: Vec<Option<usize>>,
    /// `plus[j]`：下标不小于 `j`、前一个词是 `{}` 的第一个 `+`
    plus: Vec<Option<usize>>,
}

impl SegmentEnds {
    fn new(args: &[&str]) -> Self {
        let n = args.len();
        let mut semicolon = vec![None; n + 1];
        let mut plus = vec![None; n + 1];
        for j in (0..n).rev() {
            semicolon[j] = if args[j] == ";" {
                Some(j)
            } else {
                semicolon[j + 1]
            };
            plus[j] = if args[j] == "+" && j > 0 && args[j - 1] == "{}" {
                Some(j)
            } else {
                plus[j + 1]
            };
        }
        Self { semicolon, plus }
    }

    /// 从 `from` 开始的段在哪结束；`+` 前面的 `{}` 必须在段里（下标不小于 `from`）。
    fn after(&self, from: usize) -> Option<usize> {
        let semi = self.semicolon.get(from).copied().flatten();
        let plus = self.plus.get(from + 1).copied().flatten();
        match (semi, plus) {
            (Some(a), Some(b)) => Some(a.min(b)),
            (a, b) => a.or(b),
        }
    }
}

/// 保守规则：一串没有命令结构的字面词里，`find` 之后又有 `-delete` 时，`find` 之后的**每一个**字面词
/// （以 `-` 开头的、`-delete` 自己都算）与 cwd（`.`）都是可能的删除目标。不切、不数参数、不认 `-exec` 段。
///
/// 返回删除候选从哪个下标开始：多个 `find` 时各自后缀的并集就是「后面有 `-delete` 的最早那个 `find`」
/// 之后的全部词，一遍找到它，不复制后缀（3/F-2、5/F-1）。调用方另加 cwd。
pub fn flat_delete_start<S: AsRef<str>>(words: &[S]) -> Option<usize> {
    let last_delete = words.iter().rposition(|w| w.as_ref() == "-delete")?;
    let first_find = words[..last_delete]
        .iter()
        .position(|w| program_id(w.as_ref()) == FIND)?;
    Some(first_find + 1)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn s(args: &str) -> FindCall<'_> {
        let words: Vec<&str> = args.split(' ').filter(|w| !w.is_empty()).collect();
        split(&words)
    }

    #[test]
    fn split_cases() {
        let c = s("-H /ws -delete");
        assert_eq!(
            (c.roots.clone(), c.delete, c.unregistered),
            (vec!["/ws"], true, false)
        );
        assert_eq!(s("-f /ws -- -delete").roots, vec!["/ws"]);
        let c = s("-Z /ws -delete");
        assert_eq!(
            (c.roots.is_empty(), c.maybe_roots.clone(), c.delete),
            (true, vec!["/ws"], true)
        );
        assert_eq!(c.delete_targets(), vec![".", "/ws"]);
        // -exec 段里的 -delete 不算；谓词参数里的不算
        let c = s(". -exec grep -F -- -delete {} +");
        assert!(!c.delete && c.unregistered);
        let c = s(". -name -delete");
        assert!(!c.delete && !c.unregistered);
        // 段结束之后的 -delete 算
        assert!(s(". -exec rm {} ; -delete").delete);
        // 找不到结束符：按没登记处理，不吞掉后面的 -delete
        let c = s(". -exec rm {} -delete");
        assert!(c.delete && c.unregistered);
        assert_eq!(s("-HL -O3 -D tree . -print").roots, vec!["."]);
        assert_eq!(s("-newer ref").read_targets(), vec![".", "ref"]);
        // + 前面的 {} 必须在段里：-exec 后紧跟 + 不算结束
        let c = s(". -exec + {} + -delete");
        assert!(c.delete && c.unregistered);
        // 没登记的前置选项之后的 -f：值仍算可能的起点（以 - 开头也算）
        let c = s("-Z -f -dir/../.. -- -delete");
        assert!(c.maybe_roots.contains(&"-dir/../..") && c.delete);
    }

    #[test]
    fn flat_delete_every_word_after_find() {
        let w = ["LC_ALL=C", "find", "-f", "-dir/../..", "--", "-delete"];
        assert_eq!(flat_delete_start(&w), Some(2));
        // 多个 find：最早那个之后
        assert_eq!(
            flat_delete_start(&["find", "a", "find", "b", "-delete"]),
            Some(1)
        );
        // 对照：没有 -delete、没有 find、-delete 在 find 前面
        assert_eq!(flat_delete_start(&["find", ".", "-name", "x"]), None);
        assert_eq!(flat_delete_start(&["rm", "-delete"]), None);
        assert_eq!(flat_delete_start(&["-delete", "find", "x"]), None);
    }
}

#[cfg(test)]
mod linear {
    use super::split;

    /// 6/F-4：大量没闭合的 -exec 段，切分线性。旧实现 4000 个要扫 800 万次。
    #[test]
    fn unclosed_exec_segments_are_linear() {
        let mut words = vec!["src"];
        words.extend(std::iter::repeat_n("-exec", 40_000));
        words.push("-delete");
        let t = std::time::Instant::now();
        let call = split(&words);
        assert!(
            t.elapsed() < std::time::Duration::from_secs(1),
            "{:?}",
            t.elapsed()
        );
        assert!(call.delete && call.unregistered);
    }
}
