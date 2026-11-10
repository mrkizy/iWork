//! 方案收口几轮新增的语料（T07 方案 3.3–3.8、4.3、11.3「4 Bash 归一语料」末段）。
//!
//! 每组写明链接布局与 cwd；期望手写，不从规则条目推。`Base` 指底线拒绝（`Deny(Baseline)`，单条命令时
//! 出亲手执行卡片 `UserAction`）。

mod support;

use fake_fs::FakeFs;
use iwork_core::approval::rules::builtin::builtin_rule_id;
use iwork_domain::action::{ActionKind, CallShapeKind};
use iwork_domain::approval_mode::ApprovalMode::{self, Auto, FullAccess, Manual};
use iwork_domain::id::Seq;
use iwork_domain::rule::{AskReason, DenyReason, RuleLayer, Verdict};
use support::*;

fn ws(rel: &str) -> String {
    format!("{WS}/{rel}")
}

/// 默认文件系统上再搭一些节点；快照按同一个文件系统取规则路径的事实。
fn harness(setup: impl FnOnce(&mut FakeFs)) -> Harness {
    let mut f = fs();
    setup(&mut f);
    Harness {
        snap: snapshot_on(Manual, &[], &env(), Seq(0), &f),
        fs: f,
        ..Harness::default()
    }
}

fn at(mut h: Harness, rel: &str) -> Harness {
    h.cwd = cp(&ws(rel));
    h
}

fn is_base(v: &Verdict) -> bool {
    matches!(
        v,
        Verdict::Deny(DenyReason::Baseline(_)) | Verdict::UserAction(_)
    )
}

fn deny_rule(slug: &str) -> Verdict {
    Verdict::Deny(DenyReason::DenyRule(builtin_rule_id(slug)))
}

const MODES3: [ApprovalMode; 3] = [Manual, Auto, FullAccess];

/// 三档都是底线拒绝。
fn assert_base(h: &Harness, cmd: &str) {
    assert_base_req(h, &bash_req(cmd), cmd);
}

fn assert_base_req(h: &Harness, req: &iwork_ports::agent::PermissionRequest, what: &str) {
    for m in MODES3 {
        let (call, e) = h.eval(req, m);
        assert!(
            is_base(&e.evaluation.verdict),
            "{what}（{m:?}）：{:?}\n    动作 {:?}",
            e.evaluation.verdict,
            briefs(&call)
        );
    }
}

/// 三档的简写结果。
fn three_on(h: &Harness, cmd: &str) -> [&'static str; 3] {
    MODES3.map(|m| v(&h.verdict(&bash_req(cmd), m)))
}

fn unparsable_ask(h: &Harness, cmd: &str) {
    for m in MODES3 {
        let (call, e) = h.eval(&bash_req(cmd), m);
        assert_eq!(call.shape.kind(), CallShapeKind::Unparsable, "{cmd}");
        assert_eq!(
            e.evaluation.verdict,
            Verdict::Ask(AskReason::UnparsableCommand),
            "{cmd}（{m:?}）\n    动作 {:?}",
            briefs(&call)
        );
    }
}

