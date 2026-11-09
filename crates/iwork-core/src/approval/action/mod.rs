//! 动作归一（T07 方案第 3 节）：一次工具调用 → 一组动作。
//!
//! 分三段，取事实与纯归一分开（方案 3.1）：
//! 1. [`analyze`]（纯）：解析工具输入与 Bash 文字，得到待解析的动作，路径词还是原文；
//! 2. [`gather_path_facts`]（I/O）：把出现的路径词交给 `PathResolver`，拿回规范路径或失败；
//! 3. [`normalize`]（纯）：只认取来的事实，换成规范路径，算越界、自动执行配置、沙箱外等标记。
//!
//! 引擎里三段连着调（[`normalize_request`]）。

pub mod bash;
pub mod commands;
pub mod credential;
pub mod credential_corpus;
pub mod paths;
pub mod tool_input;

use std::collections::{BTreeMap, BTreeSet};

use iwork_domain::action::{
    Action, ActionFingerprint, ActionFlags, ActionKind, ActionTarget, CallShapeKind, CanonicalPath,
    SandboxPlacement, ToolRef,
};
use iwork_domain::approval::ApprovalCallRecord;
use iwork_domain::common::Sha256;
use iwork_ports::agent::PermissionRequest;
use iwork_ports::fs::{PathResolveError, PathResolver};
use sha2::Digest;

use crate::approval::env::EnvPaths;
use crate::approval::registry::{
    self, BASH_COMMAND_FIELD, BASH_DESCRIPTION_FIELD, BASH_DISABLE_SANDBOX_FIELD, MCP_PREFIX,
    ToolClass,
};
use crate::approval::rules::{Boundaries, PathRule};
use tool_input::{ToolInput, url_host};

/// 归一时目标还没解析的动作。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PendingAction {
    pub kind: ActionKind,
    pub target: PendingTarget,
    /// 执行脚本类：审批看不到脚本内部
    pub opaque: bool,
    /// 角色未知的参数路径（表外程序的位置参数、兜底候选）：既记读又记写，只为过底线与越界
    pub argument: bool,
}

/// 目标：路径还是原文，其余已是最终形状。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PendingTarget {
    Path(String),
    Command {
        program: String,
        subcommand: Option<String>,
        argv: Vec<String>,
    },
    ToolName(String),
    Remote(String),
    None,
}

/// 调用的形状（方案 3.1）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CallShape {
    /// 非 Bash 工具：一个动作
    Tool,
    /// 恰好一个简单命令（允许字面重定向与引号界定的 here-doc）
    SingleSimpleCommand { command_text: String },
    /// 多个简单命令，且每个都归类成功
    Compound,
    /// 解析失败、含任何展开 / 通配 / cd / 前置赋值、或含小 AST 表达不了的形态
    Unparsable { why: String },
}

impl CallShape {
    pub fn kind(&self) -> CallShapeKind {
        match self {
            Self::Tool => CallShapeKind::Tool,
            Self::SingleSimpleCommand { .. } => CallShapeKind::SingleSimpleCommand,
            Self::Compound => CallShapeKind::Compound,
            Self::Unparsable { .. } => CallShapeKind::Unparsable,
        }
    }
}

/// 第一段的结果：待解析的调用。
#[derive(Debug, Clone)]
pub struct Analysis {
    pub tool: ToolRef,
    /// 归类成功时的形状与动作；`Err` 是整条解析不了的原因
    pub structured: Result<(CallShape, Vec<PendingAction>), String>,
    /// 解析不了时用的动作（Bash 才有；其他工具解析不了时只有一个无目标的动作）
    pub fallback: Vec<PendingAction>,
    pub placement: SandboxPlacement,
    pub bypass_sandbox_requested: bool,
    pub description: Option<String>,
    pub raw_summary: String,
    /// Edit 的新旧文字（亲手执行补丁卡片用）
    pub edit: Option<(String, String, String)>,
    pub raw_input_digest: Sha256,
}

/// 归一的上下文。全部来自会话（快照、环境），不碰磁盘。
pub struct NormalizeContext<'a> {
    pub paths: &'a EnvPaths,
    pub boundaries: &'a Boundaries,
    /// 声明了 readOnlyHint 的 MCP 工具全名；不知道的一律当非只读
    pub mcp_read_only: &'a BTreeSet<String>,
    /// hook 输入里的 cwd，已规范化
    pub cwd: &'a CanonicalPath,
}

/// 路径事实：字面原文 → 规范路径或失败。
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PathFacts {
    pub resolved: BTreeMap<String, Result<CanonicalPath, PathResolveError>>,
}

