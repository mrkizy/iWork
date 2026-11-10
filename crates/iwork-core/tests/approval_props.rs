//! 求值的属性测试（T07 验收 1；方案 11.1、11.2）：随机调用 × 随机规则集 × 随机档位与上下文。

mod support;

use iwork_core::approval::denial::{DenialEvent, DenialMemory, DenialPhase};
use iwork_core::approval::evaluate::strictness::strictness;
use iwork_core::approval::rules::matcher::matches;
use iwork_core::approval::rules::{build_ruleset, builtin::builtin_rules};
use iwork_domain::action::{ActionKind, CallShapeKind};
use iwork_domain::approval_mode::ApprovalMode;
use iwork_domain::rule::{
    AskReason, DenyReason, RuleEffect, RuleLayer, RuleMatcher, RuleScope, Verdict,
};
use iwork_domain::turn::TurnKind;
use iwork_ports::agent::PermissionRequest;
use proptest::prelude::*;
use support::*;

/// 九类路径（凭据、数据目录、只读区、工作区、.git、敏感、缓存、系统、其他），再随机套链接、..、大小写。
fn arb_path() -> impl Strategy<Value = String> {
    let base = prop::sample::select(vec![
        "/home/u/.ssh/id_rsa",
        "/home/u/.aws/credentials",
        "/home/u/.netrc",
        "/home/u/Library/Application Support/iWork/iwork.db",
        "/home/u/Library/Application Support/iWork/attachments/a.png",
        "/home/u/Library/Application Support/iWork/artifacts/log.txt",
        "/home/u/iWork/projects/proj/src/a.rs",
        "/home/u/iWork/projects/proj/new.txt",
        "/home/u/iWork/projects/proj/.git/config",
        "/home/u/iWork/projects/proj/.claude/settings.json",
        "/home/u/Documents/notes/x.txt",
        "/home/u/.npm/x",
        "/usr/include/stdio.h",
        "/opt/x",
        "/tmp/x",
        "/home/u/.zshrc",
    ]);
    let decorate = prop::sample::select(vec![0u8, 1, 2, 3]);
    (base, decorate).prop_map(|(p, d)| match d {
        // 经 .. 从工作区回到同一路径
        1 if p.starts_with("/home/u/") => format!("{WS}/../../../{}", &p["/home/u/".len()..]),
        // 经链接：keys → ~/.ssh，objs → .git/objects
        2 if p.starts_with("/home/u/.ssh/") => format!("keys/{}", &p["/home/u/.ssh/".len()..]),
        2 if p.ends_with(".git/config") => "objs/../config".into(),
        _ => p.to_string(),
    })
}

fn arb_req() -> impl Strategy<Value = PermissionRequest> {
    let path_cmd = (
        arb_path(),
        prop::sample::select(vec![
            "cat {}",
            "echo x > {}",
            "rm {}",
            "cp src/a.rs {}",
            "cp {} dst",
            "grep x {}",
            "touch {}",
            "rsync {} ./c",
            "find {} -delete",
        ]),
    )
        .prop_map(|(p, t)| t.replace("{}", &format!("'{p}'")));
    let plain = prop::sample::select(vec![
        "git push".to_string(),
        "git pull".into(),
        "npm test".into(),
        "npm publish".into(),
        "ls".into(),
        "echo $X".into(),
        "git reset --hard".into(),
        "./build.sh".into(),
        "eval x".into(),
    ]);
    let piece = prop_oneof![path_cmd, plain];
    let bash = prop::collection::vec(piece, 1..4).prop_map(|parts| bash_req(&parts.join(" && ")));
    let tool = (
        prop::sample::select(vec!["Read", "Write", "Edit"]),
        arb_path(),
    )
        .prop_map(|(t, p)| match t {
            "Read" => read_req(&p),
            "Write" => write_req(&p),
            _ => edit_req(&p),
        });
    let other = prop::sample::select(vec![
        "mcp__s__t",
        "Agent",
        "CronCreate",
        "NewTool",
        "WebSearch",
    ])
    .prop_map(|t| tool_req(t, serde_json::json!({})));
    prop_oneof![4 => bash, 3 => tool, 1 => other].prop_map(|mut r| {
        r.bypass_sandbox = false;
        r
    })
}

