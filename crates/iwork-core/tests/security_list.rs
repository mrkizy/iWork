//! 安全清单用户条目的校验（T07 方案 4.4；AC41）。

mod support;

use iwork_core::approval::rules::build_ruleset;
use iwork_core::approval::rules::builtin::builtin_rules;
use iwork_core::approval::rules::security_list::{ListValidationContext, validate_list_entry};
use iwork_domain::common::Timestamp;
use iwork_domain::id::{RawUlid, RuleId};
use iwork_domain::rule::{RuleEffect, RuleLayer, RuleMatcher, RuleScope, RuleSet, RuleSource};
use iwork_domain::security_list::{ListEntryRejection, SecurityList};
use support::*;

fn ruleset() -> RuleSet {
    build_ruleset(&builtin_rules(&env()), &[], &scope()).unwrap()
}

fn check(list: SecurityList, value: &str) -> Result<RuleMatcher, ListEntryRejection> {
    let rules = ruleset();
    let roots = [cp(WS)];
    let ctx = ListValidationContext {
        paths: &env(),
        project_roots: &roots,
        rules: &rules,
    };
    validate_list_entry(
        list,
        value,
        RuleScope::Project(project()),
        &ctx,
        &fs(),
        Timestamp(5),
        RuleId::from_raw(RawUlid::from_u128(77)),
    )
    .map(|e| {
        assert_eq!(e.layer, list.layer());
        assert_eq!(e.source, RuleSource::User);
        assert_eq!(e.scope, RuleScope::Project(project()));
        assert_eq!(e.label, value.trim());
        e.matcher
    })
}

fn rejected(list: SecurityList, value: &str, needle: &str) {
    match check(list, value) {
        Ok(m) => panic!("{value:?} 应拒收，却得到 {m:?}"),
        Err(r) => assert!(r.reason.contains(needle), "{value:?}：{}", r.reason),
    }
}

/// AC41：terraform apply 接受；解释器、shell、启动器拒收并说明原因。
#[test]
fn credential_rejects_interpreters_and_transfer_tools() {
    assert_eq!(
        check(SecurityList::CredentialCommands, "terraform apply").unwrap(),
        RuleMatcher::ProgramSubcommand {
            program: "terraform".into(),
            subcommand: Some("apply".into())
        }
    );
    assert!(check(SecurityList::CredentialCommands, "aws").is_ok());
    // docker 整程序拒收，docker login 已在内置清单（重复）
    rejected(SecurityList::CredentialCommands, "docker", "解释器");
    rejected(SecurityList::CredentialCommands, "docker login", "已在清单");
    assert!(check(SecurityList::CredentialCommands, "docker compose").is_ok());
    for p in [
        "python", "bash", "node", "env", "xargs", "make", "sh", "sudo",
    ] {
        rejected(SecurityList::CredentialCommands, p, "解释器");
    }
    // 审查 1/F-9：解释器 / 启动器加第二个词（脚本）同样拒收
    for p in [
        "python script.py",
        "bash script.sh",
        "node script.js",
        "env python",
        "make deploy",
        "sudo git",
    ] {
        rejected(SecurityList::CredentialCommands, p, "解释器");
    }
    for p in ["rsync", "scp", "sftp", "rsync -a"] {
        rejected(SecurityList::CredentialCommands, p, "");
    }
    rejected(SecurityList::CredentialCommands, "scp x", "传出去");
    rejected(
        SecurityList::CredentialCommands,
        "git pull origin",
        "一个子命令",
    );
    rejected(
        SecurityList::CredentialCommands,
        "/usr/bin/git pull",
        "路径",
    );
    rejected(SecurityList::CredentialCommands, "git -c", "不是子命令");
    rejected(SecurityList::CredentialCommands, "gi* pull", "不是纯程序名");
    rejected(SecurityList::CredentialCommands, "git pull", "已在清单");
    rejected(SecurityList::CredentialCommands, "  ", "不能为空");
}

