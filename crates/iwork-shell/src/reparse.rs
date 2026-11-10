//! 字符串参数里的命令再解析登记表（方案 3.3「shell 程序后面的字符串参数，进队列再解析」）。
//!
//! 只认这张固定的小表，不按「含空白就像命令」猜：提交说明、`grep` 的模式不是命令（closeout 1/F-2）。
//! 不去定位哪个参数是命令串，也不照 Bash 的选项语法取词（`bash -c -e '…'` 里 Bash 会接着消费 `-c`
//! 后面的选项，closeout 2/F-2）：登记的程序后面**每个**字面参数各自入队。加一种 = 加一行。

/// 一个登记程序后面的参数怎么入队。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Rule {
    /// 后面每个字面参数各自一段
    EachArg,
    /// `eval` 先把参数用空格连成一段再执行，引号可以跨参数（closeout 3/F-1）：连起来的一整段与
    /// 每个参数各自一段，两样都入队
    JoinedAndEachArg,
    /// 后面有 `-c`（含 `c` 的短选项组）、`--command`、`--command=…` 时，每个字面参数各自一段
    EachArgWithCommandOption,
}

const REPARSE: &[(&str, Rule)] = &[
    ("sh", Rule::EachArg),
    ("bash", Rule::EachArg),
    ("zsh", Rule::EachArg),
    ("dash", Rule::EachArg),
    ("ksh", Rule::EachArg),
    ("eval", Rule::JoinedAndEachArg),
    ("su", Rule::EachArgWithCommandOption),
];

/// 一串字面词里要再解析的文字。程序名不要求是第一个词（包装词后面的照样认），
/// 带路径前缀的按最后一段比；同一串里出现几次就处理几次。
pub(crate) fn texts_to_reparse(words: &[String]) -> Vec<String> {
    let mut out = Vec::new();
    for (i, w) in words.iter().enumerate() {
        let program = w.rsplit('/').next().unwrap_or(w);
        let Some((_, rule)) = REPARSE.iter().find(|(p, _)| *p == program) else {
            continue;
        };
        let rest = &words[i + 1..];
        match rule {
            Rule::EachArg => each(rest, &mut out),
            Rule::JoinedAndEachArg => {
                out.push(rest.join(" "));
                each(rest, &mut out);
            }
            Rule::EachArgWithCommandOption => {
                if rest.iter().any(|a| is_command_option(a)) {
                    each(rest, &mut out);
                }
            }
        }
    }
    out
}

/// 每个参数一段；形如 `--xxx=值` 的，`=` 后面的值也入队（`su --command='…'`）。
fn each(rest: &[String], out: &mut Vec<String>) {
    for a in rest {
        out.push(a.clone());
        if let Some(long) = a.strip_prefix("--")
            && let Some((_, value)) = long.split_once('=')
        {
            out.push(value.to_owned());
        }
    }
}

fn is_command_option(a: &str) -> bool {
    a == "--command"
        || a.starts_with("--command=")
        || (a.starts_with('-') && !a.starts_with("--") && a[1..].contains('c'))
}

#[cfg(test)]
mod tests {
    use super::texts_to_reparse;

    fn t(words: &[&str]) -> Vec<String> {
        let words: Vec<String> = words.iter().map(|w| (*w).to_owned()).collect();
        texts_to_reparse(&words)
    }

    #[test]
    fn registered_programs_queue_every_literal_arg() {
        assert_eq!(t(&["bash", "-c", "-e", "x"]), ["-c", "-e", "x"]);
        assert_eq!(t(&["env", "X=1", "/bin/sh", "-lc", "y"]), ["-lc", "y"]);
        assert_eq!(t(&["eval", "a", "b"]), ["a b", "a", "b"]);
        assert_eq!(
            t(&["su", "root", "--command=z"]),
            ["root", "--command=z", "z"]
        );
        assert_eq!(t(&["su", "-c", "z", "root"]), ["-c", "z", "root"]);
        // 对照：su 没有命令选项、不在表里的程序都不入队
        assert!(t(&["su", "root"]).is_empty());
        assert!(t(&["git", "commit", "-m", "Document .git/config format"]).is_empty());
        assert!(t(&["grep", "cat ~/.ssh/id_rsa", "notes.md"]).is_empty());
    }
}
