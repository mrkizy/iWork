//! 词的字面性：只认「不经任何展开就能确定值」的词（T07 方案 3.3、3.5）。
//!
//! 判断写在这里而不是用解析器的词分片，是为了让「什么算字面」的规则只有一处、短到能逐行审：
//! 拿不准的一律不算字面（调用方据此把整条调用判为解析不了，三档询问）。

/// 词的字面值：去掉引号与转义之后的文字。不是字面词返回 `None`。
///
/// 字面词只由下面几种片段组成：
/// - 不加引号的普通字符，`\x` 转义（取 `x`；`\` 加换行是续行，什么都不留）；
/// - 单引号内的任何内容；
/// - 双引号内不含 `$`、反引号的内容，`\` 只转义 `$`、反引号、`"`、`\`、换行（其余 `\` 原样保留）。
///
/// 不加引号时出现下列任何一种都不是字面词：`$`（变量、命令替换、算术、`$'…'`、`$"…"`）、反引号、
/// 通配 `*` `?` `[`、圆括号（extglob、进程替换残片）、含 `,` 或 `..` 的花括号（花括号展开）、
/// `~` 后面跟着用户名（`~user`；`~` 与 `~/…` 允许，由路径解析展开主目录）。
pub fn literal_value(raw: &str) -> Option<String> {
    let chars: Vec<char> = raw.chars().collect();
    let mut out = String::new();
    // 不加引号的部分，花括号展开判断用
    let mut unquoted = String::new();
    let mut i = 0;
    while i < chars.len() {
        let c = chars[i];
        match c {
            '\\' => {
                let next = *chars.get(i + 1)?;
                if next != '\n' {
                    out.push(next);
                }
                i += 2;
            }
            '\'' => {
                let end = find(&chars, i + 1, '\'')?;
                out.extend(&chars[i + 1..end]);
                i = end + 1;
            }
            '"' => {
                i += 1;
                loop {
                    let d = *chars.get(i)?;
                    match d {
                        '"' => break,
                        '$' | '`' => return None,
                        '\\' => {
                            let next = *chars.get(i + 1)?;
                            match next {
                                '$' | '`' | '"' | '\\' => out.push(next),
                                '\n' => {}
                                other => {
                                    out.push('\\');
                                    out.push(other);
                                }
                            }
                            i += 2;
                        }
                        other => {
                            out.push(other);
                            i += 1;
                        }
                    }
                }
                i += 1;
            }
            '$' | '`' | '*' | '?' | '[' | '(' | ')' => return None,
            '~' if tilde_names_user(&chars, i) => return None,
            other => {
                out.push(other);
                unquoted.push(other);
                i += 1;
            }
        }
    }
    if looks_like_brace_expansion(&unquoted) {
        return None;
    }
    // 引号或转义里的 `~` 不展开（Bash 只展开词首未引用的 `~`）：写成 `./~…`，路径解析就不会把它当主目录
    if out.starts_with('~') && !raw.starts_with('~') {
        out.insert_str(0, "./");
    }
    Some(out)
}

fn find(chars: &[char], from: usize, target: char) -> Option<usize> {
    (from..chars.len()).find(|&j| chars[j] == target)
}

/// 词首、或紧跟 `=` / `:` 之后的不加引号的 `~`：后面是 `/`、`=`、`:` 或词尾才是主目录，
/// 否则是 `~user`、`~+`、`~-` 之类，值取决于环境。
fn tilde_names_user(chars: &[char], i: usize) -> bool {
    let at_prefix = i == 0 || matches!(chars[i - 1], '=' | ':');
    if !at_prefix {
        return false;
    }
    !matches!(chars.get(i + 1), None | Some('/') | Some(':'))
}