/// 收口 9/F-1、F-3、F-4、F-5 与收口补定：链接的两种视图都过底线，不按程序分、不分读写。
#[test]
fn link_both_views_hit_baseline() {
    let h = harness(|f| {
        f.file(ws("src/config"));
        f.file(ws("evil"));
        f.file(ws("scripts/pre-commit"));
        f.file(ws("notes.txt"));
        f.file("/home/u/dotfiles/npmrc");
        f.file("/home/u/dotfiles/ssh_config");
        f.file("/usr/share/dict/words");
        f.link(ws("config"), ".git/config");
        f.link(ws("c"), ".git/config");
        f.link(ws("g"), ".git");
        f.link(ws("h"), ".git/hooks");
        f.link(ws(".git/hooks/pre-commit"), ws("scripts/pre-commit"));
        f.link("/home/u/.npmrc", "dotfiles/npmrc");
        f.remove("/home/u/.ssh/config");
        f.link("/home/u/.ssh/config", "/home/u/dotfiles/ssh_config");
        f.link(ws("k"), "/home/u/.ssh/id_rsa");
        f.link(ws("l"), "notes.txt");
        f.link("/home/u/Documents/n", "/usr/share/dict/words");
    });
    // ① 只动链接本身也拒（接受的代价），单条命令出卡片
    for cmd in [
        "mv src/config .",
        "ln -sf src/config config",
        "rm config",
        "mv config moved",
    ] {
        assert_base(&h, cmd);
        assert!(
            matches!(h.verdict(&bash_req(cmd), Manual), Verdict::UserAction(_)),
            "{cmd} 应出卡片"
        );
    }
    let c = h.eval(&bash_req("rm config"), Manual).0;
    assert!(
        has(&c, ActionKind::Delete, &ws("config")),
        "{:?}",
        briefs(&c)
    );
    assert!(has(&c, ActionKind::Delete, &ws(".git/config")));
    // ② 尾斜杠：两种视图都是链接指向
    for cmd in ["rm -rf g/", "rmdir h/", "mv g/ x", "rm g"] {
        assert_base(&h, cmd);
    }
    // ③ 链接在 .git 里：entry 在 .git/ 内
    for cmd in [
        "install evil .git/hooks/pre-commit",
        "mv evil .git/hooks/pre-commit",
    ] {
        assert_base(&h, cmd);
    }
    // ④ 受保护路径自身是链接（规则路径别名，方案 4.3）
    for cmd in [
        "mv evil ~/.npmrc",
        "ln -sf evil ~/.npmrc",
        "rm ~/.npmrc",
        "echo x >> ~/.npmrc",
        "cat ~/.npmrc",
        "echo x >> ~/dotfiles/npmrc",
    ] {
        assert_base(&h, cmd);
    }
    // ⑤ 没单独登记过的写法也一样
    for cmd in [
        "shred c",
        "dd if=/dev/zero of=c",
        "truncate -s 0 c",
        "echo x > c",
    ] {
        assert_base(&h, cmd);
    }
    for req in [write_req("c"), edit_req("c"), {
        tool_req(
            "NotebookEdit",
            serde_json::json!({ "notebook_path": "c", "new_source": "x" }),
        )
    }] {
        assert_base_req(&h, &req, &req.raw_input.0);
    }
    // ⑥ 读也取两种视图
    for cmd in ["cat ~/.ssh/config", "cat k", "grep host k"] {
        assert_base(&h, cmd);
    }
    for p in ["~/.ssh/config", "k"] {
        assert_base_req(&h, &read_req(p), p);
    }
    let (c, e) = h.eval(&bash_req("cat ~/Documents/n"), Auto);
    assert!(
        c.actions
            .iter()
            .any(|a| a.kind == ActionKind::Read && a.flags.out_of_bounds),
        "entry 越界：{:?}",
        briefs(&c)
    );
    assert!(e.constraints.review_eligible());
    assert_eq!(three_on(&h, "cat ~/Documents/n"), ["Ask", "Ask", "Allow"]);
    // ⑦ 对照：链接两头都在边界内的照常
    assert_eq!(three_on(&h, "cat l"), ["Allow", "Allow", "Allow"]);
    let c = h.eval(&bash_req("rm l"), Manual).0;
    assert!(has(&c, ActionKind::Delete, &ws("l")) && has(&c, ActionKind::Delete, &ws("notes.txt")));
    assert_eq!(three_on(&h, "rm l"), ["Ask", "Ask", "Allow"]);
    assert_eq!(three_on(&h, "echo x > l"), ["Ask", "Allow", "Allow"]);
    assert_eq!(three_on(&h, "rm notes.txt"), ["Ask", "Ask", "Allow"]);
    assert_base(&h, "mv src/config .git/config");
}