fn arb_matcher() -> impl Strategy<Value = RuleMatcher> {
    let kind = prop::sample::select(vec![
        ActionKind::Read,
        ActionKind::Write,
        ActionKind::Execute,
        ActionKind::OutsideSandbox,
        ActionKind::Delete,
        ActionKind::Push,
        ActionKind::Publish,
        ActionKind::WriteAutorunConfig,
        ActionKind::McpCall,
        ActionKind::Unclassified,
    ]);
    let dir = prop::sample::select(vec![
        "/home/u",
        "/home/u/.ssh",
        WS,
        "/home/u/Documents",
        "/opt",
        "/",
    ]);
    prop_oneof![
        Just(RuleMatcher::Any),
        kind.clone().prop_map(RuleMatcher::ActionKindIs),
        (kind.clone(), dir).prop_map(|(k, d)| kind_and(k, RuleMatcher::PathUnder(d.into()))),
        (kind, prop::sample::select(vec!["git", "npm", "cat", "rm"])).prop_map(|(k, p)| kind_and(
            k,
            RuleMatcher::ProgramSubcommand {
                program: p.into(),
                subcommand: None
            }
        )),
    ]
}

fn arb_live() -> impl Strategy<Value = Vec<iwork_domain::rule::RuleEntry>> {
    let one = (
        prop::sample::select(vec![
            RuleLayer::Remembered,
            RuleLayer::ProjectAllow,
            RuleLayer::UserDeny,
        ]),
        arb_matcher(),
        prop::sample::select(vec![
            RuleScope::Global,
            RuleScope::Project(project()),
            RuleScope::Project(other_project()),
        ]),
    );
    prop::collection::vec(one, 0..6).prop_map(|v| {
        v.into_iter()
            .enumerate()
            .map(|(i, (l, m, s))| live_rule(i as u128, l, m, s))
            .collect()
    })
}

fn arb_mode() -> impl Strategy<Value = ApprovalMode> {
    prop::sample::select(MODES.to_vec())
}

/// 随机一份拒绝记忆：从调用自己的指纹里挑一部分（否则几乎碰不上）。
fn harness(live: Vec<iwork_domain::rule::RuleEntry>, turn: TurnKind) -> Harness {
    Harness {
        live,
        turn,
        ..Harness::default()
    }
}

fn effect_strictness(e: RuleEffect) -> u8 {
    match e {
        RuleEffect::Allow => 0,
        RuleEffect::Ask => 1,
        RuleEffect::OutsideSandbox => 2,
        RuleEffect::UserActionEligible => 3,
        RuleEffect::Deny => 4,
    }
}

