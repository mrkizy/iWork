//! 审批求值热路径（T07 方案第 10 节）：取事实 + 归一 + 求值，每次工具调用都要过一遍。
//! 三组：① 一条 `git status` × 内置规则集；② 20 段管道 + 10 个路径词；③ 内置 + 500 条记住的规则。
//! 预算（方案）：P95 ≤ 1ms。阈值由 T11 定；这里打印本机数值。路径解析用真实文件系统（临时目录）。

use std::collections::BTreeSet;
use std::path::PathBuf;
use std::time::Instant;

use iwork_core::approval::action::{NormalizeContext, normalize_request};
use iwork_core::approval::denial::DenialMemory;
use iwork_core::approval::env::{EnvPaths, Platform, ScopeKeys};
use iwork_core::approval::evaluate::{EvalContext, evaluate_call};
use iwork_core::approval::rules::build_ruleset;
use iwork_core::approval::session::{snapshot_for, tool_set_for};
use iwork_domain::action::{ActionKind, CanonicalPath, ToolRef};
use iwork_domain::approval_mode::ApprovalMode;
use iwork_domain::common::{Actor, ToolGroups};
use iwork_domain::id::{ApprovalId, AssistantId, ConversationId, ProjectId, RawUlid, RuleId, Seq};
use iwork_domain::rule::{RuleEffect, RuleEntry, RuleLayer, RuleMatcher, RuleScope, RuleSource};
use iwork_domain::turn::TurnKind;
use iwork_host::fs::OsPathResolver;
use iwork_ports::agent::{PermissionRequest, RawToolInput};

fn main() {
    let root = std::env::temp_dir().join(format!("iwork-bench-{}", std::process::id()));
    let ws = root.join("ws");
    std::fs::create_dir_all(ws.join("src")).unwrap();
    std::fs::create_dir_all(ws.join(".git")).unwrap();
    let root = root.canonicalize().unwrap();
    let ws = CanonicalPath(root.join("ws"));
    let home = CanonicalPath(root.clone());
    let env = EnvPaths {
        platform: Platform::MacOs,
        home: home.clone(),
        data_dir: CanonicalPath(root.join("data")),
        iwork_root: CanonicalPath(root.join("iWork")),
        workspace_root: ws.clone(),
        tmp_dirs: vec![CanonicalPath(PathBuf::from("/tmp"))],
        dm_private_dir: None,
        other_project_roots: Vec::new(),
        other_dm_dirs: Vec::new(),
    };
    let scope = ScopeKeys {
        project: Some(ProjectId::from_raw(RawUlid::from_u128(1))),
        assistant: AssistantId::from_raw(RawUlid::from_u128(2)),
        conversation: ConversationId::from_raw(RawUlid::from_u128(3)),
    };
    let fs = OsPathResolver::new(
        home.0.clone(),
        OsPathResolver::detect_case_insensitive(&root).unwrap(),
    );
    let snap = snapshot_for(ApprovalMode::Auto, &[], &scope, &env, Seq(0), &fs);
    let remembered: Vec<RuleEntry> = (0..500u128)
        .map(|i| RuleEntry {
            id: RuleId::from_raw(RawUlid::from_u128(0x10_000 + i)),
            layer: RuleLayer::Remembered,
            matcher: RuleMatcher::All(vec![
                RuleMatcher::ActionKindIs(ActionKind::Execute),
                RuleMatcher::ProgramSubcommand {
                    program: format!("tool{i}"),
                    subcommand: None,
                },
            ]),
            effect: RuleEffect::Allow,
            source: RuleSource::Remembered {
                approval_id: ApprovalId::from_raw(RawUlid::from_u128(9)),
                by: Actor("u".into()),
            },
            scope: RuleScope::Global,
            created_at: iwork_domain::common::Timestamp(0),
            label: String::new(),
            applies_in: Default::default(),
        })
        .collect();
    let pipeline = pipeline_text();
    let cases: [(&str, String, &[RuleEntry]); 3] = [
        ("① git status × 内置规则集", "git status".into(), &[]),
        ("② 20 段管道 + 10 个路径词", pipeline, &[]),
        (
            "③ 内置 + 500 条记住的规则",
            "tool499 run".into(),
            &remembered,
        ),
    ];
    let none = BTreeSet::new();
    let tool_set = tool_set_for(TurnKind::Assistant, &ToolGroups::new());
    let denial = DenialMemory::default();
    for (name, cmd, live) in cases {
        let req = PermissionRequest {
            tool: ToolRef("Bash".into()),
            raw_input: RawToolInput(serde_json::json!({ "command": cmd }).to_string()),
            bypass_sandbox: false,
            updates_before: 0,
        };
        let mut samples = Vec::with_capacity(2000);
        let (mut t_norm, mut t_eval) = (0u128, 0u128);
        for _ in 0..2000 {
            let t = Instant::now();
            let ctx = NormalizeContext {
                paths: &env,
                boundaries: &snap.boundaries,
                mcp_read_only: &none,
                cwd: &ws,
            };
            let call = normalize_request(&req, &ctx, &fs);
            t_norm += t.elapsed().as_micros();
            let t2 = Instant::now();
            let rules = build_ruleset(&snap.rules, live, &scope).unwrap();
            let ectx = EvalContext {
                mode: ApprovalMode::Auto,
                turn_kind: TurnKind::Assistant,
                tool_set: &tool_set,
                disabled_groups: &ToolGroups::new(),
                denial: &denial,
            };
            let e = evaluate_call(&call, &rules, &ectx, &fs);
            std::hint::black_box(e);
            t_eval += t2.elapsed().as_micros();
            samples.push(t.elapsed().as_micros() as u64);
        }
        samples.sort_unstable();
        let p = |q: f64| samples[((samples.len() as f64 - 1.0) * q) as usize];
        println!(
            "approval_evaluate {name}: P50 {}µs  P95 {}µs  max {}µs（平均：归一 {}µs，求值 {}µs）",
            p(0.5),
            p(0.95),
            samples[samples.len() - 1],
            t_norm / 2000,
            t_eval / 2000
        );
    }
    // 解析器单独计时：组 ② 的大部分归一时间花在这里
    let t = Instant::now();
    for _ in 0..200 {
        std::hint::black_box(iwork_shell::parse(&pipeline_text()).is_ok());
    }
    println!(
        "iwork_shell::parse（组 ② 的命令）平均 {}µs",
        t.elapsed().as_micros() / 200
    );
    let _ = std::fs::remove_dir_all(&root);
}

fn pipeline_text() -> String {
    (0..20)
        .map(|i| format!("grep -n x src/f{i}.rs"))
        .collect::<Vec<_>>()
        .join(" | ")
        + " && cat src/a src/b src/c src/d src/e src/f src/g src/h src/i src/j"
}