/// 实施审查 1/F-7、8/F-2：目录目标语义，假设候选走不通只丢它。
#[test]
fn directory_target_cases() {
    let linked = harness(|f| {
        f.file(ws("src/config"));
        f.link(ws("config"), ".git/config");
    });
    for cmd in [
        "cp src/config .",
        "cp -t . src/config",
        "cp --target-directory=. src/config",
        "install src/config .",
    ] {
        assert_base(&linked, cmd);
    }
    let plain = harness(|f| {
        f.file(ws("src/config"));
        f.file(ws("a"));
        f.file(ws("existing.txt"));
        f.add(ws("dir"));
    });
    // 目的是普通文件：假设候选 NotADirectory 丢掉，不降成解析不了
    let (c, _) = plain.eval(&bash_req("cp a existing.txt"), Manual);
    assert_eq!(c.shape.kind(), CallShapeKind::SingleSimpleCommand);
    assert_eq!(
        three_on(&plain, "cp a existing.txt"),
        ["Ask", "Allow", "Allow"]
    );
    // 目的是目录：两个写
    let c = plain.eval(&bash_req("cp a dir/"), Manual).0;
    assert!(has(&c, ActionKind::Write, &ws("dir")) && has(&c, ActionKind::Write, &ws("dir/a")));
    assert_eq!(three_on(&plain, "cp a dir/"), ["Ask", "Allow", "Allow"]);
    // -T：只检查目的本身
    let c = plain.eval(&bash_req("cp -T src/config dest"), Manual).0;
    assert!(has(&c, ActionKind::Write, &ws("dest")));
    assert!(
        !has(&c, ActionKind::Write, &ws("dest/config")),
        "{:?}",
        briefs(&c)
    );
    // 对照：./config 不存在或是普通文件
    assert_eq!(
        three_on(&plain, "cp src/config ."),
        ["Ask", "Allow", "Allow"]
    );
    // 单条命令写 .git 内部：底线且出卡片
    for m in MODES3 {
        assert!(matches!(
            plain.verdict(&bash_req("cp src/config .git/config"), m),
            Verdict::UserAction(_)
        ));
    }
}

/// 实施审查 1/F-2：沙箱外是整条调用的约束，没有执行动作的命令另加沙箱外动作。
#[test]
fn bypass_applies_to_whole_call() {
    let h = Harness::default();
    for cmd in ["cat src/a.rs", "touch a", "printf x > a"] {
        let mut req = bash_req(cmd);
        req.bypass_sandbox = true;
        let (c, _) = h.eval(&req, Manual);
        assert!(
            c.actions
                .iter()
                .any(|a| a.kind == ActionKind::OutsideSandbox && a.flags.out_of_bounds),
            "{cmd}：{:?}",
            briefs(&c)
        );
        for m in [Manual, Auto] {
            assert_eq!(
                h.verdict(&req, m),
                Verdict::OutsideSandbox(AskReason::OutOfBounds),
                "{cmd}（{m:?}）"
            );
        }
        assert!(
            matches!(
                h.verdict(&req, FullAccess),
                Verdict::Deny(DenyReason::DenyRule(_))
            ),
            "{cmd}"
        );
    }
    // 对照：不带 bypass 时只是读
    assert_eq!(three_on(&h, "cat src/a.rs"), ["Allow", "Allow", "Allow"]);
}

