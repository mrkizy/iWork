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
    /// `eval`、`watch` 先把参数用空格连成一段再交给 shell 执行，引号可以跨参数（closeout 3/F-1）：
    /// 连起来的一整段与每个参数各自一段，两样都入队
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
    ("flock", Rule::EachArgWithCommandOption),
    ("watch", Rule::JoinedAndEachArg),
];

/// 连起来的文字超过上限（[`crate::TEXT_LIMIT`]）：事实收不全，调用方按资源上限处理。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct TooLarge;

/// 一串字面词里要再解析的文字。程序名不要求是第一个词（包装词后面的照样认），
/// 带路径前缀的按最后一段比；同一串里出现几次就处理几次。
///
/// 线性（5/F-1 同类）：逐参数入队时，后出现的程序的后缀是先出现的后缀的子集，只从最早一个起逐个取一次；
/// 连起来的一段每次出现各不相同，先按后缀字节数算总量，超过上限就不拼、返回 [`TooLarge`]。
pub(crate) fn texts_to_reparse(words: &[String]) -> Result<Vec<String>, TooLarge> {
    let mut each_from: Option<usize> = None;
    let mut joined_at: Vec<usize> = Vec::new();
    // 从后往前：后面有没有命令选项（su、flock 要它）
    let mut command_option_after = vec![false; words.len() + 1];
    for i in (0..words.len()).rev() {
        command_option_after[i] = command_option_after[i + 1] || is_command_option(&words[i]);
    }
    for (i, w) in words.iter().enumerate() {
        let program = w.rsplit('/').next().unwrap_or(w);
        let Some((_, rule)) = REPARSE.iter().find(|(p, _)| *p == program) else {
            continue;
        };
        let qualifies = match rule {
            Rule::EachArg => true,
            Rule::JoinedAndEachArg => {
                joined_at.push(i);
                true
            }
            Rule::EachArgWithCommandOption => command_option_after[i + 1],
        };
        if qualifies {
            each_from = Some(each_from.map_or(i + 1, |f| f.min(i + 1)));
        }
    }
    // 后缀字节数（含分隔空格）：连起来那几段的总量先算出来
    let mut suffix_bytes = vec![0usize; words.len() + 1];
    for i in (0..words.len()).rev() {
        suffix_bytes[i] = suffix_bytes[i + 1] + words[i].len() + 1;
    }
    let total = joined_at
        .iter()
        .fold(0usize, |acc, i| acc.saturating_add(suffix_bytes[i + 1]));
    if total > crate::TEXT_LIMIT {
        return Err(TooLarge);
    }
    let mut out: Vec<String> = joined_at.iter().map(|i| words[i + 1..].join(" ")).collect();
    if let Some(from) = each_from {
        each(&words[from..], &mut out);
    }
    Ok(out)
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
    use super::{TooLarge, texts_to_reparse};

    fn t(words: &[&str]) -> Vec<String> {
        let words: Vec<String> = words.iter().map(|w| (*w).to_owned()).collect();
        texts_to_reparse(&words).unwrap()
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
        assert_eq!(t(&["watch", "-n", "1", "ls"]), ["-n 1 ls", "-n", "1", "ls"]);
        assert_eq!(t(&["flock", "/tmp/l", "-c", "x"]), ["/tmp/l", "-c", "x"]);
        assert!(t(&["flock", "/tmp/l", "make"]).is_empty());
        assert!(t(&["git", "commit", "-m", "Document .git/config format"]).is_empty());
        assert!(t(&["grep", "cat ~/.ssh/id_rsa", "notes.md"]).is_empty());
    }

    /// 5/F-1 同类：登记程序名大量重复时线性；连起来的文字总量超过上限时不拼、报超额。
    #[test]
    fn repeated_programs_stay_linear() {
        let words: Vec<String> = std::iter::repeat_n("bash".to_string(), 50_000).collect();
        let t0 = std::time::Instant::now();
        let out = texts_to_reparse(&words).unwrap();
        assert!(t0.elapsed() < std::time::Duration::from_secs(1));
        assert_eq!(out.len(), 49_999);
        let words: Vec<String> = std::iter::repeat_n("eval".to_string(), 20_000).collect();
        assert_eq!(texts_to_reparse(&words), Err(TooLarge));
    }
}
