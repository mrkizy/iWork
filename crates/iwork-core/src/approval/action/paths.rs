//! 路径词的兜底抽取（T07 方案 3.6，第 6、7 轮根因修法）。
//!
//! 解析不了的调用不认参数结构，按词的字面值（去引号、去转义之后）宁可多抽：候选多数是无意义的
//! 相对名字，只有**恰好解析到受保护路径**时才让结果从询问变成拒绝。两条解析不了的路径
//! （`parse` 本身放弃、归类阶段放弃）都用这一个函数；表外程序归类成功时的 `name=value` 拆值
//! 也用同一个 [`split_assignment`]。

use std::collections::BTreeSet;

/// `name=value`（不以 `-` 开头、`=` 前非空）的值部分；值里再有 `=` 原样保留。
pub fn split_assignment(literal: &str) -> Option<&str> {
    if literal.starts_with('-') {
        return None;
    }
    let (name, value) = literal.split_once('=')?;
    (!name.is_empty() && !value.is_empty()).then_some(value)
}

/// 一个字面词的全部候选路径（方案 3.6 表，按顺序累加、去重）。
pub fn fallback_candidates(literal: &str) -> BTreeSet<String> {
    let mut out = BTreeSet::new();
    if literal.is_empty() {
        return out;
    }
    // 完整字面值恒为候选（真实文件名可能就叫 -out、--out=value）
    out.insert(literal.to_owned());
    if let Some(rest) = literal.strip_prefix("--") {
        if let Some((_, value)) = rest.split_once('=')
            && !value.is_empty()
        {
            out.insert(value.to_owned());
        }
    } else if let Some(rest) = literal.strip_prefix('-') {
        // -X值：每一个切分位置之后的剩余部分（组合短选项全覆盖，不查登记表）
        if literal.chars().count() >= 3 {
            for (i, _) in rest.char_indices().skip(1) {
                out.insert(rest[i..].to_owned());
            }
        }
    } else if let Some(value) = split_assignment(literal) {
        out.insert(value.to_owned());
    }
    out
}

/// 不是路径的字面词：空、`-`（标准输入）。
pub fn is_path_word(literal: &str) -> bool {
    !literal.is_empty() && literal != "-"
}

#[cfg(test)]
mod tests {
    use super::*;

    fn c(w: &str) -> Vec<String> {
        fallback_candidates(w).into_iter().collect()
    }

    #[test]
    fn candidates() {
        assert_eq!(c("host"), vec!["host"]);
        assert_eq!(c("-out"), vec!["-out", "t", "ut"]);
        assert_eq!(c("--log=.git/x"), vec!["--log=.git/x", ".git/x"]);
        assert_eq!(c("of=.git/config"), vec![".git/config", "of=.git/config"]);
        assert_eq!(c("A=b=c"), vec!["A=b=c", "b=c"]);
        let e = c("-vE.git/config");
        assert!(e.contains(&".git/config".to_string()) && e.contains(&"E.git/config".to_string()));
        assert_eq!(c("-p2222"), vec!["-p2222", "2", "22", "222", "2222"]);
        assert_eq!(c("-v"), vec!["-v"]);
        assert!(c("").is_empty());
    }

    #[test]
    fn assignment() {
        assert_eq!(split_assignment("OUT=.git/x"), Some(".git/x"));
        assert_eq!(split_assignment("=x"), None);
        assert_eq!(split_assignment("x="), None);
        assert_eq!(split_assignment("--a=b"), None);
        assert_eq!(split_assignment("plain"), None);
    }
}
