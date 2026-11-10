//! 会话快照、抽象约束与工具集（T07 方案 6.9；验收 6a、6b 的纯部分）。

mod support;

use iwork_core::approval::rules::builtin::builtin_rules;
use iwork_core::approval::session::{constraints_for, in_tool_set, tool_set_for, tools_to_disable};
use iwork_domain::approval_mode::ApprovalMode;
use iwork_domain::common::{Timestamp, ToolGroup, ToolGroups};
use iwork_domain::id::{RawUlid, RuleId, Seq};
use iwork_domain::rule::{RuleEffect, RuleEntry, RuleLayer, RuleMatcher, RuleScope, RuleSource};
use iwork_domain::session::{AllowWrite, CommandPattern};
use iwork_domain::turn::TurnKind;
use support::*;

#[test]
fn tool_set_by_turn_kind() {
    let none = ToolGroups::new();
    let a = tool_set_for(TurnKind::Assistant, &none);
    assert_eq!(a.enabled_groups.len(), 4);
    assert!(a.include_always_on);
    let d = tool_set_for(TurnKind::SystemAgentsMdDraft, &none);
    assert_eq!(
        d.enabled_groups,
        [ToolGroup::ReadAndSearch].into_iter().collect()
    );
    assert!(!d.include_always_on);
    let r = tool_set_for(TurnKind::SystemAiReview, &none);
    assert!(r.enabled_groups.is_empty() && !r.include_always_on);
    // AI 审查回合：一个工具都没有
    assert!(!in_tool_set("Read", &r) && !in_tool_set("Agent", &r) && !in_tool_set("mcp__x__y", &r));
    // 草稿回合：只有读与搜索
    assert!(in_tool_set("Grep", &d) && !in_tool_set("Bash", &d) && !in_tool_set("Edit", &d));
}

/// 验收 6b：关掉「编辑文件」后启动参数不含 Edit / Write / NotebookEdit；子代理、Skill 不受影响；
/// 永久禁用的工具永远在关闭清单里。
#[test]
fn disabling_edit_group_removes_edit_tools() {
    let disabled: ToolGroups = [ToolGroup::EditFiles].into_iter().collect();
    let set = tool_set_for(TurnKind::Assistant, &disabled);
    let off = tools_to_disable(&set);
    for t in [
        "Edit",
        "Write",
        "NotebookEdit",
        "RemoteTrigger",
        "CronCreate",
        "ScheduleWakeup",
    ] {
        assert!(off.iter().any(|o| o == t), "{t} 应关掉");
    }
    for t in ["Agent", "Skill", "Read", "Bash"] {
        assert!(!off.iter().any(|o| o == t), "{t} 不应关掉");
        assert!(in_tool_set(t, &set));
    }
    // 对照：没关任何分组时只有永久禁用的
    let full = tools_to_disable(&tool_set_for(TurnKind::Assistant, &ToolGroups::new()));
    assert_eq!(
        full,
        vec![
            "RemoteTrigger",
            "CronCreate",
            "CronDelete",
            "CronList",
            "ScheduleWakeup"
        ]
    );
}

#[test]
fn constraints_baseline_mode_independent() {
    let rules = builtin_rules(&env());
    let c: Vec<_> = [
        ApprovalMode::Manual,
        ApprovalMode::Auto,
        ApprovalMode::FullAccess,
    ]
    .into_iter()
    .map(|m| constraints_for(m, &rules, &env()))
    .collect();
    for x in &c[1..] {
        assert_eq!(x.deny_read_write, c[0].deny_read_write);
        assert_eq!(x.read_only, c[0].read_only);
        assert_eq!(x.excluded_commands, c[0].excluded_commands);
    }
    let deny: Vec<String> = c[0]
        .deny_read_write
        .iter()
        .map(|p| p.display().to_string())
        .collect();
    assert!(deny.contains(&"/home/u/.ssh".to_string()));
    assert!(deny.contains(&DATA.to_string()));
    // 敏感目录只由审批判定，不进沙箱（v1.52）；.git 与关键路径只由审批拦
    assert!(
        !deny
            .iter()
            .any(|p| p.contains("Documents") || p.ends_with(".git") || p == "/usr")
    );
    assert_eq!(c[0].read_only.len(), 2);
    match &c[0].allow_write {
        AllowWrite::Whitelist(w) => {
            let w: Vec<String> = w.iter().map(|p| p.display().to_string()).collect();
            assert!(w.contains(&WS.to_string()) && w.contains(&TMP.to_string()));
            assert!(w.contains(&"/home/u/.npm".to_string()));
        }
        AllowWrite::Anywhere => panic!("手动档是白名单"),
    }
    assert_eq!(c[2].allow_write, AllowWrite::Anywhere);
    assert!(c[0].allow_unsandboxed_retry && c[1].allow_unsandboxed_retry);
    assert!(!c[2].allow_unsandboxed_retry);
}

fn user_list(n: u128, layer: RuleLayer, matcher: RuleMatcher, scope: RuleScope) -> RuleEntry {
    RuleEntry {
        id: RuleId::from_raw(RawUlid::from_u128(n)),
        layer,
        matcher,
        effect: RuleEffect::Allow,
        source: RuleSource::User,
        scope,
        created_at: Timestamp(1),
        label: format!("user{n}"),
        applies_in: Default::default(),
    }
}

#[test]
fn snapshot_includes_scoped_user_lists_and_transient_sensitive() {
    let lists = vec![
        user_list(
            1,
            RuleLayer::CredentialCommands,
            RuleMatcher::ProgramSubcommand {
                program: "terraform".into(),
                subcommand: Some("apply".into()),
            },
            RuleScope::Project(project()),
        ),
        user_list(
            2,
            RuleLayer::WritableDirs,
            RuleMatcher::PathUnder("/srv/out".into()),
            RuleScope::Project(other_project()),
        ),
        // 不是清单条目：快照里不收
        user_list(
            3,
            RuleLayer::Remembered,
            RuleMatcher::Any,
            RuleScope::Global,
        ),
    ];
    let s = snapshot_on(ApprovalMode::Manual, &lists, &env(), Seq(41), &fs());
    assert_eq!(s.lists_seq, Seq(41));
    assert!(s.constraints.excluded_commands.contains(&CommandPattern {
        program: "terraform".into(),
        subcommand: Some("apply".into())
    }));
    assert!(
        !s.rules
            .iter()
            .any(|r| r.label == "user2" || r.label == "user3")
    );
    assert!(
        s.boundaries
            .sensitive_dirs
            .contains(&cp("/home/u/iWork/projects/other"))
    );
    assert!(
        s.boundaries
            .sensitive_dirs
            .contains(&cp("/home/u/iWork/assistants/bob"))
    );
    assert_eq!(s.ruleset_version.0, "m1-2");
}