/// 归一结果。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NormalizedCall {
    pub tool: ToolRef,
    /// 至少一个
    pub actions: Vec<Action>,
    pub shape: CallShape,
    pub placement: SandboxPlacement,
    pub bypass_sandbox_requested: bool,
    pub cwd: CanonicalPath,
    /// Bash 的 description：亲手执行卡片的「理由」
    pub description: Option<String>,
    /// 整条命令文字或「Edit path」这类一行摘要
    pub raw_summary: String,
    /// Edit 的 (路径原文, 旧文字, 新文字)
    pub edit: Option<(String, String, String)>,
    pub raw_input_digest: Sha256,
}

impl NormalizedCall {
    /// 全部子动作指纹，去重（方案 3.9）。
    pub fn fingerprints(&self) -> BTreeSet<ActionFingerprint> {
        self.actions.iter().map(Action::fingerprint).collect()
    }

    /// 有序子动作 ‖ 形状 ‖ 沙箱位置 ‖ cwd 的摘要：同文字不同解析（如升级后归一变了）也不相等。
    pub fn normalized_digest(&self) -> Sha256 {
        let mut h = sha2::Sha256::new();
        for a in &self.actions {
            h.update(format!("{:?}|{:?}|{:?}\n", a.kind, a.target, a.flags).as_bytes());
        }
        h.update(
            format!(
                "{:?}|{:?}|{}",
                self.shape.kind(),
                self.placement,
                self.cwd.0.display()
            )
            .as_bytes(),
        );
        Sha256(h.finalize().into())
    }

    /// 持久化到 `Approval.call` 的记录。
    pub fn record(&self, representative: usize) -> ApprovalCallRecord {
        ApprovalCallRecord {
            tool: self.tool.clone(),
            cwd: self.cwd.clone(),
            shape: self.shape.kind(),
            placement: self.placement,
            bypass_sandbox_requested: self.bypass_sandbox_requested,
            actions: self.actions.clone(),
            representative: u32::try_from(representative).expect("子动作个数远小于 u32"),
            raw_input_digest: self.raw_input_digest,
            normalized_digest: self.normalized_digest(),
        }
    }
}

/// sha256(工具名 ‖ 0 ‖ 规范化 JSON)。
pub fn raw_input_digest(tool: &str, raw: &str) -> Sha256 {
    let input = ToolInput::parse(raw);
    let mut h = sha2::Sha256::new();
    h.update(tool.as_bytes());
    h.update([0]);
    h.update(input.canonical(raw).as_bytes());
    Sha256(h.finalize().into())
}

fn single(kind: ActionKind, target: PendingTarget) -> PendingAction {
    PendingAction {
        kind,
        target,
        opaque: false,
        argument: false,
    }
}

/// 第一段（纯）：解析工具输入。
pub fn analyze(
    req: &PermissionRequest,
    boundaries: &Boundaries,
    mcp_read_only: &BTreeSet<String>,
) -> Analysis {
    let tool = req.tool.0.as_str();
    let raw = req.raw_input.0.as_str();
    let input = ToolInput::parse(raw);
    let bypass = req.bypass_sandbox || input.flag(BASH_DISABLE_SANDBOX_FIELD);
    let mut a = Analysis {
        tool: req.tool.clone(),
        structured: Err(String::new()),
        fallback: Vec::new(),
        placement: SandboxPlacement::Inside,
        bypass_sandbox_requested: bypass,
        description: None,
        raw_summary: tool.to_owned(),
        edit: None,
        raw_input_digest: raw_input_digest(tool, raw),
    };
    let class = registry::tool(tool).map(|t| t.class);
    let tool_target = || PendingTarget::ToolName(tool.to_owned());
    match class {
        Some(ToolClass::Bash) => analyze_bash(&mut a, &input, boundaries),
        Some(ToolClass::Read { field, default_cwd }) => {
            match input.str(field) {
                Some(p) => {
                    a.raw_summary = format!("{tool} {p}");
                    a.structured = Ok((
                        CallShape::Tool,
                        vec![single(ActionKind::Read, PendingTarget::Path(p.to_owned()))],
                    ));
                }
                None if default_cwd => {
                    a.structured = Ok((
                        CallShape::Tool,
                        vec![single(ActionKind::Read, PendingTarget::Path(".".into()))],
                    ));
                }
                None => a.structured = Err(format!("{tool} 缺字段 {field}")),
            }
            a.fallback = vec![single(ActionKind::Read, PendingTarget::None)];
        }
        Some(ToolClass::Write { field }) => {
            match input.str(field) {
                Some(p) => {
                    a.raw_summary = format!("{tool} {p}");
                    a.structured = Ok((
                        CallShape::Tool,
                        vec![single(ActionKind::Write, PendingTarget::Path(p.to_owned()))],
                    ));
                    if tool == registry::PATCH_TOOL
                        && let (Some(old), Some(new)) = (
                            input.str(registry::EDIT_OLD_FIELD),
                            input.str(registry::EDIT_NEW_FIELD),
                        )
                    {
                        a.edit = Some((p.to_owned(), old.to_owned(), new.to_owned()));
                    }
                }
                None => a.structured = Err(format!("{tool} 缺字段 {field}")),
            }
            a.fallback = vec![single(ActionKind::Write, PendingTarget::None)];
        }
        Some(ToolClass::Network { url_field }) => {
            let target = match url_field {
                Some(f) => {
                    PendingTarget::Remote(input.str(f).map_or_else(|| "?".to_owned(), url_host))
                }
                None => PendingTarget::Remote("search".into()),
            };
            a.structured = Ok((CallShape::Tool, vec![single(ActionKind::Network, target)]));
        }
        Some(ToolClass::Harmless | ToolClass::Disabled) | None => {
            let kind = match class {
                None if tool.starts_with(MCP_PREFIX) => {
                    if mcp_read_only.contains(tool) {
                        ActionKind::Read
                    } else {
                        ActionKind::McpCall
                    }
                }
                _ => ActionKind::Unclassified,
            };
            a.structured = Ok((CallShape::Tool, vec![single(kind, tool_target())]));
        }
    }
    a
}

