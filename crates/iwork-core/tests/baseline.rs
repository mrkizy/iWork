//! 系统底线（T07 验收 4a 凭据目录、4b 关键路径、4c `.git` 内部、5 路径逃逸；需求第 9 章）。
//! 期望路径手写，不从规则条目推。

mod support;

use iwork_domain::action::ActionKind;
use iwork_domain::approval_mode::ApprovalMode;
use iwork_domain::rule::{AskReason, DenyReason, RuleLayer, RuleMatcher, RuleScope, Verdict};
use support::*;

fn is_baseline(v: &Verdict) -> bool {
    matches!(
        v,
        Verdict::Deny(DenyReason::Baseline(_)) | Verdict::UserAction(_)
    )
}

/// 需求第 9 章凭据目录清单（macOS 形状）。
const CREDENTIAL_PATHS: &[&str] = &[
    "/home/u/.ssh/id_rsa",
    "/home/u/.gnupg/pubring.kbx",
    "/home/u/.aws/credentials",
    "/home/u/.azure/x",
    "/home/u/.kube/config",
    "/home/u/.config/gcloud/creds.json",
    "/home/u/.docker/config.json",
    "/home/u/.netrc",
    "/home/u/.git-credentials",
    "/home/u/.npmrc",
    "/home/u/.pypirc",
    "/home/u/Library/Keychains/login.keychain-db",
    "/home/u/.claude/.credentials.json",
    "/home/u/.codex/auth.json",
];

/// 4a：每个凭据路径 × 各种访问方式 × 三档 × 有放行规则 ⇒ 拒绝（不是询问）。
#[test]
fn credential_paths_denied_every_way() {
    let mut h = Harness::default();
    h.live.push(live_rule(
        1,
        RuleLayer::Remembered,
        RuleMatcher::Any,
        RuleScope::Global,
    ));
    h.live.push(live_rule(
        2,
        RuleLayer::ProjectAllow,
        RuleMatcher::Any,
        RuleScope::Project(project()),
    ));
    for p in CREDENTIAL_PATHS {
        let reqs = [
            read_req(p),
            write_req(p),
            edit_req(p),
            bash_req(&format!("cat {p}")),
            bash_req(&format!("cp {p} ./copy")),
            bash_req(&format!("cp src/a.rs {p}")),
            bash_req(&format!("grep x {p}")),
            bash_req(&format!("echo x > {p}")),
            bash_req(&format!("wc -l < {p}")),
            bash_req(&format!("rsync {p} ./copy")),
        ];
        for req in &reqs {
            for m in MODES {
                let verdict = h.verdict(req, m);
                assert!(
                    is_baseline(&verdict),
                    "{p} {m:?} {}：{verdict:?}",
                    req.raw_input.0
                );
            }
        }
    }
}

/// 4a：经符号链接或 .. 指向凭据目录的同样拒绝。
#[test]
fn symlink_and_dotdot_into_credential_dir_denied() {
    let h = Harness::default();
    for req in [
        read_req("keys/id_rsa"),
        bash_req("cat keys/config"),
        read_req(&format!("{WS}/../../../.ssh/id_rsa")),
        bash_req("cat ../../../.aws/credentials"),
    ] {
        for m in MODES {
            assert!(
                is_baseline(&h.verdict(&req, m)),
                "{m:?} {}",
                req.raw_input.0
            );
        }
    }
}

/// 4a：git push（ssh）、gh、npm publish 不因内部使用凭据被判为底线拒绝。
#[test]
fn git_push_gh_npm_publish_not_baseline() {
    let h = Harness::default();
    for cmd in ["git push", "gh pr create", "npm publish", "ssh host uptime"] {
        for m in MODES {
            let verdict = h.verdict(&bash_req(cmd), m);
            assert!(
                !matches!(verdict, Verdict::Deny(_) | Verdict::UserAction(_)),
                "{cmd} {m:?}：{verdict:?}"
            );
        }
    }
}

