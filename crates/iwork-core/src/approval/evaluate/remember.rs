//! 记住 → 规则（T07 方案 6.4；FR11 v1.61；技术设计 7.3 第 2 条；AC40）。
//!
//! 记住的规则**必须**带动作种类（`All([ActionKindIs, …])`），路径或程序只是第二个条件：
//! 「允许读 notes/」不放行写；普通执行的批准不会变成沙箱外执行的批准。
//! 必问、强制询问、解析不了、清单外重试、要问的子动作不止一个的调用，只能允许一次。

use std::path::Path;

use iwork_domain::action::{Action, ActionKind, ActionTarget, SandboxPlacement};
use iwork_domain::approval::Approval;
use iwork_domain::approval_mode::RememberScope;
use iwork_domain::common::{Actor, Timestamp};
use iwork_domain::id::RuleId;
use iwork_domain::rule::{
    RuleEffect, RuleEntry, RuleLayer, RuleMatcher, RuleScope, RuleSource, Verdict,
};

use crate::approval::action::NormalizedCall;
use crate::approval::env::ScopeKeys;
use crate::approval::evaluate::CallEvaluation;

/// 卡片上提供的记住范围与范围文案。
pub fn offered_remember(
    call: &NormalizedCall,
    eval: &CallEvaluation,
    scope: &ScopeKeys,
) -> (Vec<RememberScope>, String) {
    let once = (vec![RememberScope::Once], String::new());
    let asks = matches!(
        eval.evaluation.verdict,
        Verdict::Ask(_) | Verdict::OutsideSandbox(_)
    );
    if !asks
        || !eval
            .constraints
            .remember_eligible(call.bypass_sandbox_requested)
    {
        return once;
    }
    let asked: Vec<usize> = eval
        .per_action
        .iter()
        .enumerate()
        .filter(|(_, e)| e.verdict != Verdict::Allow)
        .map(|(i, _)| i)
        .collect();
    let [i] = asked[..] else {
        return once;
    };
    let action = &call.actions[i];
    let Some((_, label)) = rule_shape(action) else {
        return once;
    };
    let mut scopes = vec![RememberScope::Once];
    if action.kind == ActionKind::OutsideSandbox {
        // 清单内的沙箱外执行只能记到本项目（FR11）
        if call.placement != SandboxPlacement::Inside && scope.project.is_some() {
            scopes.push(RememberScope::Project);
        }
    } else {
        scopes.push(RememberScope::Conversation);
        if scope.project.is_some() {
            scopes.push(RememberScope::Project);
        }
        scopes.push(RememberScope::Forever);
    }
    if scopes.len() == 1 {
        return once;
    }
    (scopes, label)
}

/// 由代表动作得出规则的匹配条件与文案；不能记住的种类返回 `None`。
fn rule_shape(action: &Action) -> Option<(RuleMatcher, String)> {
    let kind = RuleMatcher::ActionKindIs(action.kind);
    match (action.kind, &action.target) {
        (
            ActionKind::Execute | ActionKind::OutsideSandbox,
            ActionTarget::Command {
                program,
                subcommand,
                ..
            },
        ) => {
            let text = match subcommand {
                Some(s) => format!("`{program} {s} …`"),
                None => format!("`{program} …`"),
            };
            let label = if action.kind == ActionKind::OutsideSandbox {
                format!("{text} 直接在沙箱外运行")
            } else {
                format!("{text} 不再问")
            };
            Some((
                RuleMatcher::All(vec![
                    kind,
                    RuleMatcher::ProgramSubcommand {
                        program: program.clone(),
                        subcommand: subcommand.clone(),
                    },
                ]),
                label,
            ))
        }
        (ActionKind::Write | ActionKind::Read, ActionTarget::Path(p)) => {
            let dir = p.0.parent().unwrap_or(Path::new("/")).to_path_buf();
            let verb = if action.kind == ActionKind::Write {
                "写"
            } else {
                "读"
            };
            let label = format!("{verb} `{}/` 不再问", dir.display());
            Some((
                RuleMatcher::All(vec![kind, RuleMatcher::PathUnder(dir)]),
                label,
            ))
        }
        (ActionKind::McpCall | ActionKind::Unclassified, ActionTarget::ToolName(n)) => Some((
            RuleMatcher::All(vec![kind, RuleMatcher::ToolName(n.clone())]),
            format!("`{n}` 不再问"),
        )),
        _ => None,
    }
}

/// 用户允许并选了记住范围：生成规则；`Once` 返回 `None`。
pub fn remember_rule(
    approval: &Approval,
    remember: RememberScope,
    keys: &ScopeKeys,
    by: Actor,
    now: Timestamp,
    id: RuleId,
) -> Option<RuleEntry> {
    let rule_scope = match remember {
        RememberScope::Once => return None,
        RememberScope::Conversation => RuleScope::Conversation(approval.conversation_id),
        RememberScope::Project => RuleScope::Project(keys.project?),
        RememberScope::Forever => RuleScope::Assistant(keys.assistant),
    };
    let action = approval
        .call
        .as_ref()
        .map_or(&approval.action, |c| &c.actions[c.representative as usize]);
    let (matcher, label) = rule_shape(action)?;
    Some(RuleEntry {
        id,
        layer: RuleLayer::Remembered,
        matcher,
        effect: RuleEffect::Allow,
        source: RuleSource::Remembered {
            approval_id: approval.id,
            by,
        },
        scope: rule_scope,
        created_at: now,
        label,
        applies_in: Default::default(),
    })
}
