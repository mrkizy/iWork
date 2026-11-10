//! 内置规则集 `m1-1` 与规则集拼装（T07 方案 4.2、4.3；第 3 步验收）。

mod support;

use std::collections::BTreeSet;

use iwork_core::approval::rules::builtin::{self, builtin_rule_id, builtin_rules};
use iwork_core::approval::rules::{RuleSetError, build_ruleset, revocable, rules_owned_by};
use iwork_domain::approval_mode::ApprovalMode;
use iwork_domain::common::{Actor, Timestamp};
use iwork_domain::id::{ApprovalId, RawUlid, RuleId};
use iwork_domain::rule::{RuleEffect, RuleEntry, RuleLayer, RuleMatcher, RuleScope, RuleSource};
use iwork_ports::fs::PathResolver;
use support::*;

/// 内置条目的 (slug, ID, 层, 档位, 匹配器) 快照：改 slug、改顺序、改条目都会红。
/// 更新：`IWORK_UPDATE_SNAPSHOTS=1 cargo test -p iwork-core --test builtin_rules`。
#[test]
fn builtin_ids_stable() {
    let mut text = String::new();
    for e in builtin_rules(&env()) {
        text.push_str(&format!(
            "{} {} {:?} {:?} {:?} {:?}\n",
            e.label, e.id, e.layer, e.effect, e.applies_in, e.matcher
        ));
    }
    let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join(format!(
        "tests/snapshots/builtin_{}_macos.txt",
        iwork_core::approval::rules::builtin::RULESET_VERSION
    ));
    if std::env::var_os("IWORK_UPDATE_SNAPSHOTS").is_some() {
        std::fs::write(&path, &text).unwrap();
    }
    let expected = std::fs::read_to_string(&path).expect("快照文件存在");
    assert_eq!(
        text, expected,
        "内置规则集变了：改条目要升 RULESET_VERSION 并更新快照"
    );
    // 对照：ID 只由 slug 决定，与环境无关
    assert_eq!(
        builtin_rule_id("baseline.cred.ssh"),
        builtin_rules(&env_at("/elsewhere"))
            .iter()
            .find(|e| e.label == "baseline.cred.ssh")
            .unwrap()
            .id
    );
    assert_ne!(builtin_rule_id("a"), builtin_rule_id("b"));
}

#[test]
fn builtin_ids_unique_and_versioned() {
    let rules = builtin_rules(&env());
    let ids: BTreeSet<_> = rules.iter().map(|e| e.id).collect();
    assert_eq!(ids.len(), rules.len());
    assert!(rules.iter().all(|e| e.is_builtin() && !revocable(e)));
    assert_eq!(builtin::version().0, "m1-2");
}

/// 需求第 9 章凭据目录清单逐条：期望路径手写，不从条目推。
#[test]
fn builtin_covers_credential_dirs() {
    let rules = builtin_rules(&env());
    let baseline_paths: Vec<String> = rules
        .iter()
        .filter(|e| e.layer == RuleLayer::Baseline && e.effect == RuleEffect::UserActionEligible)
        .filter_map(|e| match &e.matcher {
            RuleMatcher::PathUnder(p) | RuleMatcher::PathIs(p) => Some(p.display().to_string()),
            _ => None,
        })
        .collect();
    for want in [
        "/home/u/.ssh",
        "/home/u/.gnupg",
        "/home/u/.aws",
        "/home/u/.azure",
        "/home/u/.kube",
        "/home/u/.config/gcloud",
        "/home/u/.docker/config.json",
        "/home/u/.netrc",
        "/home/u/.git-credentials",
        "/home/u/.npmrc",
        "/home/u/.pypirc",
        "/home/u/Library/Keychains",
        "/home/u/.claude/.credentials.json",
        "/home/u/.codex/auth.json",
        DATA,
    ] {
        assert!(baseline_paths.iter().any(|p| p == want), "缺 {want}");
    }
}

#[test]
fn cache_dirs_do_not_cover_credential_paths() {
    let rules = builtin_rules(&env());
    let fs = fs();
    let paths = |layer: RuleLayer| -> Vec<_> {
        rules
            .iter()
            .filter(|e| e.layer == layer)
            .filter_map(|e| match &e.matcher {
                RuleMatcher::PathUnder(p) | RuleMatcher::PathIs(p) => {
                    Some(cp(&p.display().to_string()))
                }
                _ => None,
            })
            .collect()
    };
    let cache = paths(RuleLayer::WritableCache);
    let creds = paths(RuleLayer::Baseline);
    assert!(cache.len() > 10, "对照：缓存条目不为空");
    for c in &cache {
        for b in &creds {
            assert!(
                !fs.is_under(b, c) && !fs.is_under(c, b),
                "{c:?} 与底线 {b:?} 互相包含"
            );
        }
    }
}