#[test]
fn sensitive_dir_rules() {
    assert_eq!(
        check(SecurityList::SensitiveDirs, "~/secret").unwrap(),
        RuleMatcher::PathUnder("/home/u/secret".into())
    );
    // 下面几条用 Unix 绝对路径字面量；Windows 上 `/x` 没有盘符，不算绝对路径（校验按平台判），只在 Unix 上比。
    #[cfg(unix)]
    {
        assert_eq!(
            check(SecurityList::SensitiveDirs, "/data/../srv/./x").unwrap(),
            RuleMatcher::PathUnder("/srv/x".into())
        );
        rejected(SecurityList::SensitiveDirs, WS, "包含项目工作区");
        rejected(SecurityList::SensitiveDirs, "/", "范围太大");
    }
    rejected(SecurityList::SensitiveDirs, "~/iWork", "包含项目工作区");
    rejected(SecurityList::SensitiveDirs, "~", "范围太大");
    rejected(SecurityList::SensitiveDirs, "relative/x", "绝对路径");
    rejected(SecurityList::SensitiveDirs, "~/Documents", "已在清单");
}

#[test]
fn writable_dir_rejects_ancestor_of_baseline() {
    rejected(SecurityList::WritableDirs, "~/.config", "gcloud");
    rejected(
        SecurityList::WritableDirs,
        "~/.ssh/sub",
        "baseline.cred.ssh",
    );
    rejected(
        SecurityList::WritableDirs,
        "~/Library/Application Support",
        "baseline.data-dir",
    );
    rejected(SecurityList::WritableDirs, "~", "范围太大");
    // 下面几条用 Unix 绝对路径字面量；Windows 上 `/x` 没有盘符，不算绝对路径（校验按平台判），只在 Unix 上比。
    #[cfg(unix)]
    {
        rejected(SecurityList::WritableDirs, "/", "范围太大");
        rejected(SecurityList::WritableDirs, "/usr/local/lib", "system");
        rejected(SecurityList::WritableDirs, "/etc", "system");
    }
    // 对照：普通目录接受
    assert_eq!(
        check(SecurityList::WritableDirs, "~/build-out").unwrap(),
        RuleMatcher::PathUnder("/home/u/build-out".into())
    );
}

#[test]
fn writable_dir_rejects_autorun_dir() {
    rejected(
        SecurityList::WritableDirs,
        "~/Library/LaunchAgents/x",
        "LaunchAgents",
    );
    rejected(SecurityList::WritableDirs, "~/.claude", "重叠");
    rejected(SecurityList::WritableDirs, "~/.zshrc", "autorun.shell-rc");
    rejected(SecurityList::WritableDirs, "~/.config/fish", "fish");
}

#[test]
fn writable_dir_rejects_already_writable() {
    rejected(SecurityList::WritableDirs, "~/.npm/_cacache", "已经可写");
    // 下面几条用 Unix 绝对路径字面量；Windows 上 `/x` 没有盘符，不算绝对路径（校验按平台判），只在 Unix 上比。
    #[cfg(unix)]
    {
        rejected(
            SecurityList::WritableDirs,
            &format!("{WS}/build"),
            "已经可写",
        );
        rejected(SecurityList::WritableDirs, "/tmp/x", "已经可写");
    }
}

#[test]
fn entry_shape() {
    let rules = ruleset();
    let roots = [cp(WS)];
    let ctx = ListValidationContext {
        paths: &env(),
        project_roots: &roots,
        rules: &rules,
    };
    let e = validate_list_entry(
        SecurityList::WritableDirs,
        " ~/out ",
        RuleScope::Global,
        &ctx,
        &fs(),
        Timestamp(5),
        RuleId::from_raw(RawUlid::from_u128(77)),
    )
    .unwrap();
    assert_eq!(e.layer, RuleLayer::WritableDirs);
    assert_eq!(e.effect, RuleEffect::Allow);
    assert_eq!(e.created_at, Timestamp(5));
    assert!(!e.is_builtin());
}
