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
            if let Some(end) = exec_segment_end(args, i + 1) {
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

/// `-exec` 段的结束位置：`;`，或紧跟在 `{}` 后面的 `+`。
fn exec_segment_end(args: &[&str], from: usize) -> Option<usize> {
    (from..args.len())
        .find(|&j| args[j] == ";" || (args[j] == "+" && j > from && args[j - 1] == "{}"))
}

/// 保守规则：一串没有命令结构的字面词里，`find` 之后又有 `-delete` 时，`find` 之后的**每一个**字面词
/// （以 `-` 开头的、`-delete` 自己都算）与 cwd（`.`）都是可能的删除目标。不切、不数参数、不认 `-exec` 段。
pub fn flat_delete<S: AsRef<str>>(words: &[S]) -> Vec<String> {
    let mut out = Vec::new();
    for (i, w) in words.iter().enumerate() {
        if program_id(w.as_ref()) != FIND {
            continue;
        }
        let rest = &words[i + 1..];
        if rest.iter().any(|r| r.as_ref() == "-delete") {
            out.extend(rest.iter().map(|r| r.as_ref().to_owned()));
            out.push(".".to_owned());
        }
    }
    out.sort();
    out.dedup();
    out
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
        // 没登记的前置选项之后的 -f：值仍算可能的起点（以 - 开头也算）
        let c = s("-Z -f -dir/../.. -- -delete");
        assert!(c.maybe_roots.contains(&"-dir/../..") && c.delete);
    }

    #[test]
    fn flat_delete_every_word_after_find() {
        let w = ["LC_ALL=C", "find", "-f", "-dir/../..", "--", "-delete"];
        assert_eq!(
            flat_delete(&w),
            vec!["--", "-delete", "-dir/../..", "-f", "."]
        );
        // 对照：没有 -delete、没有 find
        assert!(flat_delete(&["find", ".", "-name", "x"]).is_empty());
        assert!(flat_delete(&["rm", "-delete"]).is_empty());
    }
}