/// Q3：内置需凭据命令除 gh、ssh 外全部写到子命令；scp / sftp / rsync 不内置且在拒收名单。
#[test]
fn credential_builtin_is_subcommand_level() {
    let rules = builtin_rules(&env());
    let mut whole = Vec::new();
    for e in rules
        .iter()
        .filter(|e| e.layer == RuleLayer::CredentialCommands)
    {
        let RuleMatcher::ProgramSubcommand {
            program,
            subcommand,
        } = &e.matcher
        else {
            panic!("{e:?}");
        };
        if subcommand.is_none() {
            whole.push(program.as_str());
        }
        assert!(!["scp", "sftp", "rsync", "aws", "gcloud", "kubectl"].contains(&program.as_str()));
    }
    assert_eq!(whole, vec!["gh", "ssh"]);
    let rejected: Vec<_> = rules
        .iter()
        .filter(|e| e.layer == RuleLayer::RejectList)
        .map(|e| e.label.as_str())
        .collect();
    for p in [
        "reject.rsync",
        "reject.scp",
        "reject.sftp",
        "reject.python",
        "reject.bash",
    ] {
        assert!(rejected.contains(&p), "{p}");
    }
}

/// 必问层：push 三档；删除、发布、写自动执行配置只在手动、自动档。
#[test]
fn must_ask_modes() {
    let rules = builtin_rules(&env());
    let modes = |label: &str| {
        rules
            .iter()
            .find(|e| e.label == label)
            .unwrap()
            .applies_in
            .clone()
    };
    assert!(modes("mustask.push").is_empty(), "空集 = 全部档位");
    for l in [
        "mustask.delete",
        "mustask.publish",
        "mustask.autorun-config",
    ] {
        assert_eq!(
            modes(l),
            [ApprovalMode::Manual, ApprovalMode::Auto]
                .into_iter()
                .collect()
        );
    }
}

fn user_entry(n: u128, layer: RuleLayer, scope: RuleScope) -> RuleEntry {
    RuleEntry {
        id: RuleId::from_raw(RawUlid::from_u128(n)),
        layer,
        matcher: RuleMatcher::ActionKindIs(iwork_domain::action::ActionKind::Read),
        effect: RuleEffect::Allow,
        source: RuleSource::Remembered {
            approval_id: ApprovalId::from_raw(RawUlid::from_u128(9)),
            by: Actor("user".into()),
        },
        scope,
        created_at: Timestamp(1),
        label: format!("u{n}"),
        applies_in: Default::default(),
    }
}

#[test]
fn merged_ruleset_invariants() {
    let snap = builtin_rules(&env());
    let live = vec![
        user_entry(1, RuleLayer::Remembered, RuleScope::Global),
        user_entry(2, RuleLayer::Remembered, RuleScope::Project(project())),
        user_entry(
            3,
            RuleLayer::Remembered,
            RuleScope::Project(other_project()),
        ),
        user_entry(
            4,
            RuleLayer::ProjectAllow,
            RuleScope::Conversation(conversation()),
        ),
        user_entry(
            5,
            RuleLayer::UserDeny,
            RuleScope::Conversation(iwork_domain::id::ConversationId::from_raw(
                RawUlid::from_u128(0x99),
            )),
        ),
    ];
    let set = build_ruleset(&snap, &live, &scope()).unwrap();
    let ids: BTreeSet<_> = set.entries.iter().map(|e| e.id).collect();
    assert_eq!(ids.len(), set.entries.len(), "ID 唯一");
    assert!(snap.iter().all(|e| ids.contains(&e.id)), "内置条目全部在");
    // 范围过滤：本项目、全局、本对话的在；他项目、他对话的不在
    for (n, kept) in [(1, true), (2, true), (3, false), (4, true), (5, false)] {
        assert_eq!(
            ids.contains(&RuleId::from_raw(RawUlid::from_u128(n))),
            kept,
            "u{n}"
        );
    }
    // 私聊选「私聊」：项目级条目都不生效
    let mut dm = scope();
    dm.project = None;
    let set = build_ruleset(&snap, &live, &dm).unwrap();
    assert!(!set.entries.iter().any(|e| e.label == "u2"));

    // 用户条目出现在底线层：拼装失败
    let mut bad = user_entry(7, RuleLayer::Remembered, RuleScope::Global);
    bad.layer = RuleLayer::Baseline;
    let mut snap_bad = snap.clone();
    snap_bad.push(bad);
    assert!(matches!(
        build_ruleset(&snap_bad, &[], &scope()),
        Err(RuleSetError::UserEntryInProtectedLayer { .. })
    ));
    // 活规则不在活规则层
    let mut not_live = user_entry(8, RuleLayer::Remembered, RuleScope::Global);
    not_live.layer = RuleLayer::SensitiveDirs;
    assert!(matches!(
        build_ruleset(&snap, &[not_live], &scope()),
        Err(RuleSetError::NotLive { .. })
    ));
    // ID 重复
    assert!(matches!(
        build_ruleset(&snap, &[live[0].clone(), live[0].clone()], &scope()),
        Err(RuleSetError::DuplicateId(_))
    ));
}

#[test]
fn rules_owned_by_scope() {
    let rules = vec![
        user_entry(
            1,
            RuleLayer::Remembered,
            RuleScope::Conversation(conversation()),
        ),
        user_entry(2, RuleLayer::Remembered, RuleScope::Global),
    ];
    assert_eq!(
        rules_owned_by(&rules, RuleScope::Conversation(conversation())),
        vec![RuleId::from_raw(RawUlid::from_u128(1))]
    );
}
