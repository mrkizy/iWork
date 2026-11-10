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
pub mod find_syntax;
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
use iwork_ports::fs::{PathResolveError, PathResolver, ResolveCertainty, ResolvedPath};
use iwork_shell::{PartialFacts, ShellScript};
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

/// 一个待解析的路径词，以及它是真实目标还是候选（方案 3.1、3.5 那张表）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PathQuery {
    pub literal: String,
    pub origin: PathOrigin,
}

/// 路径从哪来：决定「走不通」时怎么办（「查不清」两者一样，整条拒）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PathOrigin {
    /// 归类成功的调用里命令真会动的路径：走不通 → 整条解析不了，询问，不猜
    Real,
    /// 候选：目录目标语义虚构的「目的 / 源名」、兜底候选、`find` 删除规则的候选。走不通 → 只丢它
    Hypothetical,
}

/// 目标：路径还是原文，其余已是最终形状。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PendingTarget {
    Path(PathQuery),
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
    /// 解析不了时用的动作从哪来（见 [`Analysis::fallback`]）
    pub fallback: Fallback,
    pub placement: SandboxPlacement,
    pub bypass_sandbox_requested: bool,
    pub description: Option<String>,
    pub raw_summary: String,
    /// Edit 的新旧文字（亲手执行补丁卡片用）
    pub edit: Option<(String, String, String)>,
    pub raw_input_digest: Sha256,
}

/// 解析不了时的动作来源。Bash 的事实按需才取：归类成功的调用（热路径上的多数）用不上它。
#[derive(Debug, Clone)]
pub enum Fallback {
    /// 非 Bash 工具：现成的动作
    Actions(Vec<PendingAction>),
    /// `parse` 放弃：第一遍事实已随 `Unparsable` 带回
    ShellFacts(PartialFacts),
    /// `parse` 成功：事实要时再调 `iwork_shell::literal_facts`；`find` 的删除用小 AST 上的精确切法
    ShellScript {
        command: String,
        script: ShellScript,
    },
}

/// 整条调用的标记：命中 `UserDeny` 层的内置拒绝条目（方案 3.3、3.5、4.2）。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct CallFlags {
    pub resource_limit_hit: bool,
    pub unlexable: bool,
    pub path_unresolved: bool,
}

impl CallFlags {
    fn any(&self) -> bool {
        self.resource_limit_hit || self.unlexable || self.path_unresolved
    }
}

