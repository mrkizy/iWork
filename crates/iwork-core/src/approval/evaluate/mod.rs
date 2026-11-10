//! 七步求值（T07 方案第 5 节；技术设计 7.2）。纯函数：不读时钟、不生成 ID、不碰 I/O
//! （`PathResolver` 只做路径比较）。
//!
//! 每个动作：① 底线 → ② 拒绝记忆 → ③ 工具配置与边界 → ④ 拒绝规则 → ⑤ 必问 → ⑥ 放行规则 → ⑦ 默认。
//! 一条调用取最严的子动作结果，并保留每个子动作的约束（[`CallConstraints`]）。

pub mod proposal;
pub mod remember;
pub mod strictness;

use std::collections::BTreeSet;

use iwork_domain::action::{Action, ActionFingerprint, ActionFlag, ActionKind};
use iwork_domain::approval_mode::ApprovalMode;
use iwork_domain::common::ToolGroups;
use iwork_domain::id::RuleId;
use iwork_domain::rule::{
    AskReason, DenyReason, Evaluation, RuleEffect, RuleEntry, RuleLayer, RuleMatcher, RuleSet,
    Verdict,
};
use iwork_domain::session::SessionToolSet;
use iwork_domain::turn::TurnKind;
use iwork_ports::fs::PathResolver;

use crate::approval::action::{CallShape, NormalizedCall};
use crate::approval::denial::{DenialMemory, DenialPhase};
use crate::approval::registry;
use crate::approval::rules::matcher::{matches, specificity};
use crate::approval::session::in_tool_set;
use strictness::strictness;

/// 求值上下文。
pub struct EvalContext<'a> {
    /// `effective_mode()` 的结果，调用方算
    pub mode: ApprovalMode,
    pub turn_kind: TurnKind,
    /// 本会话启动时的工具集
    pub tool_set: &'a SessionToolSet,
    /// 助手配置里关掉的分组
    pub disabled_groups: &'a ToolGroups,
    /// 锁下的拒绝记忆（只读）
    pub denial: &'a DenialMemory,
}

/// 一个子动作带给整条调用的约束。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct CallConstraints {
    /// 有子动作命中必问
    pub must_ask: bool,
    /// 有子动作处于 ForceAskOnce
    pub forced_ask: bool,
    /// 调用解析不了
    pub unparsable: bool,
    /// 有子动作越界
    pub out_of_bounds: bool,
    /// 有子动作是沙箱外执行
    pub outside_sandbox: bool,
}

impl CallConstraints {
    fn union(self, o: Self) -> Self {
        Self {
            must_ask: self.must_ask || o.must_ask,
            forced_ask: self.forced_ask || o.forced_ask,
            unparsable: self.unparsable || o.unparsable,
            out_of_bounds: self.out_of_bounds || o.out_of_bounds,
            outside_sandbox: self.outside_sandbox || o.outside_sandbox,
        }
    }

    /// 能交 AI 审查：只有越界或沙箱外这两种「问」的原因，且没有必问、强制询问、解析不了。
    pub fn review_eligible(&self) -> bool {
        (self.out_of_bounds || self.outside_sandbox)
            && !self.must_ask
            && !self.forced_ask
            && !self.unparsable
    }

    /// 能记住：没有必问、强制询问、解析不了，且不是清单外重试。
    pub fn remember_eligible(&self, bypass: bool) -> bool {
        !self.must_ask && !self.forced_ask && !self.unparsable && !bypass
    }
}

/// 一条调用的求值结果。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CallEvaluation {
    /// 代表动作的结果；`matched` 合并全部子动作命中的条目
    pub evaluation: Evaluation,
    /// 与 `call.actions` 一一对应
    pub per_action: Vec<Evaluation>,
    /// 最严的那个（并列取靠前）
    pub representative: usize,
    pub constraints: CallConstraints,
    pub fingerprints: BTreeSet<ActionFingerprint>,
}

fn eval(
    verdict: Verdict,
    matched: Vec<RuleId>,
    layer_hit: RuleLayer,
    rules: &RuleSet,
) -> Evaluation {
    Evaluation {
        verdict,
        matched,
        layer_hit,
        ruleset_version: rules.version.clone(),
    }
}

fn layer_entries(
    rules: &RuleSet,
    layer: RuleLayer,
    mode: ApprovalMode,
) -> impl Iterator<Item = &RuleEntry> {
    rules
        .entries
        .iter()
        .filter(move |e| e.layer == layer && e.applies_in_mode(mode))
}

