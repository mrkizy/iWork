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
    ("find src -name '*.log' -delete", ActionKind::Delete),
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

use ActionKind::{Delete as D, Read as R, Write as W};

/// 实施审查 1/F-1、8/F-4：白名单程序带未登记选项时降成执行，**同时**保留保底角色的路径动作（只能更严）。
/// 每个白名单程序一行，期望手写。
const FLOOR: &[(&str, &[(ActionKind, &str)])] = &[
    ("cat --zz-unknown a", &[(R, "a")]),
    ("head --zz-unknown a", &[(R, "a")]),
    ("tail --zz-unknown a", &[(R, "a")]),
    ("wc --zz-unknown a", &[(R, "a")]),
    ("stat --zz-unknown a", &[(R, "a")]),
    ("md5 --zz-unknown a", &[(R, "a")]),
    ("md5sum --zz-unknown a", &[(R, "a")]),
    ("shasum --zz-unknown a", &[(R, "a")]),
    ("sha1sum --zz-unknown a", &[(R, "a")]),
    ("sha256sum --zz-unknown a", &[(R, "a")]),
    ("od --zz-unknown a", &[(R, "a")]),
    ("strings --zz-unknown a", &[(R, "a")]),
    ("cmp --zz-unknown a", &[(R, "a")]),
    ("diff --zz-unknown a", &[(R, "a")]),
    ("ls --zz-unknown a", &[(R, "a")]),
    ("du --zz-unknown a", &[(R, "a")]),
    ("readlink --zz-unknown a", &[(R, "a")]),
    ("realpath --zz-unknown a", &[(R, "a")]),
    ("basename --zz-unknown a", &[(R, "a")]),
    ("dirname --zz-unknown a", &[(R, "a")]),
    ("cut --zz-unknown a", &[(R, "a")]),
    ("tr --zz-unknown a", &[(R, "a")]),
    ("nl --zz-unknown a", &[(R, "a")]),
    ("tac --zz-unknown a", &[(R, "a")]),
    ("rev --zz-unknown a", &[(R, "a")]),
    ("column --zz-unknown a", &[(R, "a")]),
    ("fold --zz-unknown a", &[(R, "a")]),
    ("fmt --zz-unknown a", &[(R, "a")]),
    ("expand --zz-unknown a", &[(R, "a")]),
    ("unexpand --zz-unknown a", &[(R, "a")]),
    ("grep --zz-unknown a", &[(R, "a")]),
    ("egrep --zz-unknown a", &[(R, "a")]),
    ("fgrep --zz-unknown a", &[(R, "a")]),
    ("rg --zz-unknown a", &[(R, "a")]),
    ("sort --zz-unknown a", &[(R, "a")]),
    ("comm --zz-unknown a", &[(R, "a")]),
    ("join --zz-unknown a", &[(R, "a")]),
    ("paste --zz-unknown a", &[(R, "a")]),
    (
        "uniq --zz-unknown a b",
        &[(R, "a"), (W, "a"), (R, "b"), (W, "b")],
    ),
    ("touch --zz-unknown a", &[(W, "a")]),
    ("mkdir --zz-unknown a", &[(W, "a")]),
    ("truncate --zz-unknown a", &[(W, "a")]),
    ("chmod --zz-unknown a", &[(W, "a")]),
    ("chown --zz-unknown a", &[(W, "a")]),
    ("chgrp --zz-unknown a", &[(W, "a")]),
    ("tee --zz-unknown a", &[(W, "a")]),
    ("cp --zz-unknown a d", &[(R, "a"), (W, "d"), (W, "d/a")]),
    (
        "install --zz-unknown a d",
        &[(R, "a"), (W, "d"), (W, "d/a")],
    ),
    ("ln --zz-unknown a d", &[(R, "a"), (W, "d"), (W, "d/a")]),
    (
        "mv --zz-unknown a d",
        &[(R, "a"), (D, "a"), (W, "d"), (W, "d/a")],
    ),
    ("rm --zz-unknown a", &[(D, "a")]),
    ("rmdir --zz-unknown a", &[(D, "a")]),
    ("unlink --zz-unknown a", &[(D, "a")]),
    ("shred --zz-unknown a", &[(D, "a")]),
    ("dd if=a of=b conv=notrunc", &[(R, "a"), (W, "b")]),
];

#[test]
fn degraded_keeps_floor_roles() {
    for (cmd, want) in FLOOR {
        let c = norm(&bash_req(cmd));
        assert!(
            c.actions.iter().any(|a| a.kind == ActionKind::Execute),
            "{cmd}：{:?}",
            briefs(&c)
        );
        for (kind, rel) in *want {
            assert!(
                has(&c, *kind, &format!("{WS}/{rel}")),
                "{cmd} 缺 {kind:?} {rel}：{:?}",
                briefs(&c)
            );
        }
    }
    for spec in SAFE_COMMANDS {
        assert!(
            FLOOR
                .iter()
                .any(|(c, _)| c.split_whitespace().next() == Some(spec.program)),
            "{} 缺保底用例",
            spec.program
        );
    }
    // 对照：保底只在降级时产出，登记形态不多出删除
    let c = norm(&bash_req("cat a"));
    assert!(!c.actions.iter().any(|a| a.kind == ActionKind::Delete));
}

/// 实施审查 1/F-1：rm 带未登记的长选项删工作区根仍是底线；删普通文件仍是必问（不是自动放行）。
#[test]
fn degraded_rm_keeps_delete_constraints() {
    use iwork_domain::approval_mode::ApprovalMode::{Auto, FullAccess, Manual};
    use iwork_domain::rule::{AskReason, DenyReason, Verdict};
    let cmd = format!("rm --recursive --force {WS}");
    let c = norm(&bash_req(&cmd));
    assert!(has(&c, ActionKind::Delete, WS), "{:?}", briefs(&c));
    for m in [Manual, Auto, FullAccess] {
        let v = bash_verdict(&cmd, m);
        assert!(
            matches!(
                v,
                Verdict::Deny(DenyReason::Baseline(_)) | Verdict::UserAction(_)
            ),
            "{m:?}：{v:?}"
        );
    }
    let h = Harness::default();
    assert_eq!(
        h.verdict(&bash_req("rm --no-preserve-root a"), Manual),
        Verdict::Ask(AskReason::MustAsk)
    );
    assert_eq!(
        h.verdict(&bash_req("rm --no-preserve-root a"), Auto),
        Verdict::Ask(AskReason::MustAsk)
    );
    assert_eq!(
        h.verdict(&bash_req("rm --no-preserve-root a"), FullAccess),
        Verdict::Allow
    );
}
