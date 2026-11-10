//! 叠加与求值顺序（T07 验收 1、1a、1b 的求值部分；方案 5.2、5.3、6.4）。

mod support;

use iwork_core::approval::denial::{DenialEvent, DenialMemory};
use iwork_core::approval::evaluate::remember::offered_remember;
use iwork_domain::action::ActionKind;
use iwork_domain::approval_mode::{ApprovalMode, RememberScope};
use iwork_domain::rule::{AskReason, DenyReason, RuleLayer, RuleMatcher, RuleScope, Verdict};
use support::*;

use ApprovalMode::{Auto, FullAccess, Manual};

fn project_scope() -> RuleScope {
    RuleScope::Project(project())
}

/// 1a：手动档下「本项目允许写 src/」使写 src/a.rs 直接放行；读同一目录的规则不放行写（F-14）。
#[test]
fn remembered_allow_write_src() {
    let mut h = Harness::default();
    assert_eq!(
        v(&h.verdict(&write_req(&format!("{WS}/src/a.rs")), Manual)),
        "Ask"
    );
    h.live.push(live_rule(
        1,
        RuleLayer::Remembered,
        kind_and(
            ActionKind::Write,
            RuleMatcher::PathUnder(format!("{WS}/src").into()),
        ),
        project_scope(),
    ));
    assert_eq!(
        h.verdict(&write_req(&format!("{WS}/src/a.rs")), Manual),
        Verdict::Allow
    );
    assert_eq!(
        h.verdict(&bash_req("echo x > src/b.rs"), Manual).clone(),
        Verdict::Ask(AskReason::Default),
        "echo 本身仍是执行"
    );
    // 他项目的同一规则不生效
    let mut h2 = Harness::default();
    h2.live.push(live_rule(
        1,
        RuleLayer::Remembered,
        kind_and(
            ActionKind::Write,
            RuleMatcher::PathUnder(format!("{WS}/src").into()),
        ),
        RuleScope::Project(other_project()),
    ));
    assert_eq!(
        v(&h2.verdict(&write_req(&format!("{WS}/src/a.rs")), Manual)),
        "Ask"
    );
}

#[test]
fn remembered_read_does_not_allow_write() {
    let mut h = Harness::default();
    h.live.push(live_rule(
        1,
        RuleLayer::Remembered,
        kind_and(
            ActionKind::Read,
            RuleMatcher::PathUnder("/home/u/Documents/notes".into()),
        ),
        RuleScope::Global,
    ));
    assert_eq!(
        h.verdict(&read_req("/home/u/Documents/notes/x.txt"), Manual),
        Verdict::Allow
    );
    assert_eq!(
        h.verdict(&write_req("/home/u/Documents/notes/x.txt"), Manual),
        Verdict::Ask(AskReason::OutOfBounds)
    );
}

#[test]
fn remembered_execute_does_not_allow_outside_sandbox() {
    let mut h = Harness::default();
    h.live.push(live_rule(
        1,
        RuleLayer::Remembered,
        kind_and(
            ActionKind::Execute,
            RuleMatcher::ProgramSubcommand {
                program: "git".into(),
                subcommand: Some("pull".into()),
            },
        ),
        project_scope(),
    ));
    // git pull 在需凭据清单里：是沙箱外执行，普通执行的批准不放行它
    assert!(matches!(
        h.verdict(&bash_req("git pull"), Manual),
        Verdict::OutsideSandbox(_)
    ));
    // 对照：沙箱外执行的记住规则放行
    h.live.push(live_rule(
        2,
        RuleLayer::Remembered,
        kind_and(
            ActionKind::OutsideSandbox,
            RuleMatcher::ProgramSubcommand {
                program: "git".into(),
                subcommand: Some("pull".into()),
            },
        ),
        project_scope(),
    ));
    assert_eq!(h.verdict(&bash_req("git pull"), Manual), Verdict::Allow);
}