/// 收口第 2 轮 closeout 1/F-1：parse 成功时按 find 的语法切。cwd 写在每组前。
#[test]
fn find_syntax_cases() {
    // cwd 在 src：起点显式写根，删除落在根上
    let h = at(harness(|_| {}), "src");
    for cmd in [
        format!("find -H {WS} -delete"),
        format!("find -P {WS} -delete"),
        format!("find -f {WS} -- -delete"),
        format!("find -Z {WS} -delete"),
    ] {
        let c = h.eval(&bash_req(&cmd), Manual).0;
        assert!(has(&c, ActionKind::Delete, WS), "{cmd}：{:?}", briefs(&c));
        assert_base(&h, &cmd);
    }
    // cwd 是工作区根
    let h = harness(|f| {
        f.add(ws("logs"));
    });
    let c = h
        .eval(&bash_req("find . -exec grep -F -- -delete {} +"), Manual)
        .0;
    assert!(
        !c.actions.iter().any(|a| a.kind == ActionKind::Delete),
        "{:?}",
        briefs(&c)
    );
    unparsable_ask(&h, "find . -exec grep -F -- -delete {} +");
    assert_eq!(
        three_on(&h, "find . -name -delete"),
        ["Allow", "Allow", "Allow"]
    );
    assert_base(&h, "find . -exec rm {} \\; -delete");
    assert_base(&h, &format!("find {WS} -maxdepth 0 -delete"));
    for m in MODES3 {
        assert!(matches!(
            h.verdict(&bash_req("find . -name '*.log' -delete"), m),
            Verdict::UserAction(_)
        ));
    }
    assert_eq!(
        three_on(&h, "find src -name '*.log' -delete"),
        ["Ask", "Ask", "Allow"]
    );
    assert_eq!(
        three_on(
            &at(
                harness(|f| {
                    f.add(ws("logs"));
                }),
                "logs"
            ),
            "find . -name '*.log' -delete"
        ),
        ["Ask", "Ask", "Allow"]
    );
    assert_eq!(
        three_on(&h, "find . -name '*.log'"),
        ["Allow", "Allow", "Allow"]
    );
    for cmd in [
        "find . -name '*.rs' -exec grep -l foo {} +",
        "find . -type f -perm +111",
    ] {
        unparsable_ask(&h, cmd);
        let (c, e) = h.eval(&bash_req(cmd), Manual);
        let (offered, _) =
            iwork_core::approval::evaluate::remember::offered_remember(&c, &e, &scope());
        assert_eq!(
            offered,
            vec![iwork_domain::approval_mode::RememberScope::Once]
        );
    }
    assert_base(&h, "find ~/.ssh -type f -exec cat {} \\;");
    assert_base(&h, "find .git -name config -exec rm {} \\;");
    // -delete 的约束不随 Unparsable 丢掉
    for cmd in [
        format!("find {WS} -depth -delete"),
        "LC_ALL=C find . -depth -delete".into(),
        format!("sudo find {WS} -delete"),
        "find -depth -delete".into(),
        "bash -c 'find . -depth -delete'".into(),
        "find \"$DIR\" -name '*.tmp' -delete".into(),
    ] {
        assert_eq!(
            norm(&bash_req(&cmd)).shape.kind(),
            CallShapeKind::Unparsable,
            "{cmd}"
        );
        assert_base(&h, &cmd);
    }
    for m in MODES3 {
        assert!(matches!(
            h.verdict(&bash_req("find . -mindepth 1 -delete"), m),
            Verdict::UserAction(_)
        ));
    }
    unparsable_ask(&h, "find src -depth -delete");
}

/// 收口第 3–5 轮 closeout 2/F-1、3/F-2：结构丢了用保守规则，find 之后每个字面词都是删除候选。
#[test]
fn find_flat_delete_cases() {
    for rel in ["src", ""] {
        let h = if rel.is_empty() {
            harness(|_| {})
        } else {
            at(harness(|_| {}), rel)
        };
        for cmd in [
            format!("find {WS} -name \"$PATTERN\" -delete"),
            format!("find {WS} -path \"$P\" -delete"),
            format!("find {WS} -regex \"$R\" -delete"),
        ] {
            let c = h.eval(&bash_req(&cmd), Manual).0;
            assert_eq!(c.shape.kind(), CallShapeKind::Unparsable);
            assert!(
                has(&c, ActionKind::Delete, WS),
                "{cmd}（cwd {rel}）：{:?}",
                briefs(&c)
            );
            assert_base(&h, &cmd);
        }
    }
    // cwd 是根：保守规则把 cwd 也算进去（接受的多拒）；去掉前置赋值回到精确规则
    let h = harness(|_| {});
    assert_base(&h, "LC_ALL=C find src -name '*.log' -delete");
    assert_eq!(
        three_on(&h, "find src -name '*.log' -delete"),
        ["Ask", "Ask", "Allow"]
    );
    // 不按 - 前缀过滤（cwd 在 src，src/-dir 存在）
    let h = at(
        harness(|f| {
            f.add(ws("src/-dir"));
        }),
        "src",
    );
    for cmd in [
        format!("LC_ALL=C find -f {WS} -- -delete"),
        "find -f -dir/../.. -- -name \"$P\" -delete".into(),
        "find .. -name \"$P\" -delete".into(),
        "bash -c 'find -f -dir/../.. -- -delete'".into(),
    ] {
        let c = h.eval(&bash_req(&cmd), Manual).0;
        assert!(has(&c, ActionKind::Delete, WS), "{cmd}：{:?}", briefs(&c));
        assert_base(&h, &cmd);
    }
    unparsable_ask(&h, "find -f -dir -- -name \"$P\" -delete");
    // -delete 自己也是候选（cwd 在 src，src/-delete 是指向工作区根的链接）
    let cmds = [
        "find -H -f -delete -- -name \"$P\" -delete",
        "LC_ALL=C find -H -f -delete -- -delete",
    ];
    let linked = at(
        harness(|f| {
            f.link(ws("src/-delete"), WS);
        }),
        "src",
    );
    for cmd in cmds {
        let c = linked.eval(&bash_req(cmd), Manual).0;
        assert!(has(&c, ActionKind::Delete, WS), "{cmd}：{:?}", briefs(&c));
        assert_base(&linked, cmd);
    }
    // 对照：没有这个链接时只多出不存在的名字
    let plain = at(harness(|_| {}), "src");
    for cmd in cmds {
        unparsable_ask(&plain, cmd);
    }
}