/// 一个动作的七步求值，外加它带给调用的约束。
pub fn evaluate(
    action: &Action,
    shape: &CallShape,
    rules: &RuleSet,
    ctx: &EvalContext,
    fs: &impl PathResolver,
) -> (Evaluation, CallConstraints) {
    let mode = ctx.mode;
    let hit = |e: &&RuleEntry| matches(&e.matcher, e.layer, action, fs);
    let unparsable = matches!(shape, CallShape::Unparsable { .. });
    let mut c = CallConstraints {
        unparsable,
        out_of_bounds: action.flags.out_of_bounds,
        outside_sandbox: action.kind == ActionKind::OutsideSandbox,
        ..Default::default()
    };
    let mut matched = Vec::new();

    // ① 底线：取最具体的命中；只读区的放行只对读生效
    let baseline: Vec<&RuleEntry> = layer_entries(rules, RuleLayer::Baseline, mode)
        .filter(hit)
        .collect();
    if !baseline.is_empty() {
        matched.extend(baseline.iter().map(|e| e.id));
        let readonly_wins = baseline
            .iter()
            .max_by_key(|e| specificity(&e.matcher))
            .is_some_and(|e| e.effect == RuleEffect::Allow)
            && action.kind == ActionKind::Read;
        if !readonly_wins
            && let Some(deny) = baseline
                .iter()
                .filter(|e| e.effect != RuleEffect::Allow)
                .max_by_key(|e| specificity(&e.matcher))
        {
            return (
                eval(
                    Verdict::Deny(DenyReason::Baseline(deny.id)),
                    matched,
                    RuleLayer::Baseline,
                    rules,
                ),
                c,
            );
        }
    }

    // ② 拒绝记忆
    let fp = action.fingerprint();
    match ctx.denial.get(&fp) {
        Some(DenialPhase::AutoDeny) => {
            return (
                eval(
                    Verdict::Deny(DenyReason::DenialMemory(fp)),
                    matched,
                    RuleLayer::DenialMemory,
                    rules,
                ),
                c,
            );
        }
        Some(DenialPhase::ForceAskOnce) => c.forced_ask = true,
        None => {}
    }

    // ③ 工具配置与边界
    let tool = action.tool.0.as_str();
    if let Some(e) = layer_entries(rules, RuleLayer::ToolConfig, mode).find(hit) {
        matched.push(e.id);
        return (
            eval(
                Verdict::Deny(DenyReason::ToolSetExcluded),
                matched,
                RuleLayer::ToolConfig,
                rules,
            ),
            c,
        );
    }
    if registry::group_of(tool).is_some_and(|g| ctx.disabled_groups.contains(&g)) {
        return (
            eval(
                Verdict::Deny(DenyReason::ToolDisabled),
                matched,
                RuleLayer::ToolConfig,
                rules,
            ),
            c,
        );
    }
    if !in_tool_set(tool, ctx.tool_set) {
        return (
            eval(
                Verdict::Deny(DenyReason::ToolSetExcluded),
                matched,
                RuleLayer::ToolConfig,
                rules,
            ),
            c,
        );
    }

    // ④ 拒绝规则（排在必问之前，v1.58）
    if let Some(e) = layer_entries(rules, RuleLayer::UserDeny, mode).find(hit) {
        matched.push(e.id);
        return (
            eval(
                Verdict::Deny(DenyReason::DenyRule(e.id)),
                matched,
                RuleLayer::UserDeny,
                rules,
            ),
            c,
        );
    }

    // ⑤ 必问（不经 AI 审查、只能允许一次）
    if let Some(e) = layer_entries(rules, RuleLayer::MustAsk, mode).find(hit) {
        matched.push(e.id);
        c.must_ask = true;
        return (
            eval(
                Verdict::Ask(AskReason::MustAsk),
                matched,
                RuleLayer::MustAsk,
                rules,
            ),
            c,
        );
    }

    // ⑥ 放行规则：强制询问、解析不了、清单外重试时跳过
    let retry = action.kind == ActionKind::OutsideSandbox && action.flags.bypass_sandbox_requested;
    if !c.forced_ask && !unparsable && !retry {
        for layer in [RuleLayer::Remembered, RuleLayer::ProjectAllow] {
            if let Some(e) = layer_entries(rules, layer, mode).find(hit) {
                matched.push(e.id);
                return (eval(Verdict::Allow, matched, layer, rules), c);
            }
        }
    }

    // ⑦ 默认
    if c.forced_ask {
        let v = if action.kind == ActionKind::OutsideSandbox {
            Verdict::OutsideSandbox(AskReason::ForcedAfterDenial)
        } else {
            Verdict::Ask(AskReason::ForcedAfterDenial)
        };
        return (eval(v, matched, RuleLayer::Default, rules), c);
    }
    let default = layer_entries(rules, RuleLayer::Default, mode).find(hit);
    let verdict = match default {
        Some(e) => {
            matched.push(e.id);
            match e.effect {
                RuleEffect::Allow => Verdict::Allow,
                RuleEffect::Ask => Verdict::Ask(ask_reason(&e.matcher)),
                RuleEffect::OutsideSandbox => Verdict::OutsideSandbox(ask_reason(&e.matcher)),
                RuleEffect::Deny | RuleEffect::UserActionEligible => {
                    Verdict::Deny(DenyReason::DenyRule(e.id))
                }
            }
        }
        None => {
            debug_assert!(
                false,
                "默认层对 {:?} 没有结论（default_layer_total 应能证明不会发生）",
                action.kind
            );
            Verdict::Ask(AskReason::Default)
        }
    };
    // 解析不了：三档都至少询问（Q4），比默认更严的（清单外重试在全开档拒绝）保留
    let verdict =
        if unparsable && strictness(&verdict) <= strictness(&Verdict::Ask(AskReason::Default)) {
            Verdict::Ask(AskReason::UnparsableCommand)
        } else {
            verdict
        };
    (eval(verdict, matched, RuleLayer::Default, rules), c)
}