fn analyze_bash(a: &mut Analysis, input: &ToolInput, boundaries: &Boundaries) {
    a.description = input.str(BASH_DESCRIPTION_FIELD).map(str::to_owned);
    let Some(command) = input.str(BASH_COMMAND_FIELD) else {
        a.structured = Err(format!("Bash 缺字段 {BASH_COMMAND_FIELD}"));
        a.fallback = bash::fallback_actions(&Default::default(), None);
        return;
    };
    a.raw_summary = command.to_owned();
    match iwork_shell::parse(command) {
        Err(u) => {
            a.fallback =
                bash::fallback_actions(&u.facts, u.facts.literal_words.first().map(String::as_str));
            a.structured = Err(u.why);
        }
        Ok(script) => {
            let facts = bash::facts_of(&script);
            a.fallback =
                bash::fallback_actions(&facts, facts.literal_words.first().map(String::as_str));
            a.placement = credential::classify(&script, &boundaries.credential_commands);
            a.structured = bash::classify_script(&script).map(|actions| {
                let single = script.items.len() == 1
                    && script.items[0].pipeline.len() == 1
                    && !script.items[0].negated
                    && script.items[0].connector.is_none()
                    && matches!(
                        script.items[0].pipeline[0],
                        iwork_shell::CommandNode::Simple(_)
                    );
                let shape = if single {
                    CallShape::SingleSimpleCommand {
                        command_text: command.to_owned(),
                    }
                } else {
                    CallShape::Compound
                };
                (shape, actions)
            });
        }
    }
}

fn pending_paths(a: &Analysis) -> impl Iterator<Item = &str> {
    let structured = a
        .structured
        .as_ref()
        .map(|(_, s)| s.as_slice())
        .unwrap_or(&[]);
    structured
        .iter()
        .chain(&a.fallback)
        .filter_map(|p| match &p.target {
            PendingTarget::Path(w) => Some(w.as_str()),
            _ => None,
        })
}

/// 第二段（I/O）：解析出现的全部路径词。
pub fn gather_path_facts(a: &Analysis, cwd: &CanonicalPath, fs: &impl PathResolver) -> PathFacts {
    let mut facts = PathFacts::default();
    for w in pending_paths(a) {
        if !facts.resolved.contains_key(w) {
            facts.resolved.insert(w.to_owned(), fs.resolve(cwd, w));
        }
    }
    facts
}

/// 第三段（纯）：换成规范路径，算标记。`fs` 只用来比较路径（大小写规则），不碰磁盘。
pub fn normalize(
    a: &Analysis,
    ctx: &NormalizeContext,
    facts: &PathFacts,
    fs: &impl PathResolver,
) -> NormalizedCall {
    let resolve = |w: &str| -> Result<CanonicalPath, String> {
        match facts.resolved.get(w) {
            Some(Ok(p)) => Ok(p.clone()),
            Some(Err(e)) => Err(format!("路径无法解析：{w}（{e}）")),
            None => Err(format!("路径没有取事实：{w}")),
        }
    };
    let structured = a
        .structured
        .as_ref()
        .map_err(Clone::clone)
        .and_then(|(shape, pending)| {
            let mut out = Vec::with_capacity(pending.len());
            for p in pending {
                out.push((p, finish_target(&p.target, &resolve)?));
            }
            Ok((shape.clone(), out))
        });
    let (shape, resolved): (CallShape, Vec<(&PendingAction, ActionTarget)>) = match structured {
        Ok(x) => x,
        Err(why) => {
            // 解析不了：兜底动作里解析失败的路径丢掉，不影响别的候选
            let actions = a
                .fallback
                .iter()
                .filter_map(|p| finish_target(&p.target, &resolve).ok().map(|t| (p, t)))
                .collect();
            (CallShape::Unparsable { why }, actions)
        }
    };
    let mut actions: Vec<Action> = resolved
        .into_iter()
        .map(|(p, target)| finish_action(a, ctx, fs, p, target))
        .collect();
    if actions.is_empty() {
        // 非 Bash 工具解析不了且没有兜底（联网、MCP 不会走到这里）
        actions.push(finish_action(
            a,
            ctx,
            fs,
            &single(ActionKind::Unclassified, PendingTarget::None),
            ActionTarget::None,
        ));
    }
    NormalizedCall {
        tool: a.tool.clone(),
        actions,
        shape,
        placement: a.placement,
        bypass_sandbox_requested: a.bypass_sandbox_requested,
        cwd: ctx.cwd.clone(),
        description: a.description.clone(),
        raw_summary: a.raw_summary.clone(),
        edit: a.edit.clone(),
        raw_input_digest: a.raw_input_digest,
    }
}

