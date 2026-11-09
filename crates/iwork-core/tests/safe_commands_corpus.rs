//! 白名单程序的参数规则（T07 方案 3.4 出事场景 N1）：每个程序「登记形态归读写删 / 未登记选项变执行」，
//! 外加取值选项的路径角色。

mod support;

use iwork_core::approval::action::commands::SAFE_COMMANDS;
use iwork_domain::action::ActionKind;
use support::*;

fn kinds(cmd: &str) -> Vec<ActionKind> {
    norm(&bash_req(cmd))
        .actions
        .iter()
        .map(|a| a.kind)
        .collect()
}

/// (登记形态, 期望种类) —— 每个白名单程序至少一条。
const SAFE: &[(&str, ActionKind)] = &[
    ("cat -n a", ActionKind::Read),
    ("head -n 5 a", ActionKind::Read),
    ("head -n5 a", ActionKind::Read),
    ("tail -n 5 a", ActionKind::Read),
    ("wc -l a", ActionKind::Read),
    ("stat a", ActionKind::Read),
    ("md5 a", ActionKind::Read),
    ("md5sum a", ActionKind::Read),
    ("shasum -a 256 a", ActionKind::Read),
    ("sha1sum a", ActionKind::Read),
    ("sha256sum a", ActionKind::Read),
    ("od -A x -t x1 a", ActionKind::Read),
    ("strings a", ActionKind::Read),
    ("cmp a b", ActionKind::Read),
    ("diff -u a b", ActionKind::Read),
    ("ls -la src", ActionKind::Read),
    ("du -sh src", ActionKind::Read),
    ("readlink -f a", ActionKind::Read),
    ("realpath a", ActionKind::Read),
    ("basename a/b", ActionKind::Read),
    ("dirname a/b", ActionKind::Read),
    ("cut -d , -f 1 a", ActionKind::Read),
    ("tr a b", ActionKind::Read),
    ("nl a", ActionKind::Read),
    ("tac a", ActionKind::Read),
    ("rev a", ActionKind::Read),
    ("column -t a", ActionKind::Read),
    ("fold -w 80 a", ActionKind::Read),
    ("fmt a", ActionKind::Read),
    ("expand a", ActionKind::Read),
    ("unexpand a", ActionKind::Read),
    ("grep -rn foo src", ActionKind::Read),
    ("egrep foo a", ActionKind::Read),
    ("fgrep foo a", ActionKind::Read),
    ("rg -n foo src", ActionKind::Read),
    ("find src -name x", ActionKind::Read),
    ("sort -n a", ActionKind::Read),
    ("uniq -c a", ActionKind::Read),
    ("comm -12 a b", ActionKind::Read),
    ("join a b", ActionKind::Read),
    ("paste -d , a b", ActionKind::Read),
    ("touch a", ActionKind::Write),
    ("mkdir -p a/b", ActionKind::Write),
    ("truncate -s 0 a", ActionKind::Write),
    ("chmod -R 644 a", ActionKind::Write),
    ("chown -R me a", ActionKind::Write),
    ("chgrp me a", ActionKind::Write),
    ("tee -a a", ActionKind::Write),
    ("cp -r a b", ActionKind::Write),
    ("install -m 644 a b", ActionKind::Write),
    ("mv a b", ActionKind::Write),
    ("ln -s a b", ActionKind::Write),
    ("rm -rf a", ActionKind::Delete),
    ("rmdir -p a", ActionKind::Delete),
    ("unlink a", ActionKind::Delete),
    ("shred -u a", ActionKind::Delete),
    ("dd if=a of=b bs=1m", ActionKind::Write),
];

#[test]
fn registered_forms_are_safe() {
    for (cmd, want) in SAFE {
        let k = kinds(cmd);
        assert!(!k.contains(&ActionKind::Execute), "{cmd}：{k:?}");
        assert!(k.contains(want), "{cmd}：{k:?}");
    }
    // 每个白名单程序都有登记形态的用例
    for spec in SAFE_COMMANDS {
        assert!(
            SAFE.iter()
                .any(|(c, _)| c.split_whitespace().next() == Some(spec.program)),
            "{} 缺用例",
            spec.program
        );
    }
}

/// 未登记的选项、参数个数或角色对不上 → 执行（不是读）。
#[test]
fn unregistered_options_become_execute() {
    for cmd in [
        "cat --unknown a",
        "tail -f a",
        "diff --output x a b",
        "od -j 4 a",
        "sort -o res a",
        "uniq a b",
        "ls --hyperlink=x",
        "find . -delete",
        "find . -fprint x",
        "cp a",
        "dd if=a",
        "dd if=a of=b conv=notrunc",
        "rm --no-preserve-root a",
        "grep --binary-files=x a b",
    ] {
        assert!(
            kinds(cmd).contains(&ActionKind::Execute),
            "{cmd}：{:?}",
            kinds(cmd)
        );
    }
}

/// 取值选项的路径角色（方案 3.4 N1 列出的用例）。
#[test]
fn option_value_roles() {
    let c = norm(&bash_req("cp --target-directory=dst a b"));
    assert!(has(&c, ActionKind::Write, &format!("{WS}/dst")));
    assert!(
        has(&c, ActionKind::Read, &format!("{WS}/a"))
            && has(&c, ActionKind::Read, &format!("{WS}/b"))
    );
    let c = norm(&bash_req("install -d build/x"));
    assert!(has(&c, ActionKind::Write, &format!("{WS}/build/x")));
    assert!(!has(&c, ActionKind::Read, &format!("{WS}/build/x")));
    let c = norm(&bash_req("grep -e pat a"));
    assert!(has(&c, ActionKind::Read, &format!("{WS}/a")));
    assert!(!has(&c, ActionKind::Read, &format!("{WS}/pat")));
    let c = norm(&bash_req("grep pat a"));
    assert!(
        !has(&c, ActionKind::Read, &format!("{WS}/pat")),
        "第一个位置参数是模式"
    );
    let c = norm(&bash_req("xxd in .git/config"));
    assert!(c.actions.iter().any(|a| a.kind == ActionKind::Execute));
    assert!(has(&c, ActionKind::Write, &format!("{WS}/.git/config")));
}