/// 必问动作即使存在放行规则也询问。
#[test]
fn must_ask_despite_allow_rule() {
    let mut h = Harness::default();
    h.live.push(live_rule(
        1,
        RuleLayer::Remembered,
        RuleMatcher::Any,
        RuleScope::Global,
    ));
    h.live.push(live_rule(
        2,
        RuleLayer::ProjectAllow,
        RuleMatcher::Any,
        project_scope(),
    ));
    for m in MODES {
        assert_eq!(
            h.verdict(&bash_req("git push"), m),
            Verdict::Ask(AskReason::MustAsk),
            "{m:?}"
        );
    }
    for m in [Manual, Auto] {
        assert_eq!(
            h.verdict(&bash_req("rm src/a.rs"), m),
            Verdict::Ask(AskReason::MustAsk)
        );
        assert_eq!(
            h.verdict(&bash_req("npm publish"), m),
            Verdict::Ask(AskReason::MustAsk)
        );
    }
    // 对照：普通执行被放行规则放行
    assert_eq!(h.verdict(&bash_req("npm test"), Manual), Verdict::Allow);
}

/// 1a：读敏感目录手动询问、自动进 AI 审查（越界、可审查）；读其他工作区外文件放行。
#[test]
fn sensitive_read_manual_ask_auto_review_other_read_allow() {
    let h = Harness::default();
    let (_, e) = h.eval(&read_req("/home/u/Documents/notes/x.txt"), Manual);
    assert_eq!(e.evaluation.verdict, Verdict::Ask(AskReason::OutOfBounds));
    let (_, e) = h.eval(&read_req("/home/u/Documents/notes/x.txt"), Auto);
    assert_eq!(e.evaluation.verdict, Verdict::Ask(AskReason::OutOfBounds));
    assert!(e.constraints.review_eligible());
    assert_eq!(
        h.verdict(&read_req("/home/u/Documents/notes/x.txt"), FullAccess),
        Verdict::Allow
    );
    for m in MODES {
        assert_eq!(
            h.verdict(&read_req("/usr/include/stdio.h"), m),
            Verdict::Allow
        );
    }
}

/// 1a（v1.64）：只读区读放写拒，artifacts 与数据目录其余部分读写都拒。
#[test]
fn data_dir_zones() {
    let h = Harness::default();
    let zone = |p: &str| format!("{DATA}/{p}");
    for m in MODES {
        assert_eq!(
            h.verdict(&read_req(&zone("attachments/a.png")), m),
            Verdict::Allow,
            "{m:?}"
        );
        assert_eq!(
            h.verdict(&read_req(&zone("imports/skill.md")), m),
            Verdict::Allow
        );
        for req in [
            write_req(&zone("attachments/a.png")),
            write_req(&zone("imports/new.md")),
            read_req(&zone("artifacts/log.txt")),
            write_req(&zone("artifacts/log.txt")),
            read_req(&zone("iwork.db")),
            write_req(&zone("iwork.db")),
        ] {
            assert!(
                matches!(h.verdict(&req, m), Verdict::Deny(DenyReason::Baseline(_))),
                "{m:?} {:?}",
                req.raw_input
            );
        }
    }
    // 经 .. 从只读区出去也拒
    assert!(matches!(
        h.verdict(&read_req(&zone("attachments/../iwork.db")), Manual),
        Verdict::Deny(DenyReason::Baseline(_))
    ));
}

/// F-1：自动档下 `git pull && git push` 不交 AI 审查、只能允许一次。
#[test]
fn git_pull_and_push_not_reviewed_in_auto() {
    let h = Harness::default();
    let (call, e) = h.eval(&bash_req("git pull && git push"), Auto);
    assert!(e.constraints.must_ask && e.constraints.outside_sandbox);
    assert!(!e.constraints.review_eligible());
    assert_eq!(
        offered_remember(&call, &e, &scope()).0,
        vec![RememberScope::Once]
    );
    // 对照：单独的 git pull 可审查、可记到本项目
    let (call, e) = h.eval(&bash_req("git pull"), Auto);
    assert!(e.constraints.review_eligible());
    assert_eq!(
        offered_remember(&call, &e, &scope()).0,
        vec![RememberScope::Once, RememberScope::Project]
    );
}

/// 1b：存在「git push 一律拒绝」的项目规则时，git push 是拒绝而不是询问（v1.58）。
#[test]
fn deny_rule_git_push_denies_not_asks() {
    let mut h = Harness::default();
    let rule = live_rule(
        1,
        RuleLayer::UserDeny,
        RuleMatcher::ActionKindIs(ActionKind::Push),
        project_scope(),
    );
    let id = rule.id;
    h.live.push(rule);
    for m in MODES {
        assert_eq!(
            h.verdict(&bash_req("git push"), m),
            Verdict::Deny(DenyReason::DenyRule(id)),
            "{m:?}"
        );
    }
}

