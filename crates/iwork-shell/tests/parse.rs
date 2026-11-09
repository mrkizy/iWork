//! `iwork-shell::parse` 的行为：能表达的形态、整条解析不了的形态、放弃前保留的字面事实。

use iwork_shell::{
    CommandNode, Connector, PartialFacts, RedirectKind, ShellScript, SimpleCommand, Unparsable,
    parse,
};
use proptest::prelude::*;

fn ok(text: &str) -> ShellScript {
    parse(text).unwrap_or_else(|u| panic!("{text:?} 应能解析：{}", u.why))
}

fn err(text: &str) -> Unparsable {
    match parse(text) {
        Ok(s) => panic!("{text:?} 应解析不了，却得到 {s:?}"),
        Err(u) => u,
    }
}

/// 全部简单命令（展平复合命令），按出现顺序。
fn simples(s: &ShellScript) -> Vec<&SimpleCommand> {
    let mut out = Vec::new();
    fn walk<'a>(s: &'a ShellScript, out: &mut Vec<&'a SimpleCommand>) {
        for item in &s.items {
            for c in &item.pipeline {
                match c {
                    CommandNode::Simple(sc) => out.push(sc),
                    CommandNode::Subshell(x) | CommandNode::BraceGroup(x) => walk(x, out),
                    CommandNode::If { branches } => branches.iter().for_each(|b| walk(b, out)),
                    CommandNode::Loop { condition, body } => {
                        walk(condition, out);
                        walk(body, out);
                    }
                }
            }
        }
    }
    walk(s, &mut out);
    out
}

fn words(sc: &SimpleCommand) -> Vec<&str> {
    sc.words.iter().map(|w| w.literal.as_str()).collect()
}

#[test]
fn shell_simple_command_words_are_literal() {
    let s = ok(r#"grep -n "fn main" 'src/a b.rs'"#);
    assert_eq!(s.items.len(), 1);
    assert_eq!(
        words(simples(&s)[0]),
        vec!["grep", "-n", "fn main", "src/a b.rs"]
    );
    assert_eq!(simples(&s)[0].words[2].raw, r#""fn main""#);
    assert_eq!(s.items[0].connector, None);
}

#[test]
fn shell_lists_pipelines_and_connectors() {
    let s = ok("cat a | wc -l && echo ok || echo no; sleep 1 &");
    let connectors: Vec<_> = s.items.iter().map(|i| i.connector).collect();
    assert_eq!(
        connectors,
        vec![
            Some(Connector::And),
            Some(Connector::Or),
            Some(Connector::Seq),
            Some(Connector::Async)
        ]
    );
    assert_eq!(s.items[0].pipeline.len(), 2);
    assert_eq!(
        simples(&s).iter().map(|c| words(c)[0]).collect::<Vec<_>>(),
        vec!["cat", "wc", "echo", "echo", "sleep"]
    );
    assert!(ok("! true").items[0].negated);
}

#[test]
fn shell_redirects() {
    let s = ok("echo x > out.txt 2>>err.log < in.txt >&2 2>/dev/null &> both");
    let r: Vec<_> = simples(&s)[0]
        .redirects
        .iter()
        .map(|r| (r.kind, r.fd, r.target.literal.as_str()))
        .collect();
    assert_eq!(
        r,
        vec![
            (RedirectKind::Write, None, "out.txt"),
            (RedirectKind::Append, Some(2), "err.log"),
            (RedirectKind::Read, None, "in.txt"),
            (RedirectKind::Write, Some(2), "/dev/null"),
            (RedirectKind::Write, None, "both"),
        ]
    );
    // 只有重定向、没有命令名
    let s = ok("> a.txt");
    assert!(simples(&s)[0].words.is_empty());
    assert_eq!(simples(&s)[0].redirects[0].target.literal, "a.txt");
}

#[test]
fn shell_compound_without_redirects() {
    let s = ok(
        "(cd x; ls) && { echo a; } && if true; then echo b; elif false; then echo c; else echo d; fi && while false; do echo e; done",
    );
    assert_eq!(
        simples(&s)
            .iter()
            .map(|c| words(c).join(" "))
            .collect::<Vec<_>>(),
        vec![
            "cd x", "ls", "echo a", "true", "echo b", "false", "echo c", "echo d", "false",
            "echo e"
        ]
    );
}

#[test]
fn shell_quoted_heredoc_is_fine() {
    let s = ok("cat > a.txt <<'EOF'\n$HOME `x`\nEOF\n");
    let sc = simples(&s)[0];
    assert_eq!(words(sc), vec!["cat"]);
    assert_eq!(sc.redirects.len(), 1, "here-doc 本身不产生文件目标");
}

/// 方案 3.3：每种表达不了的形态各一条。
#[test]
fn shell_unparsable_forms() {
    for (text, why) in [
        ("for f in a b; do echo $f; done", "for"),
        ("select x in a; do :; done", ""),
        ("case x in a) echo;; esac", "case"),
        ("f() { :; }", "函数"),
        ("{ printf x; } > out", "复合命令上的重定向"),
        ("(ls) > out", "复合命令上的重定向"),
        ("coproc cat", ""),
        ("(( x = 1 ))", "算术"),
        ("[[ -f a ]]", "[["),
        ("time ls", "time"),
        ("echo $HOME", "展开"),
        ("echo ${X}", "展开"),
        ("echo $(git push)", "展开"),
        ("echo `git push`", "展开"),
        ("echo $((1+2))", "展开"),
        ("cat <(ls)", ""),
        ("ls ~root", "展开"),
        ("rm *.o", "展开"),
        ("ls {a,b}", "展开"),
        ("cat <<EOF\nx\nEOF\n", "here-doc"),
        ("cat <<< hi", "here-string"),
        ("X=1 ls", "前置环境变量赋值"),
        ("X=1", "前置环境变量赋值"),
        ("echo $'a'", "展开"),
        ("echo 'unterminated", ""),
        ("echo hi >& out", "文件描述符复制"),
        ("if true; then echo; fi > out", "复合命令上的重定向"),
    ] {
        let u = err(text);
        assert!(
            u.why.contains(why),
            "{text:?} 的原因是 {:?}，应含 {why:?}",
            u.why
        );
    }
}

/// 第 2 轮复审 2/F-2：解析放弃前可靠识别的字面重定向与字面词随 Unparsable 带回。
#[test]
fn partial_facts_survive_unparsable() {
    type Case<'a> = (&'a str, &'a [(RedirectKind, &'a str)], &'a [&'a str]);
    let cases: &[Case] = &[
        (
            "{ printf x; } > .git/iwork-marker",
            &[(RedirectKind::Write, ".git/iwork-marker")],
            &["printf", "x"],
        ),
        (
            "for f in $(ls) a; do :; done > .git/x",
            &[(RedirectKind::Write, ".git/x")],
            &["a", ":"],
        ),
        (
            "f() { :; } > .git/x",
            &[(RedirectKind::Write, ".git/x")],
            &[":"],
        ),
        (
            "LC_ALL=C ssh -v -E.git/config host",
            &[],
            &["ssh", "LC_ALL=C", "-v", "-E.git/config", "host"],
        ),
        (
            r#"ssh -v -E.git/config host; echo "$UNUSED""#,
            &[],
            &["ssh", "-v", "-E.git/config", "host", "echo"],
        ),
        (
            r#"LC_ALL=C ssh -E".git/config" host"#,
            &[],
            &["ssh", "LC_ALL=C", "-E.git/config", "host"],
        ),
        (
            "case x in a) cat ~/.ssh/id_rsa >> out;; esac",
            &[(RedirectKind::Append, "out")],
            &["x", "a", "cat", "~/.ssh/id_rsa"],
        ),
        (
            "[[ -f a ]] > .git/y",
            &[(RedirectKind::Write, ".git/y")],
            &["a"],
        ),
        (
            "LC_ALL=C printf x > -out",
            &[(RedirectKind::Write, "-out")],
            &["printf", "LC_ALL=C", "x"],
        ),
    ];
    for (text, redirects, words) in cases {
        let u = err(text);
        let expected = PartialFacts {
            literal_redirects: redirects.iter().map(|(k, t)| (*k, t.to_string())).collect(),
            literal_words: words.iter().map(|w| w.to_string()).collect(),
        };
        assert_eq!(u.facts, expected, "{text:?}");
    }
    // 语法错误：第三方解析器整体失败，词法分析切出的字面词仍在
    let u = err("cat ~/.ssh/id_rsa && && ls");
    assert!(
        u.facts.literal_words.contains(&"~/.ssh/id_rsa".to_string()),
        "{u:?}"
    );
    // 对照：含展开的词不收
    let u = err("cat $HOME/x");
    assert_eq!(u.facts.literal_words, vec!["cat".to_string()]);
}

