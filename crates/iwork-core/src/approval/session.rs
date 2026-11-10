//! 会话快照、抽象约束与工具集（T07 方案 6.9；技术设计 7.0、7.2b）。纯函数。
//!
//! 会话启动时算一份 [`SessionSnapshot`]：T06 把 `constraints` 翻成 CLI 设置注入，并把**同一个**
//! 快照交给引擎（`decider_for` 的 `SessionKeys.snapshot`）。hook 侧的归一、求值、需凭据匹配都用它，
//! 规则表之后怎么变都不漂移；要立即生效就重建会话。

use iwork_domain::approval_mode::ApprovalMode;
use iwork_domain::common::{ToolGroup, ToolGroups};
use iwork_domain::id::Seq;
use iwork_domain::rule::{RuleEffect, RuleEntry, RuleLayer, RuleMatcher, RuleSetVersion};
use iwork_domain::session::{AllowWrite, NetworkPolicy, SessionConstraints, SessionToolSet};
use iwork_domain::turn::TurnKind;

use crate::approval::env::{EnvPaths, ScopeKeys};
use crate::approval::registry::{self, ToolClass};
use crate::approval::rules::matcher::path_of;
use crate::approval::rules::{Boundaries, applies_to, builtin};
use iwork_domain::action::CanonicalPath;
use iwork_ports::fs::PathResolver;

/// 会话绑定的规则部分：内置规则集 + 三张清单（范围过滤后）+ 临时敏感条目，以及由它们算出的约束与边界。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionSnapshot {
    pub rules: Vec<RuleEntry>,
    pub constraints: SessionConstraints,
    pub boundaries: Boundaries,
    pub ruleset_version: RuleSetVersion,
    /// 读取三张清单时 `rule` 流的最新序号；进审计的 `snapshot_lists_seq`
    pub lists_seq: Seq,
}

/// 会话启动时算快照。`user_lists` 是三张清单里的用户条目（内置条目由规则集给出）；按 `scope` 过滤。
///
/// 规则里的路径与动作路径要比较的是同一种东西：动作目标经 `PathResolver` 解析成真实位置，所以规则路径
/// 也要解析。受保护路径本身是符号链接（`~/.ssh` 链到别处）、或系统别名（macOS `/etc` → `/private/etc`）时，
/// 原条目保留（守住入口本身，如删除链接），另加一条指向真实位置的别名条目（审查 1/F-3）。
pub fn snapshot_for(
    mode: ApprovalMode,
    user_lists: &[RuleEntry],
    scope: &ScopeKeys,
    env: &EnvPaths,
    lists_seq: Seq,
    fs: &impl PathResolver,
) -> SessionSnapshot {
    let mut rules = builtin::builtin_rules(env);
    rules.extend(builtin::transient_sensitive_rules(env));
    rules.extend(
        user_lists
            .iter()
            .filter(|e| e.security_list().is_some() && !e.is_builtin() && applies_to(e, scope))
            .cloned(),
    );
    let aliases: Vec<RuleEntry> = rules
        .iter()
        .filter_map(|r| real_location_alias(r, fs))
        .collect();
    rules.extend(aliases);
    let boundaries = Boundaries::from_rules(&rules);
    let constraints = constraints_for(mode, &rules, env);
    SessionSnapshot {
        rules,
        constraints,
        boundaries,
        ruleset_version: builtin::version(),
        lists_seq,
    }
}

/// 规则路径的真实位置与写法不同时，返回指向真实位置的别名条目；ID 由原条目 ID 与真实路径派生。
fn real_location_alias(rule: &RuleEntry, fs: &impl PathResolver) -> Option<RuleEntry> {
    let path = path_of(&rule.matcher)?;
    let root = CanonicalPath(path.ancestors().last()?.to_path_buf());
    let real = fs.resolve(&root, path.to_str()?).ok()?.target;
    if fs.same_path(&real, path) {
        return None;
    }
    let mut alias = rule.clone();
    alias.matcher = replace_path(&rule.matcher, &real.0);
    alias.id = builtin::builtin_rule_id(&format!("alias:{}:{}", rule.id, real.0.display()));
    alias.label = format!("{}（真实位置 {}）", rule.label, real.0.display());
    Some(alias)
}

