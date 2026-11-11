//! 规则集的拼装（T07 方案 4.3）：会话快照（内置 + 三张清单）∪ 活规则（记住、项目、拒绝），范围过滤。

pub mod builtin;
pub mod matcher;
pub mod security_list;

use std::collections::BTreeSet;

use iwork_domain::action::CanonicalPath;
use iwork_domain::id::RuleId;
use iwork_domain::rule::{RuleEntry, RuleLayer, RuleMatcher, RuleScope, RuleSet};
use iwork_domain::session::CommandPattern;

use crate::approval::env::ScopeKeys;

/// 这条条目在 `scope` 下是否生效。
pub fn applies_to(entry: &RuleEntry, scope: &ScopeKeys) -> bool {
    match entry.scope {
        RuleScope::Global => true,
        RuleScope::Project(p) => scope.project == Some(p),
        RuleScope::Assistant(a) => scope.assistant == a,
        RuleScope::Conversation(c) => scope.conversation == c,
    }
}

/// 用户条目（非内置）允许出现的层：三张清单与三种活规则。出现在底线、必问、默认、工具配置等层的
/// 用户条目说明持久化层被篡改或程序缺陷，拒绝拼装（防止放宽底线）。
pub fn user_layer_allowed(layer: RuleLayer) -> bool {
    matches!(
        layer,
        RuleLayer::CredentialCommands
            | RuleLayer::SensitiveDirs
            | RuleLayer::WritableDirs
            | RuleLayer::Remembered
            | RuleLayer::ProjectAllow
            | RuleLayer::UserDeny
    )
}

/// 活规则所在的层：不进 CLI 设置，下一次求值即用（AC13）。
pub fn is_live_layer(layer: RuleLayer) -> bool {
    matches!(
        layer,
        RuleLayer::Remembered | RuleLayer::ProjectAllow | RuleLayer::UserDeny
    )
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum RuleSetError {
    #[error("规则 ID 重复：{0}")]
    DuplicateId(RuleId),
    #[error("用户条目 {id} 出现在不允许用户条目的层 {layer:?}")]
    UserEntryInProtectedLayer { id: RuleId, layer: RuleLayer },
    #[error("活规则 {id} 不在活规则层（{layer:?}）")]
    NotLive { id: RuleId, layer: RuleLayer },
}

/// 拼出求值用的规则集：会话快照的条目 ∪ 活规则，范围过滤后。
///
/// 不变量（`merged_ruleset_invariants`）：ID 唯一；用户条目的层只能是 [`user_layer_allowed`]；
/// 活规则只能在活规则层。
pub fn build_ruleset(
    snapshot_rules: &[RuleEntry],
    live: &[RuleEntry],
    scope: &ScopeKeys,
) -> Result<RuleSet, RuleSetError> {
    let mut ids = BTreeSet::new();
    let mut entries = Vec::with_capacity(snapshot_rules.len() + live.len());
    for e in live {
        if !is_live_layer(e.layer) {
            return Err(RuleSetError::NotLive {
                id: e.id,
                layer: e.layer,
            });
        }
    }
    for e in snapshot_rules.iter().chain(live) {
        if !ids.insert(e.id) {
            return Err(RuleSetError::DuplicateId(e.id));
        }
        if !e.is_builtin() && !user_layer_allowed(e.layer) {
            return Err(RuleSetError::UserEntryInProtectedLayer {
                id: e.id,
                layer: e.layer,
            });
        }
        if applies_to(e, scope) {
            entries.push(e.clone());
        }
    }
    Ok(RuleSet {
        version: builtin::version(),
        entries,
    })
}

/// 内置条目不可撤销（`RULE_NOT_REVOCABLE`）。
pub fn revocable(entry: &RuleEntry) -> bool {
    !entry.is_builtin()
}

/// 删除对话 / 助手时随之删除的规则（T10 据此发 `RulesDeleted`）。
pub fn rules_owned_by(rules: &[RuleEntry], scope: RuleScope) -> Vec<RuleId> {
    rules
        .iter()
        .filter(|r| !r.is_builtin() && r.scope == scope)
        .map(|r| r.id)
        .collect()
}

/// 路径清单条目的形状。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PathRule {
    Under(CanonicalPath),
    Is(CanonicalPath),
}

impl PathRule {
    fn of(m: &RuleMatcher) -> Option<Self> {
        match m {
            RuleMatcher::PathUnder(p) => Some(Self::Under(CanonicalPath(p.clone()))),
            RuleMatcher::PathIs(p) => Some(Self::Is(CanonicalPath(p.clone()))),
            _ => None,
        }
    }

    pub fn path(&self) -> &CanonicalPath {
        match self {
            Self::Under(p) | Self::Is(p) => p,
        }
    }
}

/// 从会话快照抽出的边界（方案 3.8）：归一要用。全部来自会话快照，运行中不变。
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Boundaries {
    pub sensitive_dirs: Vec<CanonicalPath>,
    pub writable_dirs: Vec<CanonicalPath>,
    pub writable_cache: Vec<CanonicalPath>,
    pub autorun: Vec<PathRule>,
    pub credential_commands: Vec<CommandPattern>,
}

impl Boundaries {
    /// `rules` 已按范围过滤（会话快照的条目）。
    pub fn from_rules(rules: &[RuleEntry]) -> Self {
        let mut b = Self::default();
        for r in rules {
            match (r.layer, &r.matcher) {
                (RuleLayer::SensitiveDirs, RuleMatcher::PathUnder(p)) => {
                    b.sensitive_dirs.push(CanonicalPath(p.clone()));
                }
                (RuleLayer::WritableDirs, RuleMatcher::PathUnder(p)) => {
                    b.writable_dirs.push(CanonicalPath(p.clone()));
                }
                (RuleLayer::WritableCache, RuleMatcher::PathUnder(p)) => {
                    b.writable_cache.push(CanonicalPath(p.clone()));
                }
                (RuleLayer::AutorunConfig, m) => b.autorun.extend(PathRule::of(m)),
                (
                    RuleLayer::CredentialCommands,
                    RuleMatcher::ProgramSubcommand {
                        program,
                        subcommand,
                    },
                ) => b.credential_commands.push(CommandPattern {
                    program: program.clone(),
                    subcommand: subcommand.clone(),
                }),
                _ => {}
            }
        }
        b
    }
}
