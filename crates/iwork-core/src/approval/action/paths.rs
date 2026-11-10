//! 路径词的兜底抽取（T07 方案 3.6，第 6、7 轮根因修法）。
//!
//! 解析不了的调用不认参数结构，按词的字面值（去引号、去转义之后）宁可多抽：候选多数是无意义的
//! 相对名字，只有**恰好解析到受保护路径**时才让结果从询问变成拒绝。两条解析不了的路径
//! （`parse` 本身放弃、归类阶段放弃）都用这一个函数；表外程序归类成功时的 `name=value` 拆值
//! 也用同一个 [`split_assignment`]。

use std::collections::BTreeSet;

use iwork_domain::action::CanonicalPath;
use iwork_ports::fs::ResolvedPath;

/// 一个路径目标要检查的位置（方案 3.5「链接的两种视图都过底线」，一处真值）：先 `entry`（末级不跟随）
/// 后 `target`（完全解析），相同时只出一个。不看动作种类：读、写、删都取两种视图，所有产出路径动作的
/// 地方都经这里。
pub fn views(r: &ResolvedPath) -> impl Iterator<Item = &CanonicalPath> {
    std::iter::once(&r.entry).chain((r.entry != r.target).then_some(&r.target))
}

/// 目录目标语义的假设候选 `dir / basename(source)`（方案 3.4）。源的末级名去掉尾斜杠后取；
/// 末级是 `.`、`..` 或为空时没有候选（写入的就是目的本身）。
pub fn join_basename(dir: &str, source: &str) -> Option<String> {
    basename(source).map(|base| join_dir(dir, base))
}

/// 源的末级名（去掉尾斜杠）；是 `.`、`..` 或为空时没有（写入的就是目的本身）。
pub fn basename(source: &str) -> Option<&str> {
    let base = source.trim_end_matches('/').rsplit('/').next()?;
    (!base.is_empty() && base != "." && base != "..").then_some(base)
}

/// `dir / base`。
pub fn join_dir(dir: &str, base: &str) -> String {
    if dir.ends_with('/') {
        format!("{dir}{base}")
    } else {
        format!("{dir}/{base}")
    }
}

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
    // 词中间切出来的候选以 ~ 开头时，Bash 是否展开取决于上下文（`name=~/x` 展开、`--o=~/x` 不展开），
    // 两种都记（审查 1/F-5）；完整字面值的 ~ 资格已由 iwork-shell 定好
    let derived: Vec<String> = out
        .iter()
        .filter(|c| c.as_str() != literal && c.starts_with('~'))
        .map(|c| format!("./{c}"))
        .collect();
    out.extend(derived);
    out
}

/// 从词中间切出来的值（`name=值`、`--opt=值`）的全部解读：以 ~ 开头时主目录与字面 ~ 两种都要过检查。
pub fn value_variants(value: &str) -> Vec<String> {
    if value.starts_with('~') {
        vec![value.to_owned(), format!("./{value}")]
    } else {
        vec![value.to_owned()]
    }
}

/// 不是路径的字面词只有空串。`-` 是真实文件名（重定向 `> -`、兜底候选都要解析它）；
/// 只有明确把 `-` 当标准流的位置（白名单程序的操作数）由调用方排除（审查 1/F-4）。
pub fn is_path_word(literal: &str) -> bool {
    !literal.is_empty()
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
        assert_eq!(c("X=~/a"), vec!["./~/a", "X=~/a", "~/a"]);
        assert_eq!(c("-"), vec!["-"]);
        assert!(c("").is_empty());
    }

    #[test]
    fn basename_join() {
        assert_eq!(
            join_basename(".", "src/config").as_deref(),
            Some("./config")
        );
        assert_eq!(join_basename("dir/", "a").as_deref(), Some("dir/a"));
        assert_eq!(join_basename("d", "src/").as_deref(), Some("d/src"));
        assert_eq!(join_basename("d", "."), None);
        assert_eq!(join_basename("d", "../.."), None);
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