impl Analysis {
    /// 解析不了时的动作与整条调用的标记。纯函数；`parse` 成功的 Bash 调用每次调都重新取事实，
    /// 只在归类放弃或有真实路径走不通时才会走到。
    pub fn fallback(&self) -> (Vec<PendingAction>, CallFlags) {
        let (facts, precise) = match &self.fallback {
            Fallback::Actions(actions) => return (actions.clone(), CallFlags::default()),
            Fallback::ShellFacts(facts) => (std::borrow::Cow::Borrowed(facts), None),
            Fallback::ShellScript { command, script } => (
                std::borrow::Cow::Owned(iwork_shell::literal_facts(command)),
                Some(bash::precise_find_deletes(script)),
            ),
        };
        let flags = CallFlags {
            resource_limit_hit: facts.resource_limit,
            unlexable: facts.lex_failed,
            path_unresolved: false,
        };
        (bash::fallback_actions(&facts, precise.as_deref()), flags)
    }
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

/// 路径事实：字面原文 → 两种视图或失败。同一个词作真实目标与作候选时解析结果相同，按原文存一份。
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PathFacts {
    pub resolved: BTreeMap<String, Result<ResolvedPath, PathResolveError>>,
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

fn real_path(raw: &str) -> PendingTarget {
    PendingTarget::Path(PathQuery {
        literal: raw.to_owned(),
        origin: PathOrigin::Real,
    })
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
        fallback: Fallback::Actions(Vec::new()),
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
                        vec![single(ActionKind::Read, real_path(p))],
                    ));
                }
                None if default_cwd => {
                    a.structured = Ok((
                        CallShape::Tool,
                        vec![single(ActionKind::Read, real_path("."))],
                    ));
                }
                None => a.structured = Err(format!("{tool} 缺字段 {field}")),
            }
            a.fallback = Fallback::Actions(vec![single(ActionKind::Read, PendingTarget::None)]);
        }
        Some(ToolClass::Write { field }) => {
            match input.str(field) {
                Some(p) => {
                    a.raw_summary = format!("{tool} {p}");
                    a.structured = Ok((
                        CallShape::Tool,
                        vec![single(ActionKind::Write, real_path(p))],
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
            a.fallback = Fallback::Actions(vec![single(ActionKind::Write, PendingTarget::None)]);
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
        a.fallback = Fallback::ShellFacts(PartialFacts::default());
        return;
    };
    a.raw_summary = command.to_owned();
    match iwork_shell::parse(command) {
        Err(u) => {
            a.structured = Err(u.why);
            a.fallback = Fallback::ShellFacts(u.facts);
        }
        Ok(script) => {
            a.placement = credential::classify(&script, &boundaries.credential_commands);
            let outside = a.bypass_sandbox_requested
                || matches!(
                    a.placement,
                    SandboxPlacement::Outside | SandboxPlacement::Ambiguous
                );
            a.structured = bash::classify_script(&script).map(|mut groups| {
                if outside {
                    bash::add_outside_sandbox(&mut groups);
                }
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
                (shape, groups.into_iter().flat_map(|g| g.actions).collect())
            });
            a.fallback = Fallback::ShellScript {
                command: command.to_owned(),
                script,
            };
        }
    }
}

fn paths_of(pending: &[PendingAction]) -> impl Iterator<Item = &PathQuery> {
    pending.iter().filter_map(|p| match &p.target {
        PendingTarget::Path(q) => Some(q),
        _ => None,
    })
}

/// 第二段（I/O）：解析出现的路径词。兜底动作的候选只在归类失败、或有真实目标走不通时才解析
/// （它们只在那时用得上；热路径上省掉大部分 lstat）。
pub fn gather_path_facts(a: &Analysis, cwd: &CanonicalPath, fs: &impl PathResolver) -> PathFacts {
    let mut facts = PathFacts::default();
    let resolve = |facts: &mut PathFacts, q: &PathQuery| {
        if !facts.resolved.contains_key(&q.literal) {
            facts
                .resolved
                .insert(q.literal.clone(), fs.resolve(cwd, &q.literal));
        }
    };
    let need_fallback = match &a.structured {
        Ok((_, pending)) => {
            let mut real_disproved = false;
            for q in paths_of(pending) {
                resolve(&mut facts, q);
                real_disproved |= q.origin == PathOrigin::Real
                    && matches!(
                        facts.resolved.get(&q.literal),
                        Some(Err(e)) if e.certainty() == ResolveCertainty::Disproved
                    );
            }
            real_disproved
        }
        Err(_) => true,
    };
    if need_fallback {
        let (fallback, _) = a.fallback();
        for q in paths_of(&fallback) {
            resolve(&mut facts, q);
        }
    }
    facts
}

/// 一个待解析动作换成规范目标后的结果（方案 3.5 那张表）。
enum Finished {
    /// 路径目标的每种视图各一个（相同时一个）；其余目标恰好一个
    Targets(Vec<ActionTarget>),
    /// 候选走不通：只丢它
    Dropped,
    /// 真实目标走不通：整条解析不了
    Disproved(String),
    /// 查不清：整条拒（`PathUnresolved`）
    Unresolved,
}

fn finish_target(t: &PendingTarget, facts: &PathFacts) -> Finished {
    let other = |t: ActionTarget| Finished::Targets(vec![t]);
    match t {
        PendingTarget::Path(q) => match facts.resolved.get(&q.literal) {
            Some(Ok(r)) => Finished::Targets(
                paths::views(r)
                    .map(|p| ActionTarget::Path(p.clone()))
                    .collect(),
            ),
            Some(Err(e)) => match (e.certainty(), q.origin) {
                (ResolveCertainty::Unknown, _) => Finished::Unresolved,
                (ResolveCertainty::Disproved, PathOrigin::Hypothetical) => Finished::Dropped,
                (ResolveCertainty::Disproved, PathOrigin::Real) => {
                    Finished::Disproved(format!("路径无法解析：{}（{e}）", q.literal))
                }
            },
            // 取事实漏了这个词是程序缺陷：失败即关闭，按查不清拒
            None => {
                debug_assert!(false, "路径没有取事实：{}", q.literal);
                Finished::Unresolved
            }
        },
        PendingTarget::Command {
            program,
            subcommand,
            argv,
        } => other(ActionTarget::Command {
            program: program.clone(),
            subcommand: subcommand.clone(),
            argv: argv.clone(),
        }),
        PendingTarget::ToolName(n) => other(ActionTarget::ToolName(n.clone())),
        PendingTarget::Remote(r) => other(ActionTarget::Remote(r.clone())),
        PendingTarget::None => other(ActionTarget::None),
    }
}

/// 一组待解析动作换成规范目标。真实目标走不通时返回 `Err(原因)`；查不清的记进 `flags`。
fn finish_all<'p>(
    pending: &'p [PendingAction],
    facts: &PathFacts,
    flags: &mut CallFlags,
) -> Result<Vec<(&'p PendingAction, ActionTarget)>, String> {
    let mut out = Vec::with_capacity(pending.len());
    for p in pending {
        match finish_target(&p.target, facts) {
            Finished::Targets(ts) => out.extend(ts.into_iter().map(|t| (p, t))),
            Finished::Dropped => {}
            Finished::Unresolved => flags.path_unresolved = true,
            Finished::Disproved(why) => return Err(why),
        }
    }
    Ok(out)
}

/// 第三段（纯）：换成规范路径，算标记。`fs` 只用来比较路径（大小写规则），不碰磁盘。
pub fn normalize(
    a: &Analysis,
    ctx: &NormalizeContext,
    facts: &PathFacts,
    fs: &impl PathResolver,
) -> NormalizedCall {
    let mut flags = CallFlags::default();
    let structured = a
        .structured
        .as_ref()
        .map_err(Clone::clone)
        .and_then(|(shape, pending)| Ok((shape.clone(), finish_all(pending, facts, &mut flags)?)));
    let fallback_store;
    let (shape, resolved): (CallShape, Vec<(&PendingAction, ActionTarget)>) = match structured {
        Ok(x) => x,
        Err(why) => {
            let (fallback, fallback_flags) = a.fallback();
            fallback_store = fallback;
            flags.resource_limit_hit |= fallback_flags.resource_limit_hit;
            flags.unlexable |= fallback_flags.unlexable;
            // 兜底动作全是候选，不会走到「真实目标走不通」
            let actions = finish_all(&fallback_store, facts, &mut flags).unwrap_or_default();
            (CallShape::Unparsable { why }, actions)
        }
    };
    // 参数标记只给归类成功的调用：解析不了的兜底候选三档都问，不能借默认层的参数放行条目放行
    let structured_ok = !matches!(shape, CallShape::Unparsable { .. });
    let mut actions: Vec<Action> = resolved
        .into_iter()
        .map(|(p, target)| finish_action(a, ctx, fs, p, target, structured_ok))
        .collect();
    if actions.is_empty() {
        // 非 Bash 工具解析不了且没有兜底（联网、MCP 不会走到这里）
        actions.push(finish_action(
            a,
            ctx,
            fs,
            &single(ActionKind::Unclassified, PendingTarget::None),
            ActionTarget::None,
            false,
        ));
    }
    if flags.any() {
        // 承载整条调用的动作：命中 UserDeny 层的内置拒绝条目（方案 4.2），排在最后，底线命中的动作仍在它前面
        let mut carrier = finish_action(
            a,
            ctx,
            fs,
            &single(
                ActionKind::Unclassified,
                PendingTarget::ToolName(a.tool.0.clone()),
            ),
            ActionTarget::ToolName(a.tool.0.clone()),
            false,
        );
        carrier.flags.resource_limit_hit = flags.resource_limit_hit;
        carrier.flags.unlexable = flags.unlexable;
        carrier.flags.path_unresolved = flags.path_unresolved;
        actions.push(carrier);
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

fn finish_action(
    a: &Analysis,
    ctx: &NormalizeContext,
    fs: &impl PathResolver,
    p: &PendingAction,
    target: ActionTarget,
    tag_arguments: bool,
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
    // 沙箱外执行是调用级约束：执行动作改种类；没有执行动作的命令在归类时已另加了沙箱外动作（方案 3.8）
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
            argument_path: tag_arguments && p.argument,
            ..ActionFlags::default()
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
