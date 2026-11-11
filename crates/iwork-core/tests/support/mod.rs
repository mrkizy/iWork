//! 审批测试共用的环境：固定形状的路径、同构的 `FakeFs`、ID 与范围。

#![allow(dead_code, reason = "各测试各用一部分")]

#[path = "../../../iwork-ports/tests/fake_fs/mod.rs"]
pub mod fake_fs;

#[path = "../../../iwork-ports/tests/reference_projection/mod.rs"]
pub mod reference_projection;

pub mod engine_fakes;
pub mod fold_fx;
pub mod judges;
pub mod ordered_decider;
pub mod step_store;

use std::path::PathBuf;

use fake_fs::FakeFs;
use iwork_core::approval::env::{EnvPaths, Platform, ScopeKeys};
use iwork_domain::action::CanonicalPath;
use iwork_domain::id::{AssistantId, ConversationId, ProjectId, RawUlid};

pub const HOME: &str = "/home/u";
pub const DATA: &str = "/home/u/Library/Application Support/iWork";
pub const IWORK_ROOT: &str = "/home/u/iWork";
pub const WS: &str = "/home/u/iWork/projects/proj";
pub const TMP: &str = "/tmp";

pub fn cp(p: &str) -> CanonicalPath {
    CanonicalPath(PathBuf::from(p))
}

/// macOS 形状的环境（路径用 `/home/u` 前缀，与真实用户无关）。
pub fn env() -> EnvPaths {
    env_at(WS)
}

pub fn env_at(workspace: &str) -> EnvPaths {
    EnvPaths {
        platform: Platform::MacOs,
        home: cp(HOME),
        data_dir: cp(DATA),
        iwork_root: cp(IWORK_ROOT),
        workspace_root: cp(workspace),
        tmp_dirs: vec![cp(TMP)],
        dm_private_dir: None,
        other_project_roots: vec![cp("/home/u/iWork/projects/other")],
        other_dm_dirs: vec![cp("/home/u/iWork/assistants/bob")],
    }
}

/// 与 `env()` 同构的文件系统：工作区、`.git`、凭据目录与几条链接。大小写敏感（Linux 形状）。
pub fn fs() -> FakeFs {
    fs_ci(false)
}

pub fn fs_ci(case_insensitive: bool) -> FakeFs {
    let mut f = FakeFs::new(HOME, case_insensitive);
    for p in [
        "/home/u/iWork/projects/proj/src/a.rs",
        "/home/u/iWork/projects/proj/.git/objects",
        "/home/u/iWork/projects/proj/.git/config",
        "/home/u/iWork/projects/proj/.git/hooks",
        "/home/u/.ssh/id_rsa",
        "/home/u/.ssh/config",
        "/home/u/.aws/credentials",
        "/home/u/Documents/notes/x.txt",
        "/home/u/Library/Application Support/iWork/attachments/a.png",
        "/home/u/Library/Application Support/iWork/imports/skill.md",
        "/home/u/Library/Application Support/iWork/artifacts/log.txt",
        "/home/u/Library/Application Support/iWork/iwork.db",
        "/usr/include/stdio.h",
        "/tmp",
    ] {
        f.add(p);
    }
    // 工作区里指向 .git/objects、指向凭据目录、指向工作区外的链接
    f.link("/home/u/iWork/projects/proj/objs", ".git/objects");
    f.link("/home/u/iWork/projects/proj/keys", "/home/u/.ssh");
    f.link("/home/u/iWork/projects/proj/out", "/home/u/outside");
    f
}

pub fn project() -> ProjectId {
    ProjectId::from_raw(RawUlid::from_u128(0x11))
}

pub fn other_project() -> ProjectId {
    ProjectId::from_raw(RawUlid::from_u128(0x12))
}

pub fn assistant() -> AssistantId {
    AssistantId::from_raw(RawUlid::from_u128(0x21))
}

pub fn conversation() -> ConversationId {
    ConversationId::from_raw(RawUlid::from_u128(0x31))
}

pub fn scope() -> ScopeKeys {
    ScopeKeys {
        project: Some(project()),
        assistant: assistant(),
        conversation: conversation(),
    }
}

use std::collections::BTreeSet;

