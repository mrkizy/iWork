//! 每个回合开始前重算会话快照（T10a 方案 D8）：已提交的清单与设置 + 此刻磁盘上的路径 → 一份 `SessionKeys`，
//! 同一份交给运行时（`SessionSpec`）与判定方。**构造 `EnvPaths` 的函数只有 [`env_paths`] 一个**（T10e 1.4 的 A3）。
//!
//! 不拿引擎折叠里的序号当「不用重读」的依据：它可能还没追平，磁盘上的变化它也看不到。

use std::path::{Path, PathBuf};
use std::sync::Arc;

use iwork_core::approval::engine::SessionKeys;
use iwork_core::approval::env::{EnvPaths, ScopeKeys};
use iwork_core::approval::session::{
    SessionSnapshot, rule_paths_for, snapshot_for, tool_set_for, tools_to_disable,
};
use iwork_domain::action::{CanonicalPath, ToolRef};
use iwork_domain::approval_mode::{ApprovalMode, ModeInputs, effective_mode};
use iwork_domain::assistant::Assistant;
use iwork_domain::conversation::ConversationKind;
use iwork_domain::id::ProjectId;
use iwork_domain::project::{AiReviewSettings, Project};
use iwork_domain::turn::Turn;
use iwork_ports::agent::SessionSpec;
use iwork_ports::fs::PathResolver;
use iwork_ports::read_model::SnapshotInputs;

use super::ports::HostEnv;

/// 用户可见的 iWork 根目录（`~/iWork`）在家目录下的名字。
const IWORK_ROOT: &str = "iWork";
/// 助手私聊目录（`~/iWork/assistants/<助手名>/`，技术设计 9.1）。
const ASSISTANTS_DIR: &str = "assistants";

/// 算不出来的原因（这一趟以「没开始」结束）。
pub type NotPrepared = String;

/// 一条路径的规范形：解析得出来就用解析后的位置，解析不了（还不存在等）就用原样。
fn canonical(fs: &impl PathResolver, p: &Path) -> CanonicalPath {
    let root = CanonicalPath(PathBuf::from("/"));
    match p.to_str().map(|s| fs.resolve(&root, s)) {
        Some(Ok(r)) => r.target,
        _ => CanonicalPath(p.to_path_buf()),
    }
}

fn dm_dir(iwork_root: &Path, a: &Assistant) -> PathBuf {
    iwork_root.join(ASSISTANTS_DIR).join(&a.current.config.name)
}

/// 这个回合所在会话线的项目：项目对话是对话的项目，私聊是所选的项目（没选为 `None`）。
pub fn line_project(kind: &ConversationKind) -> Option<ProjectId> {
    match *kind {
        ConversationKind::Project { project_id } => Some(project_id),
        ConversationKind::AssistantDm {
            context_project, ..
        } => context_project,
    }
}

/// 一条会话的路径事实。唯一的构造处。
pub fn env_paths(
    env: &HostEnv,
    fs: &impl PathResolver,
    inputs: &SnapshotInputs,
    assistant: &Assistant,
) -> EnvPaths {
    let iwork_root = env.home.join(IWORK_ROOT);
    let project = line_project(&inputs.conversation.kind);
    let dm_private = matches!(
        inputs.conversation.kind,
        ConversationKind::AssistantDm { .. }
    )
    .then(|| dm_dir(&iwork_root, assistant));
    let workspace = project
        .and_then(|p| inputs.projects.iter().find(|x| x.id == p))
        .map(|p| p.root_path.clone())
        .or_else(|| dm_private.clone())
        .unwrap_or_else(|| iwork_root.clone());
    let mut tmp: Vec<CanonicalPath> = env.tmp_dirs.iter().map(|p| canonical(fs, p)).collect();
    tmp.sort_by(|a, b| a.0.cmp(&b.0));
    tmp.dedup();
    EnvPaths {
        platform: env.platform,
        home: canonical(fs, &env.home),
        data_dir: canonical(fs, &env.data_dir),
        iwork_root: canonical(fs, &iwork_root),
        workspace_root: canonical(fs, &workspace),
        tmp_dirs: tmp,
        dm_private_dir: dm_private.as_deref().map(|p| canonical(fs, p)),
        other_project_roots: inputs
            .projects
            .iter()
            .filter(|p| Some(p.id) != project)
            .map(|p| canonical(fs, &p.root_path))
            .collect(),
        other_dm_dirs: inputs
            .assistants
            .iter()
            .filter(|a| a.id != assistant.id)
            .map(|a| canonical(fs, &dm_dir(&iwork_root, a)))
            .collect(),
    }
}