fn finish_target(
    t: &PendingTarget,
    resolve: &impl Fn(&str) -> Result<CanonicalPath, String>,
) -> Result<ActionTarget, String> {
    Ok(match t {
        PendingTarget::Path(w) => ActionTarget::Path(resolve(w)?),
        PendingTarget::Command {
            program,
            subcommand,
            argv,
        } => ActionTarget::Command {
            program: program.clone(),
            subcommand: subcommand.clone(),
            argv: argv.clone(),
        },
        PendingTarget::ToolName(n) => ActionTarget::ToolName(n.clone()),
        PendingTarget::Remote(r) => ActionTarget::Remote(r.clone()),
        PendingTarget::None => ActionTarget::None,
    })
}

fn finish_action(
    a: &Analysis,
    ctx: &NormalizeContext,
    fs: &impl PathResolver,
    p: &PendingAction,
    target: ActionTarget,
) -> Action {
    let mut kind = p.kind;
    let path = match &target {
        ActionTarget::Path(path) => Some(path),
        _ => None,
    };
    // 写会被自动执行的配置：改判种类（参数路径也改判：宁可多问）
    if kind == ActionKind::Write && path.is_some_and(|t| is_autorun(t, ctx.boundaries, fs)) {
        kind = ActionKind::WriteAutorunConfig;
    }
    // 沙箱外执行是调用级约束，只改执行动作的种类；推送、发布、删除等必问种类保留（方案 3.8）
    let outside = a.bypass_sandbox_requested
        || matches!(
            a.placement,
            SandboxPlacement::Outside | SandboxPlacement::Ambiguous
        );
    if kind == ActionKind::Execute && outside {
        kind = ActionKind::OutsideSandbox;
    }
    let out_of_bounds = match (kind, path) {
        (ActionKind::Write | ActionKind::Delete | ActionKind::WriteAutorunConfig, Some(t)) => {
            !is_writable(t, ctx, fs)
        }
        (ActionKind::Read, Some(t)) => {
            ctx.boundaries
                .sensitive_dirs
                .iter()
                .any(|d| fs.is_under(t, d))
                && !fs.is_under(t, &ctx.paths.workspace_root)
        }
        (ActionKind::OutsideSandbox, _) => a.bypass_sandbox_requested,
        _ => false,
    };
    Action {
        kind,
        target,
        tool: a.tool.clone(),
        raw_summary: a.raw_summary.clone(),
        flags: ActionFlags {
            out_of_bounds,
            bypass_sandbox_requested: a.bypass_sandbox_requested,
            opaque_script: p.opaque,
        },
    }
}

fn is_autorun(t: &CanonicalPath, b: &Boundaries, fs: &impl PathResolver) -> bool {
    b.autorun.iter().any(|r| match r {
        PathRule::Under(d) => fs.is_under(t, d),
        PathRule::Is(p) => fs.same_path(t, p),
    })
}

/// 写在边界内：工作区、临时目录、可写缓存、可写目录、私聊私有目录之下（方案 3.8）。
fn is_writable(t: &CanonicalPath, ctx: &NormalizeContext, fs: &impl PathResolver) -> bool {
    std::iter::once(&ctx.paths.workspace_root)
        .chain(&ctx.paths.tmp_dirs)
        .chain(&ctx.boundaries.writable_cache)
        .chain(&ctx.boundaries.writable_dirs)
        .chain(ctx.paths.dm_private_dir.iter())
        .any(|d| fs.is_under(t, d))
}

/// 三段连着调（引擎用）。
pub fn normalize_request(
    req: &PermissionRequest,
    ctx: &NormalizeContext,
    fs: &impl PathResolver,
) -> NormalizedCall {
    let a = analyze(req, ctx.boundaries, ctx.mcp_read_only);
    let facts = gather_path_facts(&a, ctx.cwd, fs);
    normalize(&a, ctx, &facts, fs)
}