/// 4b：删除关键路径或其上级拒绝；删除工作区内普通文件是必问。
#[test]
fn critical_paths_delete_denied() {
    let h = Harness::default();
    for cmd in [
        "rm -rf ~",
        "rm -rf /",
        &format!("rm -rf {WS}"),
        "rm -rf .git",
        "rm -rf ~/iWork/projects",
        "rm -rf ~/iWork",
        "rmdir ~/iWork/assistants",
        "rm -rf /usr",
        "rm -rf ~/Library",
        &format!("rm -rf '{DATA}'"),
    ] {
        for m in MODES {
            assert!(is_baseline(&h.verdict(&bash_req(cmd), m)), "{cmd} {m:?}");
        }
    }
    for m in MODES {
        assert!(
            !is_baseline(&h.verdict(&bash_req("rm -rf ~/*"), m)),
            "通配：解析不了，询问"
        );
        assert_ne!(h.verdict(&bash_req("rm -rf ~/*"), m), Verdict::Allow);
    }
    for m in [ApprovalMode::Manual, ApprovalMode::Auto] {
        assert_eq!(
            h.verdict(&bash_req("rm src/a.rs"), m),
            Verdict::Ask(AskReason::MustAsk)
        );
    }
    // 对照：删除 /usr 之下的普通文件不是关键路径（是越界删除）
    assert!(!is_baseline(
        &h.verdict(&bash_req("rm /usr/local/x"), ApprovalMode::Manual)
    ));
}

/// 4c：Write / Edit / 重定向 / rm / mv / cp 的目标落在 .git/ 内拒绝；git commit、checkout 不因此拒绝。
#[test]
fn git_internal_writes_denied() {
    let h = Harness::default();
    let g = |p: &str| format!("{WS}/.git/{p}");
    for req in [
        write_req(&g("config")),
        edit_req(&g("config")),
        bash_req("echo x > .git/config"),
        bash_req("echo x >> .git/info/exclude"),
        bash_req("rm .git/index"),
        bash_req("mv src/a.rs .git/x"),
        bash_req("cp src/a.rs .git/x"),
        bash_req("touch .git/hooks/pre-commit"),
        // F-9：objs → .git/objects，链接后接 .. 得到 .git/config
        write_req("objs/../config"),
        bash_req("echo x > objs/../config"),
    ] {
        for m in MODES {
            assert!(
                is_baseline(&h.verdict(&req, m)),
                "{m:?} {}",
                req.raw_input.0
            );
        }
    }
    for cmd in [
        "git commit -m x",
        "git checkout main",
        "git add .",
        "git status",
    ] {
        for m in MODES {
            assert!(!is_baseline(&h.verdict(&bash_req(cmd), m)), "{cmd} {m:?}");
        }
    }
    // 读 .git 内部不是底线
    assert_eq!(
        h.verdict(&read_req(&g("config")), ApprovalMode::Manual),
        Verdict::Allow
    );
}

/// 验收 5：../、符号链接指向工作区外、大小写变体，都判为工作区外。
#[test]
fn path_escapes_are_out_of_workspace() {
    let h = Harness::default();
    for req in [
        write_req("../escape.txt"),
        write_req("out/x.txt"),
        bash_req("echo x > ../escape.txt"),
    ] {
        let (call, e) = h.eval(&req, ApprovalMode::Auto);
        assert!(
            call.actions
                .iter()
                .any(|a| a.kind == ActionKind::Write && a.flags.out_of_bounds),
            "{}",
            req.raw_input.0
        );
        assert_eq!(e.evaluation.verdict, Verdict::Ask(AskReason::OutOfBounds));
    }
    // 大小写不敏感的文件系统：.GIT/CONFIG 就是 .git/config
    let ci = Harness {
        fs: fs_ci(true),
        ..Harness::default()
    };
    for m in MODES {
        assert!(is_baseline(&ci.verdict(&write_req(".GIT/CONFIG"), m)));
        assert!(is_baseline(
            &ci.verdict(&read_req("/home/u/.SSH/id_rsa"), m)
        ));
    }
    // 对照：大小写敏感时 .GIT 不是 .git
    assert!(!is_baseline(
        &h.verdict(&write_req(".GIT/CONFIG"), ApprovalMode::Auto)
    ));
}