/// 这个回合的有效档位（需求 FR11），从已提交的输入算，不读折叠。
pub fn mode_of(inputs: &SnapshotInputs, turn: &Turn) -> ApprovalMode {
    let project_default = |p: ProjectId| {
        inputs
            .projects
            .iter()
            .find(|x| x.id == p)
            .map(|x| x.settings.default_approval_mode)
    };
    let c = &inputs.conversation;
    let mode_inputs = match c.kind {
        ConversationKind::Project { project_id } => ModeInputs::ProjectConversation {
            member_override: inputs
                .member_modes
                .iter()
                .find(|(a, _)| *a == turn.assistant_id)
                .map(|(_, m)| *m),
            conversation_setting: c.approval_mode,
            project_default: project_default(project_id).unwrap_or_default(),
        },
        ConversationKind::AssistantDm {
            context_project, ..
        } => ModeInputs::AssistantDm {
            own_setting: c.approval_mode,
            selected_project_default: context_project.and_then(project_default),
        },
    };
    effective_mode(&mode_inputs)
}

/// 一次重算的结果。
pub struct Prepared {
    pub keys: SessionKeys,
    pub spec: SessionSpec,
    pub assistant_name: String,
    pub context_project_id: Option<ProjectId>,
    /// 和这条线绑着的那份比的键（D8：`lists_seq` 不比，别的逐项比）
    pub line_key: LineKey,
}

/// 会话线能不能沿用：这些都一样才沿用。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LineKey {
    snapshot: SessionSnapshot,
    keys_rest: String,
}

fn line_key(keys: &SessionKeys) -> LineKey {
    let mut snapshot = (*keys.snapshot).clone();
    // 记审计用的序号：清单没变时它也会随别的 rule 流事件前进
    snapshot.lists_seq = Default::default();
    LineKey {
        snapshot,
        keys_rest: format!(
            "{:?}|{:?}|{:?}|{:?}|{:?}|{:?}|{:?}|{:?}|{:?}|{:?}",
            keys.conversation,
            keys.assistant,
            keys.runtime_id,
            keys.project,
            keys.turn_kind,
            keys.tool_set,
            keys.disabled_groups,
            keys.cwd,
            keys.paths,
            keys.ai_review
        ),
    }
}

/// 算这个回合的会话快照与开会话的参数（D8）。路径解析在这里做（读磁盘），所以调用方把它放在读库之外。
pub fn prepare(
    env: &HostEnv,
    fs: &impl PathResolver,
    inputs: &SnapshotInputs,
    turn: &Turn,
) -> Result<Prepared, NotPrepared> {
    let assistant = inputs
        .assistants
        .iter()
        .find(|a| a.id == turn.assistant_id)
        .ok_or_else(|| format!("回合 {} 的助手 {} 不在", turn.id, turn.assistant_id))?;
    let project_id = line_project(&inputs.conversation.kind);
    let project: Option<&Project> =
        project_id.and_then(|p| inputs.projects.iter().find(|x| x.id == p));
    let paths = env_paths(env, fs, inputs, assistant);
    let scope = ScopeKeys {
        project: project_id,
        assistant: assistant.id,
        conversation: inputs.conversation.id,
    };
    let mode = mode_of(inputs, turn);
    let rule_paths = rule_paths_for(&inputs.security_lists, &scope, &paths, fs);
    let snapshot = snapshot_for(
        mode,
        &inputs.security_lists,
        &scope,
        &paths,
        inputs.lists_seq,
        &rule_paths,
    );
    let config = &assistant.current.config;
    let tool_set = tool_set_for(turn.kind, &config.disabled_groups);
    let ai_review: AiReviewSettings = project
        .map(|p| p.settings.ai_review.clone())
        .unwrap_or_default();
    let review_model = ai_review
        .model
        .clone()
        .unwrap_or_else(|| turn.model.clone());
    let spec = SessionSpec {
        cwd: paths.workspace_root.0.clone(),
        model: turn.model.clone(),
        system_prompt: config.prompt.clone(),
        // MCP 与插件归 T10f（本机导入）
        mcp: Vec::new(),
        constraints: snapshot.constraints.clone(),
        tool_set: tool_set.clone(),
        disabled_tools: tools_to_disable(&tool_set)
            .into_iter()
            .map(ToolRef)
            .collect(),
        plugin_dirs: Vec::new(),
        resume: None,
        kind: turn.kind,
    };
    let keys = SessionKeys {
        conversation: inputs.conversation.id,
        assistant: assistant.id,
        runtime_id: turn.runtime_id,
        assistant_version: turn.assistant_version,
        project: project_id,
        turn_kind: turn.kind,
        snapshot: Arc::new(snapshot),
        tool_set,
        disabled_groups: config.disabled_groups.clone(),
        cwd: paths.workspace_root.clone(),
        paths,
        ai_review,
        review_model,
    };
    Ok(Prepared {
        line_key: line_key(&keys),
        keys,
        spec,
        assistant_name: config.name.clone(),
        context_project_id: project_id,
    })
}