/// 收口第 5 轮 closeout 4/F-1：路径查不清（gate 没有搜索权限）三档拒、不出卡片；走不通的照旧。
#[test]
fn path_unresolved_cases() {
    let h = harness(|f| {
        f.add(ws("gate/root"));
        f.deny(ws("gate"));
        f.file(ws("a"));
        f.file(ws("existing.txt"));
    });
    let reqs = [
        bash_req("chmod u+x gate; find -H gate/root -delete"),
        bash_req("chmod u+x gate && rm -rf gate/root"),
        bash_req("LC_ALL=C cat gate/secret"),
        read_req("gate/x"),
    ];
    for req in &reqs {
        for m in MODES3 {
            let (c, e) = h.eval(req, m);
            assert_eq!(
                e.evaluation.verdict,
                deny_rule("deny.path-unresolved"),
                "{}（{m:?}）：{:?}",
                req.raw_input.0,
                briefs(&c)
            );
            assert_eq!(e.evaluation.layer_hit, RuleLayer::UserDeny);
        }
    }
    // 底线更先判
    for m in MODES3 {
        assert!(matches!(
            h.verdict(&bash_req("chmod u+x gate; cat gate/x ~/.ssh/id_rsa"), m),
            Verdict::Deny(DenyReason::Baseline(_))
        ));
    }
    // 对照：走不通的照旧
    assert_eq!(three_on(&h, "cp a existing.txt"), ["Ask", "Allow", "Allow"]);
    unparsable_ask(&h, "LC_ALL=C cat missing/../x");
    let (c, _) = h.eval(&bash_req("cat missing/../x"), Manual);
    assert_eq!(c.shape.kind(), CallShapeKind::Unparsable);
    assert_eq!(three_on(&h, "cat nosuchfile"), ["Allow", "Allow", "Allow"]);
}

/// 方案 3.3：登记表里的 shell 程序后面的字符串参数再解析，都断言底线拒。
#[test]
fn nested_command_text_keeps_facts() {
    let h = Harness::default();
    for cmd in [
        "eval \"cat ~/.ssh/id_rsa\"",
        "bash -c 'printf x > .git/config'",
        "sh -c \"rm -rf ~/.ssh\"",
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
        "eval \"printf x > .git/config\necho 'abc\"",
    ] {
        assert_eq!(
            norm(&bash_req(cmd)).shape.kind(),
            CallShapeKind::Unparsable,
            "{cmd}"
        );
        for m in MODES3 {
            assert!(
                matches!(
                    h.verdict(&bash_req(cmd), m),
                    Verdict::Deny(DenyReason::Baseline(_))
                ),
                "{cmd}（{m:?}）"
            );
        }
    }
    // 工作区带空格：eval 把参数连起来才看得到重定向目标（closeout 3/F-1）
    let space = "/tmp/probe/repo space";
    let mut f = fs();
    f.add(format!("{space}/.git/config"));
    let e = env_at(space);
    let hs = Harness {
        snap: snapshot_on(Manual, &[], &e, Seq(0), &f),
        fs: f,
        cwd: cp(space),
        env: e,
        ..Harness::default()
    };
    for cmd in [
        format!(
            "eval 'printf changed > \"{}' '{}/.git/config\"'",
            "/tmp/probe/repo", "space"
        ),
        format!(
            "eval printf x '>' '\"{}' '{}/.git/config\"'",
            "/tmp/probe/repo", "space"
        ),
    ] {
        for m in MODES3 {
            assert!(
                matches!(
                    hs.verdict(&bash_req(&cmd), m),
                    Verdict::Deny(DenyReason::Baseline(_))
                ),
                "{cmd}（{m:?}）"
            );
        }
    }
    // 对照
    unparsable_ask(&h, "eval \"cat src/a.rs\"");
}

