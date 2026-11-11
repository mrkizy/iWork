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
            &["LC_ALL=C", "ssh", "-v", "-E.git/config", "host"],
        ),
        (
            r#"ssh -v -E.git/config host; echo "$UNUSED""#,
            &[],
            &["ssh", "-v", "-E.git/config", "host", "echo"],
        ),
        (
            r#"LC_ALL=C ssh -E".git/config" host"#,
            &[],
            &["LC_ALL=C", "ssh", "-E.git/config", "host"],
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
            &["LC_ALL=C", "printf", "x"],
        ),
    ];
    for (text, redirects, words) in cases {
        let u = err(text);
        let redirects: Vec<(RedirectKind, String)> =
            redirects.iter().map(|(k, t)| (*k, t.to_string())).collect();
        let words: Vec<String> = words.iter().map(|w| w.to_string()).collect();
        assert_eq!(u.facts.literal_redirects, redirects, "{text:?}");
        assert_eq!(u.facts.literal_words, words, "{text:?}");
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

fn facts(text: &str) -> PartialFacts {
    iwork_shell::literal_facts(text)
}

fn has_redirect(f: &PartialFacts, kind: RedirectKind, target: &str) -> bool {
    f.literal_redirects
        .iter()
        .any(|(k, t)| *k == kind && t == target)
}

/// 9/F-7：替换体在任何词里都收——参数展开的操作数、算术展开、here-string、可展开 here-doc、
/// 整体语法错时词法切出的词。对照：没有替换体的写法收不到。
#[test]
fn substitution_found_in_any_word() {
    for text in [
        "echo $(printf x > .git/config)",
        "echo `printf x > .git/config`",
        "echo ${x:-$(printf x > .git/config)}",
        "echo $(( $(printf x > .git/config; echo 1) + 1 ))",
        "cat <<< \"$(printf x > .git/config)\"",
        "cat <<EOF\n$(printf x > .git/config)\nEOF\n",
        "echo \"a $(printf x > .git/config) b\"",
        "printf x > .git/config\nif\n",
        "<(printf x > .git/config) cat",
    ] {
        let f = facts(text);
        assert!(
            has_redirect(&f, RedirectKind::Write, ".git/config")
                || f.literal_words.iter().any(|w| w == ".git/config"),
            "{text:?}：{f:?}"
        );
    }
    // 对照
    let f = facts("echo ${x:-y} $((1 + 2))");
    assert!(f.literal_redirects.is_empty(), "{f:?}");
}

/// 8/F-1：没有深度上限，10 层嵌套照样收到。
#[test]
fn deep_nesting_has_no_depth_limit() {
    let mut text = "printf x > .git/config".to_string();
    for _ in 0..10 {
        text = format!("echo $({text})");
    }
    let f = facts(&text);
    assert!(
        has_redirect(&f, RedirectKind::Write, ".git/config"),
        "{f:?}"
    );
    assert!(!f.resource_limit);
}

/// 资源上限：总文字超过 256 KiB 置 resource_limit；以下不置。
#[test]
fn resource_limit_flags_oversized_text() {
    let big = format!("echo {}", "a".repeat(iwork_shell::TEXT_LIMIT));
    assert!(facts(&big).resource_limit);
    assert!(parse(&big).unwrap_err().facts.resource_limit);
    let small = format!("echo {}", "a".repeat(1000));
    assert!(!facts(&small).resource_limit);
    // 入队文字也算：原文不大，嵌套展开出来的总量超了
    let chunk = "b".repeat(60 * 1024);
    let nested = format!("eval '{chunk}' '{chunk}' '{chunk}'");
    assert!(facts(&nested).resource_limit);
}

/// 登记表里的 shell 程序：后面每个字面参数再解析，包装词后面的也认；eval 另加连起来的一段。
#[test]
fn nested_command_text_keeps_facts() {
    for text in [
        "eval \"printf x > .git/config\"",
        "bash -c 'printf x > .git/config'",
        "env X=1 bash -lc 'printf x > .git/config'",
        "sudo sh -ec 'printf x > .git/config'",
        "timeout 5 bash -o pipefail -c 'printf x > .git/config'",
        "bash -c -e 'printf x > .git/config'",
        "bash -c -- 'printf x > .git/config'",
        "bash -c -o pipefail 'printf x > .git/config'",
        "su -c 'printf x > .git/config' root",
        "su root --command='printf x > .git/config'",
        "eval printf x '>' .git/config",
        "eval \"eval \\\"eval 'printf x > .git/config'\\\"\"",
        "LC_ALL=C bash -c 'printf x > .git/config'; echo $V",
    ] {
        let f = facts(text);
        assert!(
            has_redirect(&f, RedirectKind::Write, ".git/config"),
            "{text:?}：{f:?}"
        );
    }
    // eval 的引号跨参数：连起来才看得到重定向目标
    let f = facts(r#"eval 'printf changed > "/tmp/probe/repo' 'space/.git/config"'"#);
    assert!(
        has_redirect(&f, RedirectKind::Write, "/tmp/probe/repo space/.git/config"),
        "{f:?}"
    );
    // 嵌套文字词法失败：粗切，仍收到 .git/config
    let f = facts("eval \"printf x > .git/config\necho 'abc\"");
    assert!(f.queued_words.iter().any(|w| w == ".git/config"), "{f:?}");
    assert!(!f.lex_failed);
    // 队列里收来的词进 queued_words，原文的词进 literal_words
    let f = facts("bash -c 'find . -delete'");
    assert!(f.queued_words.iter().any(|w| w == "-delete"), "{f:?}");
    assert!(!f.literal_words.iter().any(|w| w == "-delete"), "{f:?}");
}

/// closeout 1/F-2：不在登记表程序后面的字符串参数不再解析。
#[test]
fn plain_string_args_are_not_reparsed() {
    for text in [
        "env LC_ALL=C git commit -m 'Document .git/config format'",
        "env LC_ALL=C printf '%s\\n' 'Document .git/config format'",
        "env LC_ALL=C grep 'cat ~/.ssh/id_rsa' notes.md",
        "sudo echo 'rm -rf ~/.ssh'",
    ] {
        let f = facts(text);
        assert!(f.queued_words.is_empty(), "{text:?}：{f:?}");
        assert!(f.literal_redirects.is_empty(), "{text:?}：{f:?}");
    }
}

/// 收口补定：整条命令原文词法都切不出时置 lex_failed，不抽事实。
#[test]
fn unlexable_original_text() {
    for text in ["printf x > .git/config\nls\necho 'abc", "echo \"abc"] {
        let u = parse(text).unwrap_err();
        assert!(u.facts.lex_failed, "{text:?}：{u:?}");
    }
    // 对照：语法错但词法切得出
    let u = parse("printf x > .git/config\nif\n").unwrap_err();
    assert!(!u.facts.lex_failed);
}

proptest! {
    /// 把替换体塞进随机脚本里会展开的位置，第一遍都收得到（方案 11.2 substitution_found_in_any_word 的 shell 层部分）。
    #[test]
    fn substitution_in_random_script(
        parts in prop::collection::vec(fragment(), 1..4),
        wrap in prop::sample::select(vec![
            "$(S)", "`S`", "${x:-$(S)}", "$(( $(S; echo 1) + 1 ))", "\"a $(S) b\"", "$(echo $(S))",
        ]),
        at in 0usize..4,
    ) {
        let inner = wrap.replace('S', "printf x > .git/config");
        let mut words: Vec<String> = parts.into_iter().map(|(p, _)| p).collect();
        let at = at.min(words.len());
        words.insert(at, format!("echo {inner}"));
        let text = words.join("; ");
        let f = facts(&text);
        prop_assert!(has_redirect(&f, RedirectKind::Write, ".git/config"), "{text:?}：{f:?}");
    }

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