fn replace_path(m: &RuleMatcher, real: &std::path::Path) -> RuleMatcher {
    match m {
        RuleMatcher::PathUnder(_) => RuleMatcher::PathUnder(real.to_path_buf()),
        RuleMatcher::PathIs(_) => RuleMatcher::PathIs(real.to_path_buf()),
        RuleMatcher::All(ms) => {
            RuleMatcher::All(ms.iter().map(|x| replace_path(x, real)).collect())
        }
        other => other.clone(),
    }
}

/// 抽象沙箱约束。底线部分（`deny_read_write`、`read_only`）三档相同；只写白名单与沙箱外重试随档位。
pub fn constraints_for(
    mode: ApprovalMode,
    snapshot_rules: &[RuleEntry],
    env: &EnvPaths,
) -> SessionConstraints {
    let mut deny_read_write = Vec::new();
    let mut read_only = Vec::new();
    let mut writable = vec![env.workspace_root.0.clone()];
    writable.extend(env.tmp_dirs.iter().map(|p| p.0.clone()));
    writable.extend(env.dm_private_dir.iter().map(|p| p.0.clone()));
    let mut excluded_commands = Vec::new();
    for r in snapshot_rules {
        match (r.layer, &r.matcher, r.effect) {
            // 凭据与数据目录：沙箱禁读禁写。.git 内部与关键路径只由审批拦（它们的匹配器是组合条件）
            (
                RuleLayer::Baseline,
                RuleMatcher::PathUnder(p) | RuleMatcher::PathIs(p),
                RuleEffect::UserActionEligible,
            ) => deny_read_write.push(p.clone()),
            (RuleLayer::Baseline, RuleMatcher::PathUnder(p), RuleEffect::Allow) => {
                read_only.push(p.clone());
            }
            (RuleLayer::WritableCache | RuleLayer::WritableDirs, RuleMatcher::PathUnder(p), _) => {
                writable.push(p.clone());
            }
            _ => {}
        }
    }
    excluded_commands.extend(Boundaries::from_rules(snapshot_rules).credential_commands);
    SessionConstraints {
        deny_read_write,
        read_only,
        allow_write: match mode {
            ApprovalMode::FullAccess => AllowWrite::Anywhere,
            ApprovalMode::Manual | ApprovalMode::Auto => AllowWrite::Whitelist(writable),
        },
        excluded_commands,
        allow_unsandboxed_retry: mode != ApprovalMode::FullAccess,
        network: NetworkPolicy::Open,
    }
}

/// 四个分组。
pub const ALL_GROUPS: [ToolGroup; 4] = [
    ToolGroup::ReadAndSearch,
    ToolGroup::EditFiles,
    ToolGroup::RunCommands,
    ToolGroup::Web,
];

/// 本会话可用的工具（技术设计 7.2b）：助手回合 = 全部分组减去助手配置关掉的；AGENTS.md 草稿只有
/// 读文件与搜索；AI 审查不开任何工具。
pub fn tool_set_for(turn_kind: TurnKind, disabled_groups: &ToolGroups) -> SessionToolSet {
    match turn_kind {
        TurnKind::Assistant => SessionToolSet {
            enabled_groups: ALL_GROUPS
                .into_iter()
                .filter(|g| !disabled_groups.contains(g))
                .collect(),
            include_always_on: true,
        },
        TurnKind::SystemAgentsMdDraft => SessionToolSet {
            enabled_groups: [ToolGroup::ReadAndSearch].into_iter().collect(),
            include_always_on: false,
        },
        TurnKind::SystemAiReview => SessionToolSet {
            enabled_groups: ToolGroups::new(),
            include_always_on: false,
        },
    }
}

/// 工具在不在本会话的工具集里。永久禁用的工具永远不在。启动参数（[`tools_to_disable`]）与
/// hook 兜底（求值第 3 步）都按这一个函数判，出自同一份数据（验收 6b）。
pub fn in_tool_set(tool: &str, set: &SessionToolSet) -> bool {
    match registry::tool(tool) {
        Some(spec) if spec.class == ToolClass::Disabled => false,
        Some(spec) => match spec.group {
            Some(g) => set.enabled_groups.contains(&g),
            None => set.include_always_on,
        },
        // MCP 与表外工具：不属于四组，跟「始终开启」走
        None => set.include_always_on,
    }
}

/// 启动 CLI 时要关掉的内置工具（T06 翻成 `--disallowedTools`）；永久禁用工具永远在内。
pub fn tools_to_disable(set: &SessionToolSet) -> Vec<String> {
    registry::TOOLS
        .iter()
        .filter(|t| !in_tool_set(t.name, set))
        .map(|t| t.name.to_owned())
        .collect()
}