/// closeout 1/F-2：不在登记程序后面的字符串参数不再解析，用户可以允许一次。
#[test]
fn plain_string_args_are_not_reparsed() {
    let h = Harness::default();
    for cmd in [
        "env LC_ALL=C git commit -m 'Document .git/config format'",
        "env LC_ALL=C printf '%s\\n' 'Document .git/config format'",
        "env LC_ALL=C grep 'cat ~/.ssh/id_rsa' notes.md",
        "sudo echo 'rm -rf ~/.ssh'",
    ] {
        unparsable_ask(&h, cmd);
    }
}

/// 9/F-7：替换体在任何词里都收；8/F-1：没有深度上限。
#[test]
fn substitution_in_any_word_keeps_facts() {
    let h = Harness::default();
    let mut cmds: Vec<String> = [
        "echo ${x:-$(printf x > .git/config)}",
        "echo $(( $(printf x > .git/config; echo 1) + 1 ))",
        "cat <<< \"$(printf x > .git/config)\"",
        "printf x > .git/config\nif\n",
        "echo $(printf x > .git/config)",
        "echo `cat ~/.ssh/id_rsa`",
    ]
    .map(String::from)
    .to_vec();
    for depth in [8, 9, 10] {
        let mut t = "printf x > .git/config".to_string();
        for _ in 0..depth {
            t = format!("echo $({t})");
        }
        cmds.push(t);
    }
    for cmd in &cmds {
        assert_eq!(
            norm(&bash_req(cmd)).shape.kind(),
            CallShapeKind::Unparsable,
            "{cmd}"
        );
        for m in MODES3 {
            assert!(
                matches!(
                    h.verdict(&bash_req(cmd), m),
                    Verdict::Deny(DenyReason::Baseline(_))
                ),
                "{cmd}（{m:?}）"
            );
        }
    }
    unparsable_ask(&h, "echo ${x:-$(printf x > out.txt)}");
}

/// 收口补定：整条命令词法都切不出时三档拒、不出卡片。
#[test]
fn unlexable_command_is_denied() {
    let h = Harness::default();
    for cmd in ["printf x > .git/config\nls\necho 'abc", "echo \"abc"] {
        for m in MODES3 {
            let (_, e) = h.eval(&bash_req(cmd), m);
            assert_eq!(
                e.evaluation.verdict,
                deny_rule("deny.unlexable"),
                "{cmd:?}（{m:?}）"
            );
            assert_eq!(e.evaluation.layer_hit, RuleLayer::UserDeny);
        }
    }
}

/// 8/F-1、9/F-6：命令文字总量超过 256 KiB 三档拒；同长度以下的普通命令不受影响。
#[test]
fn resource_limit_is_denied() {
    let h = Harness::default();
    let big = format!("echo {}", "a".repeat(iwork_shell::TEXT_LIMIT));
    for m in MODES3 {
        let (_, e) = h.eval(&bash_req(&big), m);
        assert_eq!(
            e.evaluation.verdict,
            deny_rule("deny.resource-limit"),
            "{m:?}"
        );
        assert_eq!(e.evaluation.layer_hit, RuleLayer::UserDeny);
    }
    let small = format!("echo {}", "a".repeat(iwork_shell::TEXT_LIMIT - 100));
    assert_eq!(three_on(&h, &small), ["Ask", "Allow", "Allow"]);
}

