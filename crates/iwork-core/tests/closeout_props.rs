//! 方案收口几轮新增的性质（T07 方案 11.2）：链接两种视图、`find` 的删除约束、shell 程序参数再解析、
//! 任何词里的替换体。期望只看结果类别（底线拒 / 没有删除），不从实现推。

mod support;

use fake_fs::FakeFs;
use iwork_domain::action::{ActionKind, CallShapeKind};
use iwork_domain::approval_mode::ApprovalMode;
use iwork_domain::id::Seq;
use iwork_domain::rule::{DenyReason, Verdict};
use proptest::prelude::*;
use support::*;

fn ws(rel: &str) -> String {
    format!("{WS}/{rel}")
}

fn harness_on(f: FakeFs, cwd: &str) -> Harness {
    Harness {
        snap: snapshot_on(ApprovalMode::Manual, &[], &env(), Seq(0), &f),
        fs: f,
        cwd: cp(cwd),
        ..Harness::default()
    }
}

fn is_base(v: &Verdict) -> bool {
    matches!(
        v,
        Verdict::Deny(DenyReason::Baseline(_)) | Verdict::UserAction(_)
    )
}

fn arb_mode() -> impl Strategy<Value = ApprovalMode> {
    prop::sample::select(vec![
        ApprovalMode::Manual,
        ApprovalMode::Auto,
        ApprovalMode::FullAccess,
    ])
}

/// 链接布局：(链接所在位置, 链接指向, 能碰底线的访问方式)。
/// - 工作区里的链接指向受保护位置：`target` 视图命中；
/// - 凭据目录里的链接指向工作区普通文件：`entry` 视图命中。
///
/// `.git/` 与关键路径只拦写和删，读的布局只用凭据与数据目录。
fn arb_link() -> impl Strategy<Value = (String, String, bool)> {
    prop::sample::select(vec![
        (ws("lnk"), "/home/u/.ssh/id_rsa".to_string(), true),
        (
            ws("lnk"),
            "/home/u/Library/Application Support/iWork/iwork.db".to_string(),
            true,
        ),
        (ws("lnk"), ws(".git/config"), false),
        ("/home/u/.ssh/lnk".to_string(), ws("notes.txt"), true),
        (
            "/home/u/Library/Application Support/iWork/lnk".to_string(),
            ws("notes.txt"),
            true,
        ),
    ])
}

/// 写或删的命令模板（`{}` 是路径词）；读的模板另列。
const WRITE_DELETE: &[&str] = &[
    "echo x > {}",
    "echo x >> {}",
    "rm {}",
    "rm -f {}",
    "cp src/a.rs {}",
    "mv {} moved",
    "mv src/a.rs {}",
    "ln -sf src/a.rs {}",
    "shred {}",
    "truncate -s 0 {}",
    "touch {}",
    "dd if=/dev/zero of={}",
    "tee {}",
];
const READS: &[&str] = &["cat {}", "grep x {}", "head -n 1 {}", "wc -l {}"];

proptest! {
    #![proptest_config(ProptestConfig { cases: 128, ..ProptestConfig::default() })]

    /// 读、写或删一个路径词，`entry`、`target` 任一视图命中底线 ⇒ 底线拒，与程序名、档位无关。
    #[test]
    fn link_views_both_checked(
        (at, target, readable) in arb_link(),
        write in prop::sample::select(WRITE_DELETE.to_vec()),
        read in prop::sample::select(READS.to_vec()),
        use_read in any::<bool>(),
        use_tool in any::<bool>(),
        mode in arb_mode(),
    ) {
        let mut f = fs();
        f.file(ws("notes.txt"));
        f.link(&at, &target);
        let h = harness_on(f, WS);
        let word = if at.starts_with(WS) { "lnk".to_string() } else { at.clone() };
        let quoted = format!("'{word}'");
        let req = match (use_read && readable, use_tool) {
            (true, true) => read_req(&word),
            (true, false) => bash_req(&read.replace("{}", &quoted)),
            (false, true) => write_req(&word),
            (false, false) => bash_req(&write.replace("{}", &quoted)),
        };
        let (call, e) = h.eval(&req, mode);
        prop_assert!(
            is_base(&e.evaluation.verdict),
            "{}：{:?}\n动作 {:?}",
            req.raw_input.0,
            e.evaluation.verdict,
            briefs(&call)
        );
    }

    /// 带尾斜杠时两种视图相同：目录链接 `g → .git` 删、挪都命中底线。
    #[test]
    fn trailing_slash_link_is_target(
        cmd in prop::sample::select(vec!["rm -rf g/", "rmdir g/", "mv g/ x", "rm g", "rm -rf g/."]),
        mode in arb_mode(),
    ) {
        let mut f = fs();
        f.link(ws("g"), ".git");
        let h = harness_on(f, WS);
        prop_assert!(is_base(&h.verdict(&bash_req(cmd), mode)), "{cmd}");
    }
}

/// 从 cwd 写到工作区根的几种起点写法。
fn arb_find_root() -> impl Strategy<Value = (String, String)> {
    prop::sample::select(vec![
        (WS.to_string(), ".".to_string()),
        (WS.to_string(), WS.to_string()),
        (WS.to_string(), "-f .".to_string()),
        (ws("src"), WS.to_string()),
        (ws("src"), "..".to_string()),
        (ws("src"), "-f ..".to_string()),
        (ws("src"), "-f -dir/../..".to_string()),
        (ws("src"), format!("-f {WS}")),
    ])
}

