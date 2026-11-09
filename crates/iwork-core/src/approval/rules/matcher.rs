//! 规则条目对动作的匹配（T07 方案 4.1 语义约定）。
//!
//! - `PathUnder(d)`：目标路径在 `d` 之下（含相等），用 `PathResolver::is_under`。目标是路径集合时任一命中即命中。
//! - `PathIs(p)`：目标等于 `p`。**例外**：底线层里对删除动作，还匹配「目标是 `p` 的上级」（需求第 9 章
//!   「删除目标是下列路径之一或它们的上级目录」）。别的层是精确匹配。目标是命令时任何路径匹配器都不命中。
//! - `ProgramSubcommand{program, subcommand}`：目标是命令，程序名相等，且条目无子命令或子命令相等。
//! - `ToolName(n)`：目标是 `ToolName(n)`。
//! - `ActionKindIs`、`Flag`、`All`、`Any` 字面义。
//!
//! 比较都经 `PathResolver`（大小写不敏感的文件系统上忽略大小写），不用 `Path::starts_with`。

use std::path::Path;

use iwork_domain::action::{Action, ActionKind, ActionTarget, CanonicalPath};
use iwork_domain::rule::{RuleLayer, RuleMatcher};
use iwork_ports::fs::PathResolver;

/// `entry_layer` 决定 `PathIs` 的上级语义。
pub fn matches(
    m: &RuleMatcher,
    entry_layer: RuleLayer,
    action: &Action,
    fs: &impl PathResolver,
) -> bool {
    let ancestor_too = entry_layer == RuleLayer::Baseline && action.kind == ActionKind::Delete;
    match m {
        RuleMatcher::PathUnder(dir) => any_path(&action.target, |p| fs.is_under(p, dir)),
        RuleMatcher::PathIs(want) => any_path(&action.target, |p| {
            fs.same_path(p, want) || (ancestor_too && fs.is_under(want, p))
        }),
        RuleMatcher::ProgramSubcommand {
            program,
            subcommand,
        } => match &action.target {
            ActionTarget::Command {
                program: p,
                subcommand: s,
                ..
            } => {
                p == program
                    && subcommand
                        .as_ref()
                        .is_none_or(|want| s.as_ref() == Some(want))
            }
            _ => false,
        },
        RuleMatcher::ToolName(n) => matches!(&action.target, ActionTarget::ToolName(t) if t == n),
        RuleMatcher::ActionKindIs(k) => action.kind == *k,
        RuleMatcher::Flag(f) => action.flags.has(*f),
        RuleMatcher::All(ms) => ms.iter().all(|m| matches(m, entry_layer, action, fs)),
        RuleMatcher::Any => true,
    }
}

/// 条目里最具体的路径有几段：底线层取「最具体」的命中（方案 4.2）。没有路径条件为 0。
pub fn specificity(m: &RuleMatcher) -> usize {
    match m {
        RuleMatcher::PathUnder(p) | RuleMatcher::PathIs(p) => p.components().count(),
        RuleMatcher::All(ms) => ms.iter().map(specificity).max().unwrap_or(0),
        _ => 0,
    }
}

/// 条目里出现的路径（清单抽边界用）。
pub fn path_of(m: &RuleMatcher) -> Option<&Path> {
    match m {
        RuleMatcher::PathUnder(p) | RuleMatcher::PathIs(p) => Some(p),
        RuleMatcher::All(ms) => ms.iter().find_map(path_of),
        _ => None,
    }
}

fn any_path(target: &ActionTarget, f: impl Fn(&CanonicalPath) -> bool) -> bool {
    match target {
        ActionTarget::Path(p) => f(p),
        ActionTarget::Paths(ps) => ps.iter().any(f),
        _ => false,
    }
}