/// 方案 4.3：规则路径的两个位置；只给保护性条目生成别名。
#[test]
fn rule_path_alias_cases() {
    // ~/.zshrc 是链接：写入口、写真实位置都改判自动执行配置
    let h = harness(|f| {
        f.file("/home/u/dotfiles/zshrc");
        f.link("/home/u/.zshrc", "dotfiles/zshrc");
    });
    for p in ["~/.zshrc", "~/dotfiles/zshrc"] {
        let (c, _) = h.eval(&write_req(p), Manual);
        assert!(
            c.actions
                .iter()
                .any(|a| a.kind == ActionKind::WriteAutorunConfig),
            "{p}：{:?}",
            briefs(&c)
        );
    }
    // /etc 是 /private/etc 的别名：两种写法删除都拒
    let h = harness(|f| {
        f.add("/private/etc");
        f.link("/etc", "/private/etc");
    });
    assert!(
        h.snap
            .rule_paths
            .iter()
            .any(|r| r.path == std::path::Path::new("/etc"))
    );
    for cmd in ["rm -rf /etc", "rm -rf /private/etc"] {
        assert_base(&h, cmd);
    }
    // 没有链接时不产生别名
    assert!(snapshot().rule_paths.is_empty());
    // imports / attachments 链到 artifacts：快照里没有指向 artifacts 的放行，读两边都拒
    let artifacts = format!("{DATA}/artifacts");
    for zone in ["imports", "attachments"] {
        let h = harness(|f| {
            f.remove(format!("{DATA}/{zone}"));
            f.link(format!("{DATA}/{zone}"), "artifacts");
        });
        assert!(
            !h.snap.rules.iter().any(|r| {
                r.effect == iwork_domain::rule::RuleEffect::Allow
                    && matches!(&r.matcher, iwork_domain::rule::RuleMatcher::PathUnder(p) if p.starts_with(&artifacts))
            }),
            "{zone}"
        );
        for p in [
            format!("{artifacts}/log.txt"),
            format!("{DATA}/{zone}/log.txt"),
        ] {
            assert_base_req(&h, &read_req(&p), &p);
            assert_base(&h, &format!("cat '{p}'"));
        }
    }
    // 可写缓存是链接时不产生别名
    let h = harness(|f| {
        f.add("/home/u/caches/npm");
        f.link("/home/u/.npm", "caches/npm");
    });
    assert!(h.snap.rule_paths.is_empty(), "{:?}", h.snap.rule_paths);
}

/// 2/F-7（1/F-10 残留）：大小写不敏感时，拒绝 `Write DIR/New.TXT`（DIR 还不存在）→ 获准 `mkdir DIR` →
/// 原样重试，仍被拒绝记忆挡住；跨工具（重定向）同样。
#[test]
fn denial_key_survives_parent_creation() {
    use iwork_core::approval::denial::{DenialEvent, DenialMemory};
    let mut h = Harness {
        fs: fs_ci(true),
        ..Harness::default()
    };
    h.snap = snapshot_on(Manual, &[], &env(), Seq(0), &h.fs);
    let (call, _) = h.eval(&write_req("DIR/New.TXT"), Auto);
    h.denial = DenialMemory::replay(&[DenialEvent::UserDenied(call.fingerprints())]);
    assert_eq!(h.verdict(&bash_req("mkdir DIR"), Auto), Verdict::Allow);
    h.fs.add(ws("DIR"));
    for req in [
        write_req("DIR/New.TXT"),
        bash_req("printf x > dir/new.txt"),
        write_req(&ws("Dir/NEW.txt")),
    ] {
        assert!(
            matches!(
                h.verdict(&req, Auto),
                Verdict::Deny(DenyReason::DenialMemory(_))
            ),
            "{}",
            req.raw_input.0
        );
    }
    // 对照：别的文件照常
    assert_eq!(h.verdict(&write_req("DIR/other.txt"), Auto), Verdict::Allow);
}