proptest! {
    /// 任意字符串都不 panic：要么小 AST，要么 Unparsable。
    #[test]
    fn shell_parse_never_panics(text in ".{0,200}") {
        let _ = parse(&text);
    }

    /// 从 Bash 片段随机拼出来的脚本也不 panic；含展开、通配、前置赋值的一定解析不了。
    #[test]
    fn shell_parse_fragments(parts in prop::collection::vec(fragment(), 1..6), seps in prop::collection::vec(sep(), 6)) {
        let mut text = String::new();
        for (i, (p, _)) in parts.iter().enumerate() {
            text.push_str(p);
            if i + 1 < parts.len() {
                text.push_str(seps[i]);
            }
        }
        let tainted = parts.iter().any(|(_, t)| *t);
        let result = parse(&text);
        if tainted {
            prop_assert!(result.is_err(), "{text:?} 含展开却解析成功：{result:?}");
        }
    }
}

/// (片段, 是否一定让整条解析不了)
fn fragment() -> impl Strategy<Value = (String, bool)> {
    prop_oneof![
        Just(("ls -la".to_string(), false)),
        Just(("cat 'a b' > out".to_string(), false)),
        Just(("(echo x)".to_string(), false)),
        Just(("echo $X".to_string(), true)),
        Just(("echo $(ls)".to_string(), true)),
        Just(("rm *.o".to_string(), true)),
        Just(("A=1 make".to_string(), true)),
        Just(("{ echo; } > x".to_string(), true)),
        Just(("for i in a; do :; done".to_string(), true)),
        Just(("cat <<EOF\nx\nEOF\n".to_string(), true)),
        "[a-z/.~ -]{1,12}".prop_map(|s| (s, false)),
    ]
}

fn sep() -> impl Strategy<Value = &'static str> {
    prop::sample::select(vec![" && ", " || ", "; ", " | ", "\n"])
}