proptest! {
    #![proptest_config(ProptestConfig { cases: 256, ..ProptestConfig::default() })]

    /// 最终结果永不比步骤 1–5 任一层的命中更宽；底线命中恒为拒绝（或亲手执行）；必问在其档位内不放行；
    /// 用户拒绝规则命中恒为拒绝，即使同时是必问（验收 1）。
    #[test]
    fn never_wider_than_any_layer(req in arb_req(), live in arb_live(), mode in arb_mode()) {
        let h = harness(live, TurnKind::Assistant);
        let (call, e) = h.eval(&req, mode);
        let rules = build_ruleset(&h.snap.rules, &h.live, &scope()).unwrap();
        for (a, ev) in call.actions.iter().zip(&e.per_action) {
            let got = strictness(&ev.verdict);
            let hits = |layer: RuleLayer| rules.entries.iter()
                .filter(move |r| r.layer == layer && r.applies_in_mode(mode))
                .filter(|r| matches(&r.matcher, r.layer, a, &h.fs));
            // 底线（只读区里的读除外）
            let readonly_read = a.kind == ActionKind::Read && hits(RuleLayer::Baseline).any(|r| r.effect == RuleEffect::Allow);
            if !readonly_read && hits(RuleLayer::Baseline).any(|r| r.effect != RuleEffect::Allow) {
                prop_assert!(matches!(ev.verdict, Verdict::Deny(DenyReason::Baseline(_))), "{a:?} → {:?}", ev.verdict);
                prop_assert!(matches!(e.evaluation.verdict, Verdict::Deny(_) | Verdict::UserAction(_)));
            }
            for layer in [RuleLayer::UserDeny, RuleLayer::MustAsk] {
                for r in hits(layer) {
                    prop_assert!(got >= effect_strictness(r.effect), "{a:?} 命中 {} 却得到 {:?}", r.label, ev.verdict);
                }
            }
            if hits(RuleLayer::UserDeny).next().is_some() && !matches!(ev.verdict, Verdict::Deny(DenyReason::Baseline(_) | DenyReason::ToolSetExcluded | DenyReason::ToolDisabled)) {
                prop_assert!(matches!(ev.verdict, Verdict::Deny(DenyReason::DenyRule(_))), "{a:?}：{:?}", ev.verdict);
            }
            if hits(RuleLayer::MustAsk).next().is_some() {
                prop_assert_ne!(&ev.verdict, &Verdict::Allow);
            }
        }
        // 调用结果 = 子动作最严；亲手执行是底线拒绝的卡片形态，只出现在代表动作底线拒绝时
        let max = e.per_action.iter().map(|x| strictness(&x.verdict)).max().unwrap();
        match &e.evaluation.verdict {
            Verdict::UserAction(_) => prop_assert!(matches!(e.per_action[e.representative].verdict, Verdict::Deny(DenyReason::Baseline(_)))),
            v => prop_assert_eq!(strictness(v), max),
        }
    }

    /// 参数标记只出现在归类成功的调用里、执行 / 发布的参数路径上（读、写，路径目标）；解析不了的调用里一个都没有。
    #[test]
    fn argument_path_only_on_external_program_arguments(req in arb_req()) {
        let c = norm(&req);
        let tagged: Vec<_> = c.actions.iter().filter(|a| a.flags.argument_path).collect();
        if c.shape.kind() == CallShapeKind::Unparsable || req.tool.0 != "Bash" {
            prop_assert!(tagged.is_empty(), "{:?}", c.actions);
        }
        for a in &tagged {
            prop_assert!(matches!(a.kind, ActionKind::Read | ActionKind::Write | ActionKind::WriteAutorunConfig), "{a:?}");
            prop_assert!(matches!(a.target, iwork_domain::action::ActionTarget::Path(_)));
        }
        if !tagged.is_empty() {
            prop_assert!(c.actions.iter().any(|a| matches!(a.kind, ActionKind::Execute | ActionKind::OutsideSandbox | ActionKind::Publish | ActionKind::Push | ActionKind::Delete)));
        }
    }

    /// 推送三档都不放行（除非被拒绝层接走）；解析不了三档都不放行，与放行规则无关。
    #[test]
    fn push_and_unparsable_never_allow(live in arb_live(), mode in arb_mode(), extra in prop::sample::select(vec!["", " && ls", " && echo $X"])) {
        let h = harness(live, TurnKind::Assistant);
        prop_assert_ne!(h.verdict(&bash_req(&format!("git push{extra}")), mode), Verdict::Allow);
        let (call, e) = h.eval(&bash_req(&format!("echo $Y{extra}")), mode);
        prop_assert_eq!(call.shape.kind(), CallShapeKind::Unparsable);
        prop_assert_ne!(e.evaluation.verdict, Verdict::Allow);
    }

    /// 任一子动作 AutoDeny ⇒ 整条拒绝；任一子动作 ForceAskOnce ⇒ 询问类且三档相同，即使有放行规则。
    #[test]
    fn denial_auto_deny_and_forced_ask(req in arb_req(), pick in any::<prop::sample::Index>()) {
        let mut h = harness(vec![live_rule(1, RuleLayer::Remembered, RuleMatcher::Any, RuleScope::Global)], TurnKind::Assistant);
        let (call, _) = h.eval(&req, ApprovalMode::Manual);
        let fps: Vec<_> = call.fingerprints().into_iter().collect();
        let fp = fps[pick.index(fps.len())].clone();
        h.denial = DenialMemory::replay(&[DenialEvent::UserDenied([fp.clone()].into())]);
        let results: Vec<_> = MODES.iter().map(|m| h.eval(&req, *m)).collect();
        for (_, e) in &results {
            let at = call.actions.iter().position(|a| a.fingerprint() == fp).unwrap();
            let sub = &e.per_action[at].verdict;
            prop_assert!(matches!(sub, Verdict::Deny(DenyReason::DenialMemory(_)) | Verdict::Deny(DenyReason::Baseline(_)) | Verdict::Deny(DenyReason::ToolSetExcluded)), "{sub:?}");
            prop_assert!(matches!(e.evaluation.verdict, Verdict::Deny(_)));
        }
        h.denial.apply(&DenialEvent::UserMessage);
        prop_assert_eq!(h.denial.get(&fp), Some(DenialPhase::ForceAskOnce));
        let verdicts: Vec<_> = MODES.iter().map(|m| h.eval(&req, *m).1).collect();
        for e in &verdicts {
            prop_assert_ne!(&e.evaluation.verdict, &Verdict::Allow);
            if !matches!(e.evaluation.verdict, Verdict::Deny(_) | Verdict::UserAction(_)) {
                prop_assert!(e.constraints.forced_ask);
                prop_assert!(!e.constraints.review_eligible());
                prop_assert!(matches!(e.evaluation.verdict, Verdict::Ask(_) | Verdict::OutsideSandbox(_)));
            }
        }
        // 三档同类（原因可能是必问或强制询问）
        prop_assert!(verdicts.windows(2).all(|w| v(&w[0].evaluation.verdict) == v(&w[1].evaluation.verdict)));
    }

    /// 档位单调：手动 ≥ 自动 ≥ 全开；例外只有清单外重试（全开拒绝）。
    #[test]
    fn mode_monotonic(req in arb_req(), live in arb_live()) {
        let h = harness(live, TurnKind::Assistant);
        let s: Vec<u8> = MODES.iter().map(|m| strictness(&h.verdict(&req, *m))).collect();
        prop_assert!(s[0] >= s[1], "{:?}", s);
        prop_assert!(s[1] >= s[2], "{:?}", s);
    }

    /// 系统回合里只有放行与拒绝。
    #[test]
    fn system_turn_only_allow_or_deny(req in arb_req(), live in arb_live(), mode in arb_mode(),
        turn in prop::sample::select(vec![TurnKind::SystemAiReview, TurnKind::SystemAgentsMdDraft])) {
        let h = harness(live, turn);
        let v = h.verdict(&req, mode);
        prop_assert!(matches!(v, Verdict::Allow | Verdict::Deny(_)), "{v:?}");
    }

    /// 调用约束是子动作约束的或；有必问、强制、解析不了时不可审查。
    #[test]
    fn call_constraints_are_union(req in arb_req(), mode in arb_mode()) {
        let h = Harness::default();
        let (call, e) = h.eval(&req, mode);
        let any_oob = call.actions.iter().any(|a| a.flags.out_of_bounds) && e.per_action.iter().any(|_| true);
        let any_must = e.per_action.iter().any(|x| x.layer_hit == RuleLayer::MustAsk);
        prop_assert_eq!(e.constraints.must_ask, any_must);
        prop_assert_eq!(e.constraints.unparsable, call.shape.kind() == CallShapeKind::Unparsable);
        if any_oob && !e.per_action.iter().any(|x| matches!(x.verdict, Verdict::Deny(_))) {
            prop_assert!(e.constraints.out_of_bounds);
        }
        if e.constraints.must_ask || e.constraints.forced_ask || e.constraints.unparsable {
            prop_assert!(!e.constraints.review_eligible());
        }
    }

    /// 记住的规则带动作种类：读的规则不放行写，执行的不放行沙箱外执行。
    #[test]
    fn remembered_rule_requires_kind(p in arb_path()) {
        let mut h = Harness::default();
        h.live.push(live_rule(1, RuleLayer::Remembered, kind_and(ActionKind::Read, RuleMatcher::PathUnder("/".into())), RuleScope::Global));
        let wv = h.verdict(&write_req(&p), ApprovalMode::Manual);
        let without = Harness::default().verdict(&write_req(&p), ApprovalMode::Manual);
        prop_assert_eq!(wv, without);
    }

    /// 任意字符串当命令、当工具输入都不 panic。
    #[test]
    fn normalize_never_panics(cmd in ".{0,120}", raw in ".{0,80}") {
        let _ = norm(&bash_req(&cmd));
        let _ = norm(&tool_req("Read", serde_json::json!({ "file_path": cmd })));
        let mut r = bash_req("x");
        r.raw_input = iwork_ports::agent::RawToolInput(raw);
        let _ = norm(&r);
    }

    /// 含展开 / 通配 / cd / 前置赋值 / 复合重定向的脚本一定解析不了。
    #[test]
    fn safe_form_requires_all_literal(pre in prop::sample::select(vec!["ls", "cat a", "git status"]),
        bad in prop::sample::select(vec!["echo $X", "ls *.rs", "cd x", "A=1 ls", "{ ls; } > o", "echo `x`", "for i in a; do :; done"])) {
        let c = norm(&bash_req(&format!("{pre} && {bad}")));
        prop_assert_eq!(c.shape.kind(), CallShapeKind::Unparsable);
    }

    /// 拒绝记忆：重放与在线一致。
    #[test]
    fn denial_memory_replay_equals_online(ops in prop::collection::vec((0u8..3, prop::collection::btree_set(0u8..6, 0..3)), 0..12)) {
        let fp = |n: u8| iwork_domain::action::ActionFingerprint::new(ActionKind::Write, iwork_domain::action::ActionTarget::Path(cp(&format!("/x/{n}"))));
        let events: Vec<DenialEvent> = ops.iter().map(|(k, s)| {
            let set = s.iter().map(|n| fp(*n)).collect();
            match k { 0 => DenialEvent::UserDenied(set), 1 => DenialEvent::UserMessage, _ => DenialEvent::UserAllowed(set) }
        }).collect();
        let mut online = DenialMemory::default();
        for e in &events {
            online.apply(e);
        }
        prop_assert_eq!(online, DenialMemory::replay(&events));
    }
}