/// 2/F-6：路径查不清与底线同时命中（`gate` 没有搜索权限）：单条命令也不出卡片，三档拒。
/// 同类：拒绝规则、拒绝记忆与底线并存时同样不出卡片。
#[test]
fn unresolved_and_baseline_does_not_offer_card() {
    let h = harness(|f| {
        f.add(ws("gate"));
        f.deny(ws("gate"));
    });
    for m in MODES3 {
        let (c, e) = h.eval(&bash_req("cp gate/a .git/config"), m);
        assert!(
            matches!(e.evaluation.verdict, Verdict::Deny(_)),
            "{m:?}：{:?}\n动作 {:?}",
            e.evaluation.verdict,
            briefs(&c)
        );
        assert!(
            e.per_action
                .iter()
                .any(|x| x.verdict == deny_rule("deny.path-unresolved"))
        );
    }
    // 对照：没有查不清的路径时照常出卡片
    for m in MODES3 {
        assert!(matches!(
            h.verdict(&bash_req("cp src/a.rs .git/config"), m),
            Verdict::UserAction(_)
        ));
    }
}

/// 2/F-5（1/F-1、1/F-7 残留）：降级时保底角色作用于全部词，不再把最后一个操作数猜作目的。
#[test]
fn degraded_floor_does_not_guess_destination() {
    // GNU 写法：-t 给目的、--backup 未登记；真实源是工作区根
    let h = harness(|_| {});
    let cmd = format!("mv -t /tmp/destination --backup {WS}");
    let c = h.eval(&bash_req(&cmd), FullAccess).0;
    assert!(has(&c, ActionKind::Delete, WS), "{:?}", briefs(&c));
    assert_base(&h, &cmd);
    // ./config 是指向 .git/config 的链接：--verbose 未登记，-t . 的目录目标仍要检查 ./config
    let linked = harness(|f| {
        f.file(ws("src/config"));
        f.link(ws("config"), ".git/config");
    });
    let cmd = "cp -t . --verbose src/config";
    let c = linked.eval(&bash_req(cmd), Auto).0;
    assert!(
        has(&c, ActionKind::Write, &ws("config")),
        "{:?}",
        briefs(&c)
    );
    assert!(has(&c, ActionKind::Write, &ws(".git/config")));
    assert_base(&linked, cmd);
}

/// 方案第 13 节实施审查第 2 轮：解析不了的调用里，白名单程序名（含经包装词）后面的每个字面词按保底角色
/// 产出动作；watch、flock -c 的参数再解析。
#[test]
fn flat_words_use_program_floor() {
    let linked = harness(|f| {
        f.file(ws("src/config"));
        f.link(ws("config"), ".git/config");
    });
    for cmd in [
        format!("timeout 5 rm -rf {WS}"),
        format!("nice -n 5 rm -rf {WS}"),
        "caffeinate cp src/config .".into(),
        "watch 'printf x > .git/config'".into(),
        "watch -n 1 printf x '>' .git/config".into(),
        "flock /tmp/l -c 'printf x > .git/config'".into(),
        format!("flock /tmp/l rm -rf {WS}"),
        format!("LC_ALL=C mv -t /tmp/d {WS}"),
    ] {
        assert_eq!(
            norm(&bash_req(&cmd)).shape.kind(),
            CallShapeKind::Unparsable,
            "{cmd}"
        );
        for m in MODES3 {
            assert!(
                matches!(
                    linked.verdict(&bash_req(&cmd), m),
                    Verdict::Deny(DenyReason::Baseline(_))
                ),
                "{cmd}（{m:?}）"
            );
        }
    }
    // 对照：删工作区里的普通文件只是询问（解析不了，三档问）
    unparsable_ask(&linked, "timeout 5 rm -rf src/a.rs");
}

/// 目录目标候选是词数的平方级：超过上限整条按资源上限拒，不截断。
#[test]
fn too_many_dir_candidates_is_resource_limit() {
    let h = Harness::default();
    let words: Vec<String> = (0..100).map(|i| format!("f{i}")).collect();
    let cmd = format!("cp --zz-unknown {} dst", words.join(" "));
    for m in MODES3 {
        let (_, e) = h.eval(&bash_req(&cmd), m);
        assert_eq!(
            e.evaluation.verdict,
            deny_rule("deny.resource-limit"),
            "{m:?}"
        );
    }
    // 对照：登记形态不走保底，词多也照常
    let cmd = format!("cp {} dst", words.join(" "));
    assert_eq!(three_on(&h, &cmd), ["Ask", "Allow", "Allow"]);
}
