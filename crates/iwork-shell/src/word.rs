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

/// 不加引号的 `{…}` 里出现 `,` 或 `..` 就按花括号展开处理；`{}`（`find -exec … {} \;`）与
/// `@{u}` 这类不是展开。
fn looks_like_brace_expansion(unquoted: &str) -> bool {
    let Some(open) = unquoted.find('{') else {
        return false;
    };
    let Some(close) = unquoted[open..].find('}') else {
        return false;
    };
    let inner = &unquoted[open + 1..open + close];
    inner.contains(',') || inner.contains("..")
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
            "@(a|b)",
            "'unterminated",
            "\"unterminated",
            "trailing\\",
        ] {
            assert_eq!(lit(raw), None, "{raw}");
        }
    }
}