/// 默认条目的询问原因由它的匹配条件看出：越界 / 清单外重试、MCP、未归类、其余。
fn ask_reason(m: &RuleMatcher) -> AskReason {
    fn any(m: &RuleMatcher, f: &impl Fn(&RuleMatcher) -> bool) -> bool {
        f(m) || matches!(m, RuleMatcher::All(ms) if ms.iter().any(|x| any(x, f)))
    }
    if any(m, &|x| {
        matches!(
            x,
            RuleMatcher::Flag(ActionFlag::OutOfBounds | ActionFlag::BypassSandboxRequested)
        )
    }) {
        AskReason::OutOfBounds
    } else if any(m, &|x| {
        matches!(x, RuleMatcher::ActionKindIs(ActionKind::McpCall))
    }) {
        AskReason::McpNotReadOnly
    } else if any(m, &|x| {
        matches!(x, RuleMatcher::ActionKindIs(ActionKind::Unclassified))
    }) {
        AskReason::Unclassified
    } else {
        AskReason::Default
    }
}

/// 一条调用：逐个子动作求值，取最严，约束取或；再判亲手执行卡片与系统回合。
pub fn evaluate_call(
    call: &NormalizedCall,
    rules: &RuleSet,
    ctx: &EvalContext,
    fs: &impl PathResolver,
) -> CallEvaluation {
    let mut per_action = Vec::with_capacity(call.actions.len());
    let mut constraints = CallConstraints {
        unparsable: matches!(call.shape, CallShape::Unparsable { .. }),
        ..Default::default()
    };
    for a in &call.actions {
        let (e, c) = evaluate(a, &call.shape, rules, ctx, fs);
        constraints = constraints.union(c);
        per_action.push(e);
    }
    let representative = per_action.iter().enumerate().fold(0, |best, (i, e)| {
        if strictness(&e.verdict) > strictness(&per_action[best].verdict) {
            i
        } else {
            best
        }
    });
    let mut evaluation = per_action[representative].clone();
    let mut seen = BTreeSet::new();
    evaluation.matched = per_action
        .iter()
        .flat_map(|e| e.matched.iter().copied())
        .filter(|id| seen.insert(*id))
        .collect();
    let fingerprints = call.fingerprints();

    // 亲手执行卡片（5.4）：底线拒绝、单个简单命令或 Edit、没有子动作在拒绝记忆里、不是系统回合，
    // 且整条调用没有底线以外的硬拒绝（路径查不清、拒绝规则、工具被禁用等三档拒、不出卡片，2/F-6）
    let other_hard_deny = per_action
        .iter()
        .any(|e| matches!(&e.verdict, Verdict::Deny(r) if !matches!(r, DenyReason::Baseline(_))));
    if evaluation.layer_hit == RuleLayer::Baseline
        && !other_hard_deny
        && ctx.turn_kind == TurnKind::Assistant
        && fingerprints.iter().all(|fp| ctx.denial.get(fp).is_none())
        && let Some(p) = proposal::propose_user_action(call)
    {
        evaluation.verdict = Verdict::UserAction(p);
    }

    // 系统回合只有放行与拒绝（技术设计 7.2b）
    if ctx.turn_kind != TurnKind::Assistant
        && matches!(
            evaluation.verdict,
            Verdict::Ask(_) | Verdict::OutsideSandbox(_) | Verdict::UserAction(_)
        )
    {
        evaluation.verdict = Verdict::Deny(DenyReason::SystemTurnOutOfToolSet);
        evaluation.layer_hit = RuleLayer::ToolConfig;
    }

    CallEvaluation {
        evaluation,
        per_action,
        representative,
        constraints,
        fingerprints,
    }
}