/// 1b：自动档下被拒的边界内动作，用户发出下一条消息后第一次请求询问一次；记住的放行规则不能跳过；
/// 允许后同一动作恢复正常求值；全开档同样询问这一次。
#[test]
fn forced_ask_once_then_allow_then_normal() {
    let mut h = Harness::default();
    h.live.push(live_rule(
        1,
        RuleLayer::Remembered,
        RuleMatcher::Any,
        RuleScope::Global,
    ));
    let req = bash_req("npm test");
    let (call, _) = h.eval(&req, Auto);
    h.denial = DenialMemory::replay(&[DenialEvent::UserDenied(call.fingerprints())]);
    assert!(matches!(
        h.verdict(&req, Auto),
        Verdict::Deny(DenyReason::DenialMemory(_))
    ));
    h.denial.apply(&DenialEvent::UserMessage);
    for m in MODES {
        assert_eq!(
            h.verdict(&req, m),
            Verdict::Ask(AskReason::ForcedAfterDenial),
            "{m:?}"
        );
    }
    let (call, e) = h.eval(&req, Auto);
    assert!(e.constraints.forced_ask && !e.constraints.review_eligible());
    assert_eq!(
        offered_remember(&call, &e, &scope()).0,
        vec![RememberScope::Once]
    );
    h.denial
        .apply(&DenialEvent::UserAllowed(call.fingerprints()));
    assert_eq!(h.verdict(&req, Auto), Verdict::Allow);
}

/// 用户的拒绝规则命中时恒为拒绝，即使同时是必问（验收 1）。
#[test]
fn deny_rule_beats_must_ask_example() {
    let mut h = Harness::default();
    h.live.push(live_rule(
        1,
        RuleLayer::UserDeny,
        RuleMatcher::ActionKindIs(ActionKind::Delete),
        RuleScope::Global,
    ));
    assert!(matches!(
        h.verdict(&bash_req("rm src/a.rs"), Manual),
        Verdict::Deny(DenyReason::DenyRule(_))
    ));
}

/// 越界动作可以被放行规则放行（技术设计 7.2 第 6 步）。
#[test]
fn allow_rule_can_allow_out_of_bounds() {
    let mut h = Harness::default();
    h.live.push(live_rule(
        1,
        RuleLayer::Remembered,
        kind_and(ActionKind::Write, RuleMatcher::PathUnder("/opt/out".into())),
        RuleScope::Global,
    ));
    assert_eq!(h.verdict(&write_req("/opt/out/x"), Auto), Verdict::Allow);
}

/// 记住范围：执行按程序名 + 子命令；写按目录；未归类按工具名；必问只能一次。
#[test]
fn offered_remember_scopes() {
    let h = Harness::default();
    let (call, e) = h.eval(&write_req(&format!("{WS}/src/a.rs")), Manual);
    let (scopes, label) = offered_remember(&call, &e, &scope());
    assert_eq!(
        scopes,
        vec![
            RememberScope::Once,
            RememberScope::Conversation,
            RememberScope::Project,
            RememberScope::Forever
        ]
    );
    assert!(label.contains("src/"), "{label}");
    let (call, e) = h.eval(&tool_req("SomeNewTool", serde_json::json!({})), Manual);
    assert_eq!(e.evaluation.verdict, Verdict::Ask(AskReason::Unclassified));
    let (scopes, label) = offered_remember(&call, &e, &scope());
    assert_eq!(scopes.len(), 4);
    assert!(label.contains("SomeNewTool"));
    let (call, e) = h.eval(&bash_req("git push"), Manual);
    assert_eq!(
        offered_remember(&call, &e, &scope()).0,
        vec![RememberScope::Once]
    );
    // 私聊选「私聊」：没有「本项目」
    let mut dm = scope();
    dm.project = None;
    let (call, e) = h.eval(&write_req(&format!("{WS}/src/a.rs")), Manual);
    assert!(
        !offered_remember(&call, &e, &dm)
            .0
            .contains(&RememberScope::Project)
    );
}