/// 词里任何一处不加引号的 `{…}`（含嵌套、含前面已有字面 `{}` 的情形）出现 `,` 或 `..` 就按花括号展开
/// 处理，拿不准就不算字面；`{}`（`find -exec … {} \;`）与 `@{u}` 这类不是展开。
///
/// 判定等价于「存在 `{` 在前、`,` 或 `..` 在中间、`}` 在后」，单遍扫描，时间与词长成正比（2/F-8）：
/// 见过 `{` 之后再见到分隔符就记下，之后任何一个 `}` 都让它成立。
fn looks_like_brace_expansion(unquoted: &str) -> bool {
    let mut seen_open = false;
    let mut separator_after_open = false;
    let mut prev = '\0';
    for c in unquoted.chars() {
        match c {
            '{' => seen_open = true,
            ',' if seen_open => separator_after_open = true,
            '.' if seen_open && prev == '.' => separator_after_open = true,
            '}' if separator_after_open => return true,
            _ => {}
        }
        prev = c;
    }
    false
}

#[cfg(test)]
mod tests {
    use super::literal_value as lit;

    #[test]
    fn literal_forms() {
        assert_eq!(lit("abc").as_deref(), Some("abc"));
        assert_eq!(lit("'a b'").as_deref(), Some("a b"));
        assert_eq!(lit("\"a b\"").as_deref(), Some("a b"));
        assert_eq!(lit("-E\".git/config\"").as_deref(), Some("-E.git/config"));
        assert_eq!(lit("\"-E.git/config\"").as_deref(), Some("-E.git/config"));
        assert_eq!(lit("-E.git/con\\fig").as_deref(), Some("-E.git/config"));
        assert_eq!(lit("of=\".git/config\"").as_deref(), Some("of=.git/config"));
        assert_eq!(lit("~/x").as_deref(), Some("~/x"));
        assert_eq!(lit("~").as_deref(), Some("~"));
        assert_eq!(lit("--output=~/x").as_deref(), Some("--output=~/x"));
        assert_eq!(lit("{}").as_deref(), Some("{}"));
        assert_eq!(lit("@{u}").as_deref(), Some("@{u}"));
        assert_eq!(lit("\"a\\nb\"").as_deref(), Some("a\\nb"));
        assert_eq!(lit("'*'").as_deref(), Some("*"));
        // 引号 / 转义里的 ~ 是字面，不是主目录（审查 1/F-5）
        assert_eq!(lit("'~/config'").as_deref(), Some("./~/config"));
        assert_eq!(lit("\"~\"/x").as_deref(), Some("./~/x"));
        assert_eq!(lit("\\~/x").as_deref(), Some("./~/x"));
        assert_eq!(lit("~/x").as_deref(), Some("~/x"));
        assert_eq!(lit("{}{x}").as_deref(), Some("{}{x}"));
        assert_eq!(lit("a,b}").as_deref(), Some("a,b}"));
        assert_eq!(lit("{a}.").as_deref(), Some("{a}."));
    }

    /// 2/F-8：大量字面 `{}` 时成本线性。64 KiB 的 `{}` 重复在调试构建下也应远小于一秒
    /// （旧实现三次复杂度，6400 字节就要十几秒）。
    #[test]
    fn brace_check_is_linear() {
        let word = "{}".repeat(32 * 1024);
        let t = std::time::Instant::now();
        assert_eq!(lit(&word).as_deref(), Some(word.as_str()));
        let spent = t.elapsed();
        assert!(spent < std::time::Duration::from_secs(1), "耗时 {spent:?}");
    }

    #[test]
    fn non_literal_forms() {
        for raw in [
            "$HOME",
            "${X}",
            "$(ls)",
            "`ls`",
            "$((1+2))",
            "\"$X\"",
            "\"`ls`\"",
            "$'a'",
            "*.rs",
            "a?",
            "[ab]",
            "~root/x",
            "X=~root",
            "{a,b}",
            "{1..3}",
            // 审查 1/F-11：前面有字面 {} 或 {x}，后面才是展开
            "{}{a,b}",
            "{x}{1..3}",
            "a{b}c{d,e}",
            "{{a,b}}",
            "@(a|b)",
            "'unterminated",
            "\"unterminated",
            "trailing\\",
        ] {
            assert_eq!(lit(raw), None, "{raw}");
        }
    }
}