/// 默认层对每种动作 × 标记 × 档位都有结论（求值第 7 步的 debug_assert 由它证明不会触发）。
#[test]
fn default_layer_total() {
    use iwork_domain::action::{Action, ActionFlags, ActionTarget, ToolRef};
    let rules = builtin_rules(&env());
    let fs = fs();
    let kinds = [
        ActionKind::Read,
        ActionKind::Write,
        ActionKind::Execute,
        ActionKind::OutsideSandbox,
        ActionKind::WriteAutorunConfig,
        ActionKind::Delete,
        ActionKind::Push,
        ActionKind::Publish,
        ActionKind::Network,
        ActionKind::McpCall,
        ActionKind::Unclassified,
    ];
    let mut checked = 0;
    for kind in kinds {
        for bits in 0..16u8 {
            for mode in MODES {
                let a = Action {
                    kind,
                    target: ActionTarget::None,
                    tool: ToolRef("X".into()),
                    raw_summary: String::new(),
                    flags: ActionFlags {
                        out_of_bounds: bits & 1 != 0,
                        bypass_sandbox_requested: bits & 2 != 0,
                        opaque_script: bits & 4 != 0,
                        argument_path: bits & 8 != 0,
                    },
                };
                let hit = rules
                    .iter()
                    .filter(|r| r.layer == RuleLayer::Default && r.applies_in_mode(mode))
                    .any(|r| matches(&r.matcher, r.layer, &a, &fs));
                assert!(hit, "{kind:?} {bits} {mode:?}");
                checked += 1;
            }
        }
    }
    assert_eq!(checked, 11 * 16 * 3);
    // default.push 不可达：必问层先接走
    for m in MODES {
        assert_eq!(
            bash_verdict("git push", m),
            Verdict::Ask(AskReason::MustAsk)
        );
    }
}

/// 同一路径经 Edit 与经重定向归一后指纹相等；同一命令不同引号写法指纹相等。
#[test]
fn fingerprint_tool_independent() {
    let a = norm(&write_req(&format!("{WS}/a.txt")));
    let b = norm(&bash_req("printf x > a.txt"));
    let fa = a.actions[0].fingerprint();
    assert!(b.fingerprints().contains(&fa));
    let c1 = norm(&bash_req("npm test --x"));
    let c2 = norm(&bash_req("'npm' \"test\" --x"));
    assert_eq!(c1.fingerprints(), c2.fingerprints());
    // 对照：参数不同指纹不同
    let c3 = norm(&bash_req("npm test --y"));
    assert_ne!(c1.fingerprints(), c3.fingerprints());
}