use iwork_core::approval::action::{NormalizeContext, NormalizedCall, normalize_request};
use iwork_core::approval::session::{SessionSnapshot, snapshot_for};
use iwork_domain::action::{Action, ActionKind, ActionTarget, ToolRef};
use iwork_domain::approval_mode::ApprovalMode;
use iwork_domain::id::Seq;
use iwork_ports::agent::{PermissionRequest, RawToolInput};

pub fn snapshot() -> SessionSnapshot {
    snapshot_on(ApprovalMode::Manual, &[], &env(), Seq(0), &fs())
}

/// 会话启动时的两段：先经文件系统取规则路径的事实，再纯函数算快照（方案 4.3、6.9）。
pub fn snapshot_on(
    mode: ApprovalMode,
    lists: &[RuleEntry],
    env: &EnvPaths,
    seq: Seq,
    fs: &fake_fs::FakeFs,
) -> SessionSnapshot {
    let rule_paths = iwork_core::approval::session::rule_paths_for(lists, &scope(), env, fs);
    snapshot_for(mode, lists, &scope(), env, seq, &rule_paths)
}

pub fn bash_req(command: &str) -> PermissionRequest {
    PermissionRequest {
        tool: ToolRef("Bash".into()),
        raw_input: RawToolInput(serde_json::json!({ "command": command }).to_string()),
        bypass_sandbox: false,
        updates_before: 0,
        cwd: None,
    }
}

pub fn tool_req(tool: &str, input: serde_json::Value) -> PermissionRequest {
    PermissionRequest {
        tool: ToolRef(tool.into()),
        raw_input: RawToolInput(input.to_string()),
        bypass_sandbox: false,
        updates_before: 0,
        cwd: None,
    }
}

/// 在工作区根下归一一次调用（`env()` + `fs()` + 默认快照）。
pub fn norm(req: &PermissionRequest) -> NormalizedCall {
    norm_with(req, &snapshot(), &fs())
}

pub fn norm_with(
    req: &PermissionRequest,
    snap: &SessionSnapshot,
    fs: &fake_fs::FakeFs,
) -> NormalizedCall {
    norm_at(req, snap, fs, &cp(WS), &env())
}

/// 在给定 cwd 下归一一次调用。
pub fn norm_at(
    req: &PermissionRequest,
    snap: &SessionSnapshot,
    fs: &fake_fs::FakeFs,
    cwd: &CanonicalPath,
    e: &EnvPaths,
) -> NormalizedCall {
    let none = BTreeSet::new();
    let ctx = NormalizeContext {
        paths: e,
        boundaries: &snap.boundaries,
        mcp_read_only: &none,
        cwd,
    };
    normalize_request(req, &ctx, fs)
}

/// 动作的简写：`Read /path`、`Execute git status`。
pub fn brief(a: &Action) -> String {
    let t = match &a.target {
        ActionTarget::Path(p) => p.0.display().to_string(),
        ActionTarget::Paths(ps) => format!("{ps:?}"),
        ActionTarget::Command {
            program,
            subcommand,
            ..
        } => match subcommand {
            Some(s) => format!("{program} {s}"),
            None => program.clone(),
        },
        ActionTarget::ToolName(n) => n.clone(),
        ActionTarget::Remote(r) => r.clone(),
        ActionTarget::None => "-".into(),
    };
    format!("{:?} {t}", a.kind)
}

pub fn briefs(c: &NormalizedCall) -> Vec<String> {
    c.actions.iter().map(brief).collect()
}

pub fn has(c: &NormalizedCall, kind: ActionKind, path: &str) -> bool {
    c.actions
        .iter()
        .any(|a| a.kind == kind && a.target == ActionTarget::Path(cp(path)))
}

use iwork_core::approval::denial::DenialMemory;
use iwork_core::approval::evaluate::{CallEvaluation, EvalContext, evaluate_call};
use iwork_core::approval::rules::build_ruleset;
use iwork_core::approval::session::tool_set_for;
use iwork_domain::common::ToolGroups;
use iwork_domain::rule::{RuleEntry, Verdict};
use iwork_domain::turn::TurnKind;