proptest! {
    #![proptest_config(ProptestConfig { cases: 256, ..ProptestConfig::default() })]

    /// `find` 有一个关键路径起点和表达式位置的 `-delete`，随机插前置选项、谓词、结束了的 `-exec` 段、
    /// 包装词、前置赋值、谓词参数换成展开 ⇒ 恒为底线拒，不会是询问（方案 11.2）。
    #[test]
    fn find_delete_never_just_asks_on_critical_root(
        (cwd, root) in arb_find_root(),
        wrapper in prop::sample::select(vec!["", "sudo ", "LC_ALL=C ", "env ", "bash -c '"]),
        leading in prop::sample::select(vec!["", "-H ", "-L ", "-P -x ", "-HL ", "-Z ", "-O3 "]),
        preds in prop::collection::vec(prop::sample::select(vec![
            "-name x", "-type f", "-depth", "-perm 644", "-maxdepth 3", "-exec true {} \\;",
            "-exec true {} +", "-name \"$X\"", "-path \"$Y\"", "-newer src/a.rs", "-print",
        ]), 0..4),
        mode in arb_mode(),
    ) {
        let mut f = fs();
        f.add(ws("src/-dir"));
        let h = harness_on(f, &cwd);
        // bash -c '…' 里不能再有单引号与 $ 展开的意外：谓词在单引号里照样是字面
        let close = if wrapper.ends_with('\'') { "'" } else { "" };
        let preds = preds.join(" ");
        let cmd = format!("{wrapper}find {leading}{root} -- {preds} -delete{close}");
        let cmd = if root.starts_with("-f") { cmd } else { cmd.replacen(" -- ", " ", 1) };
        let (call, e) = h.eval(&bash_req(&cmd), mode);
        prop_assert!(
            is_base(&e.evaluation.verdict),
            "{cmd}（cwd {cwd}）：{:?}\n动作 {:?}",
            e.evaluation.verdict,
            briefs(&call)
        );
    }

    /// 整条都是字面、parse 成功的 `find`，`-delete` 只出现在 `-exec` 段里或当登记过谓词的参数 ⇒
    /// 动作里没有删除。
    #[test]
    fn find_delete_only_in_expression_position(
        root in prop::sample::select(vec![".", "src", ""]),
        preds in prop::collection::vec(prop::sample::select(vec![
            "-name x", "-type f", "-maxdepth 2", "-print", "-not -empty",
        ]), 0..3),
        form in prop::sample::select(vec![
            "-exec grep -F -- -delete {} +",
            "-exec grep -e -delete {} \\;",
            "-execdir echo -delete \\;",
            "-name -delete",
            "-path -delete",
            "-regex -delete",
        ]),
    ) {
        let cmd = format!("find {root} {} {form}", preds.join(" "));
        let c = norm(&bash_req(&cmd));
        prop_assert!(
            !c.actions.iter().any(|a| a.kind == ActionKind::Delete),
            "{cmd}：{:?}",
            briefs(&c)
        );
    }

    /// 登记表里的 shell 程序，命令串前后随机插选项，命令串是 `printf x > .git/config` ⇒ 恒为底线拒。
    #[test]
    fn shell_program_args_all_reparsed(
        shell in prop::sample::select(vec!["sh", "bash", "zsh", "dash", "ksh", "/bin/bash"]),
        wrapper in prop::sample::select(vec!["", "env X=1 ", "sudo ", "timeout 5 ", "nohup "]),
        before in prop::collection::vec(prop::sample::select(vec!["-e", "-l", "-o pipefail", "-x"]), 0..3),
        c_form in prop::sample::select(vec!["-c", "-lc", "-ec", "-cx"]),
        after in prop::collection::vec(prop::sample::select(vec!["-e", "--", "-o pipefail", "-l"]), 0..3),
        mode in arb_mode(),
    ) {
        let cmd = format!(
            "{wrapper}{shell} {} {c_form} {} 'printf x > .git/config'",
            before.join(" "),
            after.join(" ")
        );
        prop_assert!(
            matches!(Harness::default().verdict(&bash_req(&cmd), mode), Verdict::Deny(DenyReason::Baseline(_))),
            "{cmd}"
        );
    }

    /// 把替换体塞进脚本里任意一个会展开的位置 ⇒ 解析不了且底线拒。
    #[test]
    fn substitution_found_in_any_word(
        parts in prop::collection::vec(prop::sample::select(vec![
            "ls -la", "cat src/a.rs", "echo $X", "rm *.o", "A=1 make", "{ echo; } > x",
            "(echo y)", "cat 'a b' > out",
        ]), 0..4),
        wrap in prop::sample::select(vec![
            "$(S)", "`S`", "${x:-$(S)}", "$(( $(S; echo 1) + 1 ))", "\"a $(S) b\"",
            "$(echo $(S))", "<<< \"$(S)\"",
        ]),
        at in 0usize..4,
        sep in prop::sample::select(vec!["; ", " && ", " || ", " | ", "\n"]),
        mode in arb_mode(),
    ) {
        let inner = wrap.replace('S', "printf x > .git/config");
        let mut words: Vec<String> = parts.into_iter().map(String::from).collect();
        words.insert(at.min(words.len()), format!("cat {inner}"));
        let cmd = words.join(sep);
        let (call, e) = Harness::default().eval(&bash_req(&cmd), mode);
        prop_assert_eq!(call.shape.kind(), CallShapeKind::Unparsable, "{}", cmd);
        prop_assert!(
            matches!(e.evaluation.verdict, Verdict::Deny(DenyReason::Baseline(_))),
            "{cmd}：{:?}",
            e.evaluation.verdict
        );
    }
}