/// ArgumentPath（supervisor 2026-11-10 定 (a)）：角色未知的参数路径在工作区内不单独再问；
/// 越界、底线、解析不了、重定向都不受影响。
#[test]
fn argument_path_does_not_add_a_second_ask() {
    use iwork_core::approval::evaluate::remember::remember_rule;
    use iwork_domain::approval::{Approval, ApprovalKind, ApprovalState, ReviewState};
    use iwork_domain::common::{Actor, Timestamp};
    use iwork_domain::id::{ApprovalId, RawUlid, RuleId, TurnId};
    let h = Harness::default();
    // 手动档 npm test foo：只有执行动作要问，可按「npm test」记住
    let (call, e) = h.eval(&bash_req("npm test foo"), Manual);
    let asked: Vec<_> = e
        .per_action
        .iter()
        .filter(|x| x.verdict != Verdict::Allow)
        .collect();
    assert_eq!(asked.len(), 1, "{:?}", briefs(&call));
    assert!(
        call.actions
            .iter()
            .any(|a| a.kind == ActionKind::Write && a.flags.argument_path)
    );
    let (scopes, label) = offered_remember(&call, &e, &scope());
    assert!(scopes.contains(&RememberScope::Project), "{scopes:?}");
    assert!(label.contains("npm test"), "{label}");
    // 记住到本项目
    let approval = Approval {
        id: ApprovalId::from_raw(RawUlid::from_u128(1)),
        turn_id: TurnId::from_raw(RawUlid::from_u128(2)),
        conversation_id: conversation(),
        kind: ApprovalKind::Ask,
        action: call.actions[e.representative].clone(),
        call: Some(Box::new(call.record(e.representative))),
        matched_rules: vec![],
        ruleset_version: e.evaluation.ruleset_version.clone(),
        state: ApprovalState::Pending,
        review: ReviewState::NotReviewed,
        offered_remember: scopes,
        remember_label: label,
        requested_at: Timestamp(0),
    };
    let rule = remember_rule(
        &approval,
        RememberScope::Project,
        &scope(),
        Actor("u".into()),
        Timestamp(1),
        RuleId::from_raw(RawUlid::from_u128(3)),
    )
    .unwrap();
    let mut h = Harness::default();
    h.live.push(rule);
    assert_eq!(h.verdict(&bash_req("npm test foo"), Manual), Verdict::Allow);
    assert_eq!(
        h.verdict(&bash_req("npm test bar --watch"), Manual),
        Verdict::Allow,
        "带任何参数都算"
    );
    // 记住后越界的参数路径仍询问
    assert_eq!(
        h.verdict(&bash_req("npm test ../outside"), Manual),
        Verdict::Ask(AskReason::OutOfBounds)
    );
    // 底线照旧
    for m in MODES {
        assert!(
            matches!(
                h.verdict(&bash_req("npm test .git/x"), m),
                Verdict::Deny(DenyReason::Baseline(_)) | Verdict::UserAction(_)
            ),
            "{m:?}"
        );
    }
    // 解析不了的调用里同样的路径：仍是解析不了的询问（兜底候选不打标记）
    let (call, e) = h.eval(&bash_req("npm test foo $X"), Manual);
    assert!(call.actions.iter().all(|a| !a.flags.argument_path));
    assert_eq!(
        e.evaluation.verdict,
        Verdict::Ask(AskReason::UnparsableCommand)
    );
    // 重定向目标不打标记：手动档仍按写询问
    let h = Harness::default();
    let (call, e) = h.eval(&bash_req("npm test > out.log"), Manual);
    let w = call
        .actions
        .iter()
        .position(|a| a.kind == ActionKind::Write && !a.flags.argument_path)
        .unwrap();
    assert_eq!(e.per_action[w].verdict, Verdict::Ask(AskReason::Default));
    // 白名单的精确角色不打：cp 的目的地手动档照常询问
    let (call, _) = h.eval(&bash_req("cp src/a.rs dst.rs"), Manual);
    assert!(call.actions.iter().all(|a| !a.flags.argument_path));
    assert_eq!(
        v(&h.verdict(&bash_req("cp src/a.rs dst.rs"), Manual)),
        "Ask"
    );
    // git 组合短选项的兜底候选不打
    let (call, _) = h.eval(&bash_req("git clean -fd"), Manual);
    assert!(
        call.actions.iter().all(|a| !a.flags.argument_path),
        "{:?}",
        briefs(&call)
    );
}
