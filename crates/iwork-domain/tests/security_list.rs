//! 安全清单条目就是规则条目（需求第 9 章「安全规则的组织」、技术设计 7.2c）：
//! 清单视图从 `RuleEntry` 推导，不另存一份。

use iwork_domain::common::Actor;
use iwork_domain::common::Timestamp;
use iwork_domain::id::{ApprovalId, ProjectId, RawUlid, RuleId};
use iwork_domain::rule::{
    RuleEffect, RuleEntry, RuleLayer, RuleMatcher, RuleScope, RuleSetVersion, RuleSource,
};
use iwork_domain::security_list::SecurityList;

fn rule(layer: RuleLayer, source: RuleSource) -> RuleEntry {
    RuleEntry {
        id: RuleId::from_raw(RawUlid::from_u128(1)),
        layer,
        matcher: RuleMatcher::PathUnder("/home/me/.aws".into()),
        effect: RuleEffect::Deny,
        source,
        scope: RuleScope::Project(ProjectId::from_raw(RawUlid::from_u128(2))),
        created_at: Timestamp(0),
        label: String::new(),
        applies_in: Default::default(),
    }
}

fn builtin() -> RuleSource {
    RuleSource::Builtin {
        ruleset_version: RuleSetVersion("1".into()),
    }
}

#[test]
fn security_list_is_derived_from_rule_layer() {
    let cases = [
        (
            RuleLayer::CredentialCommands,
            SecurityList::CredentialCommands,
        ),
        (RuleLayer::SensitiveDirs, SecurityList::SensitiveDirs),
        (RuleLayer::WritableDirs, SecurityList::WritableDirs),
    ];
    for (layer, list) in cases {
        assert_eq!(rule(layer, RuleSource::User).security_list(), Some(list));
        assert_eq!(list.layer(), layer);
    }
    // 对照：不是清单层的规则不属于任何清单
    for layer in [
        RuleLayer::Baseline,
        RuleLayer::Remembered,
        RuleLayer::WritableCache,
        RuleLayer::RejectList,
    ] {
        assert_eq!(
            rule(layer, RuleSource::User).security_list(),
            None,
            "{layer:?}"
        );
    }
}

#[test]
fn builtin_flag_is_derived_from_rule_source() {
    assert!(rule(RuleLayer::SensitiveDirs, builtin()).is_builtin());
    assert!(!rule(RuleLayer::SensitiveDirs, RuleSource::User).is_builtin());
    assert!(
        !rule(
            RuleLayer::SensitiveDirs,
            RuleSource::Remembered {
                approval_id: ApprovalId::from_raw(RawUlid::from_u128(3)),
                by: Actor("u".into())
            }
        )
        .is_builtin()
    );
}