/// 一次求值的全部输入，默认是：助手回合、没有活规则、没有拒绝记忆、没有关掉的分组。
pub struct Harness {
    pub snap: SessionSnapshot,
    pub live: Vec<RuleEntry>,
    pub denial: DenialMemory,
    pub turn: TurnKind,
    pub disabled: ToolGroups,
    pub fs: fake_fs::FakeFs,
    /// hook 输入里的 cwd，默认工作区根
    pub cwd: CanonicalPath,
    /// 环境路径，默认 `env()`；换了它要用同一个环境重算 `snap`
    pub env: EnvPaths,
}

impl Default for Harness {
    fn default() -> Self {
        Self {
            snap: snapshot(),
            live: Vec::new(),
            denial: DenialMemory::default(),
            turn: TurnKind::Assistant,
            disabled: ToolGroups::new(),
            fs: fs(),
            cwd: cp(WS),
            env: env(),
        }
    }
}

impl Harness {
    pub fn eval(
        &self,
        req: &PermissionRequest,
        mode: ApprovalMode,
    ) -> (NormalizedCall, CallEvaluation) {
        let call = norm_at(req, &self.snap, &self.fs, &self.cwd, &self.env);
        let rules = build_ruleset(&self.snap.rules, &self.live, &scope()).unwrap();
        let tool_set = tool_set_for(self.turn, &self.disabled);
        let ctx = EvalContext {
            mode,
            turn_kind: self.turn,
            tool_set: &tool_set,
            disabled_groups: &self.disabled,
            denial: &self.denial,
        };
        let e = evaluate_call(&call, &rules, &ctx, &self.fs);
        (call, e)
    }

    pub fn verdict(&self, req: &PermissionRequest, mode: ApprovalMode) -> Verdict {
        self.eval(req, mode).1.evaluation.verdict
    }
}

pub const MODES: [ApprovalMode; 3] = [
    ApprovalMode::Manual,
    ApprovalMode::Auto,
    ApprovalMode::FullAccess,
];

/// 默认环境下一条 Bash 命令在某档位的结果。
pub fn bash_verdict(cmd: &str, mode: ApprovalMode) -> Verdict {
    Harness::default().verdict(&bash_req(cmd), mode)
}

/// 结果的简写：Allow / Ask / OutsideSandbox / Deny / UserAction，便于表驱动。
pub fn v(verdict: &Verdict) -> &'static str {
    match verdict {
        Verdict::Allow => "Allow",
        Verdict::Ask(_) => "Ask",
        Verdict::OutsideSandbox(_) => "Outside",
        Verdict::Deny(_) => "Deny",
        Verdict::UserAction(_) => "UserAction",
    }
}

/// 三档的简写结果。
pub fn three(req: &PermissionRequest) -> [&'static str; 3] {
    let h = Harness::default();
    MODES.map(|m| v(&h.verdict(req, m)))
}

use iwork_domain::common::{Actor, Timestamp};
use iwork_domain::id::{ApprovalId, RuleId};
use iwork_domain::rule::{RuleEffect, RuleLayer, RuleMatcher, RuleScope, RuleSource};

/// 一条活规则（记住 / 项目放行 / 用户拒绝）。
pub fn live_rule(n: u128, layer: RuleLayer, matcher: RuleMatcher, scope: RuleScope) -> RuleEntry {
    RuleEntry {
        id: RuleId::from_raw(RawUlid::from_u128(0x1000 + n)),
        layer,
        matcher,
        effect: if layer == RuleLayer::UserDeny {
            RuleEffect::Deny
        } else {
            RuleEffect::Allow
        },
        source: RuleSource::Remembered {
            approval_id: ApprovalId::from_raw(RawUlid::from_u128(0x2000 + n)),
            by: Actor("user".into()),
        },
        scope,
        created_at: Timestamp(1),
        label: format!("live{n}"),
        applies_in: Default::default(),
    }
}

pub fn kind_and(kind: ActionKind, m: RuleMatcher) -> RuleMatcher {
    RuleMatcher::All(vec![RuleMatcher::ActionKindIs(kind), m])
}

pub fn read_req(path: &str) -> PermissionRequest {
    tool_req("Read", serde_json::json!({ "file_path": path }))
}

pub fn write_req(path: &str) -> PermissionRequest {
    tool_req(
        "Write",
        serde_json::json!({ "file_path": path, "content": "x" }),
    )
}

pub fn edit_req(path: &str) -> PermissionRequest {
    tool_req(
        "Edit",
        serde_json::json!({ "file_path": path, "old_string": "a", "new_string": "b" }),
    )
}
