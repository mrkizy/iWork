//! 启动表第 9 步里各参与方的先后（T10a 方案 2.8）：都在装好提交观察者之后、`bind` 之前。
//!
//! 0. 存储已包成带封口闸的那一个（`daemon.rs`），这里拿到的就是它。
//! - 第 1 步：探测运行时，提交运行时那一行（探测不到也提交：状态 `Missing`、路径为空，「找不到运行时」的占位）。
//! - 第 1a 步：登记内置助手 iWork（库里没有 `builtin` 为真的助手时）。
//! - 第 2 步：组装运行时（`AcpRuntime`），`env` 给 `IWORK_DATA_DIR`、`IWORK_CLAUDE_CLI`；探测没有路径就不组装。
//! - 第 3、4、5 步：`ApprovalEngine::start`；提交引擎返回的恢复计划；`TurnHost::start`。
//!
//! 运行时种类的名字只在这一层出现（领域层不出现厂商名）。

use std::collections::{BTreeMap, BTreeSet};
use std::ffi::OsString;
use std::path::PathBuf;
use std::sync::Arc;

use iwork_agent_acp::{AcpRuntime, AcpSession, RuntimeLaunch};
use iwork_core::approval::engine::{
    ApprovalEngine, ENGINE_ACTOR, EngineError, EnginePorts, SessionKeys,
};
use iwork_domain::assistant::{Assistant, AssistantVersion, builtin_iwork_config};
use iwork_domain::common::Actor;
use iwork_domain::event::{AssistantCreated, RuntimeUpdated};
use iwork_domain::id::{AssistantId, CommandId, ConversationId, RuntimeId, TurnId};
use iwork_domain::runtime::{Runtime, RuntimeKind, RuntimeState};
use iwork_ports::agent::{
    AgentError, AgentPort, PermissionHandler, RuntimeCapabilities, SessionSpec,
};
use iwork_ports::clock::{Clock, IdGenerator, Timer};
use iwork_ports::discovery::{DiscoveryError, RuntimeDiscovery, RuntimeProbe};
use iwork_ports::fs::PathResolver;
use iwork_ports::read_model::{PendingItem, ReadModelPort};
use iwork_ports::store::{CommitRequest, StorageError, StoragePort};

use crate::host::ports::HostEnv;
use crate::host::prepare::prepare;

/// M1 唯一的运行时的种类名（适配层的名字）。
pub const RUNTIME_KIND: &str = "claude-code";
pub const RUNTIME_DISPLAY_NAME: &str = "Claude Code";
/// 桥接程序的名字（文件名另加平台的可执行后缀，`install::program_file`）。
pub const BRIDGE_NAME: &str = "iwork-bridge-claude";
/// 给桥接的两个环境变量（T06 方案 3.1、第 19 节）：都是绝对路径。
pub const ENV_DATA_DIR: &str = "IWORK_DATA_DIR";
pub const ENV_CLAUDE_CLI: &str = "IWORK_CLAUDE_CLI";
/// 内置助手登记的 actor 与 `command_id` 的名字空间（2.8 第 1a 步）。
pub const BOOTSTRAP_ACTOR: &str = "system:bootstrap";
pub const BUILTIN_ASSISTANT_NAMESPACE: &str = "builtin-assistant";

/// T06 的 S8（探测与版本策略）合入之前的占位：什么都探测不到（supervisor 2026-11-13 定）。
/// 运行时那一行照样提交成 `Missing`，回合派发时以「运行时缺失」失败。S8 合入时组装处换一行。
#[derive(Debug, Clone, Copy, Default)]
pub struct NothingFound;

impl RuntimeDiscovery for NothingFound {
    async fn discover(&self, _kind: &RuntimeKind) -> Result<RuntimeProbe, DiscoveryError> {
        Ok(RuntimeProbe {
            cli_path: None,
            cli_version: None,
            login: None,
        })
    }
}

/// 桥接程序的位置：和后台程序同一个目录（发布包里两者放在一起；开发构建是 `target/<profile>/`）。
pub fn bridge_program(daemon_exe: &std::path::Path) -> PathBuf {
    daemon_exe
        .parent()
        .unwrap_or_else(|| std::path::Path::new("."))
        .join(crate::install::program_file(BRIDGE_NAME))
}

/// 启动命令行：程序是桥接，环境给数据目录与探测到的 CLI 的绝对路径。探测没有路径返回 `None`（不组装）。
pub fn runtime_launch(
    bridge: PathBuf,
    data_dir: &std::path::Path,
    probe: &RuntimeProbe,
) -> Option<RuntimeLaunch> {
    let cli = probe.cli_path.as_ref()?;
    Some(RuntimeLaunch {
        program: bridge,
        args: Vec::new(),
        env: vec![
            (
                OsString::from(ENV_DATA_DIR),
                data_dir.as_os_str().to_owned(),
            ),
            (OsString::from(ENV_CLAUDE_CLI), cli.as_os_str().to_owned()),
        ],
    })
}

/// 运行时插槽：探测到了就是组装好的运行时，没探测到开会话报「找不到运行时程序」（编排把回合收成失败）。
#[derive(Debug, Clone)]
pub struct RuntimeSlot(pub Option<AcpRuntime>);

impl AgentPort for RuntimeSlot {
    type Session = AcpSession;

    async fn capabilities(&self) -> Result<RuntimeCapabilities, AgentError> {
        match &self.0 {
            Some(r) => r.capabilities().await,
            None => Err(AgentError::RuntimeNotFound),
        }
    }

    async fn open_session<H: PermissionHandler + 'static>(
        &self,
        spec: SessionSpec,
        on_permission: H,
    ) -> Result<AcpSession, AgentError> {
        match &self.0 {
            Some(r) => r.open_session(spec, on_permission).await,
            None => Err(AgentError::RuntimeNotFound),
        }
    }
}

/// 第 1 步：运行时那一行（纯函数）。没探测到：状态 `Missing`、路径为空。验证状态归 S8，这里只分有没有。
pub fn runtime_row(probe: &RuntimeProbe, adapter_version: &str) -> Runtime {
    Runtime {
        id: RuntimeId::DEFAULT_RUNTIME,
        kind: RuntimeKind(RUNTIME_KIND.into()),
        display_name: RUNTIME_DISPLAY_NAME.into(),
        cli_path: probe.cli_path.clone(),
        cli_version: probe.cli_version.clone(),
        adapter_version: adapter_version.into(),
        state: match (&probe.cli_path, &probe.login) {
            (None, _) => RuntimeState::Missing,
            (Some(_), Some(l)) if !l.logged_in => RuntimeState::NotLoggedIn,
            (Some(_), _) => RuntimeState::ReadyUnverified,
        },
        login: probe.login.clone(),
    }
}

/// 第 1 步：库里没有这一行、或有变化时提交。
pub async fn commit_runtime_row<S: StoragePort, R: ReadModelPort>(
    store: &S,
    read: &R,
    ids: &impl IdGenerator,
    row: Runtime,
) -> Result<(), StorageError> {
    let existing = read.runtimes().await?;
    if existing.contains(&row) {
        return Ok(());
    }
    store
        .commit(CommitRequest {
            command_id: CommandId::from_raw(ids.next()),
            actor: Actor(BOOTSTRAP_ACTOR.into()),
            events: vec![RuntimeUpdated { runtime: row }.into()],
            outbox: Vec::new(),
        })
        .await?;
    Ok(())
}

/// 第 1a 步：库里没有 `builtin` 为真的助手就登记内置助手 iWork（R8）。判「有没有」看标记不看编号；
/// `command_id` 由固定编号算出，落过盘再提交是 `replayed`。用户改过的不动。
pub async fn register_builtin_assistant<S: StoragePort, R: ReadModelPort>(
    store: &S,
    read: &R,
    clock: &impl Clock,
) -> Result<(), StorageError> {
    if read.assistants().await?.iter().any(|a| a.builtin) {
        return Ok(());
    }
    let id = AssistantId::BUILTIN_IWORK;
    store
        .commit(CommitRequest {
            command_id: CommandId::derived(BUILTIN_ASSISTANT_NAMESPACE, id.to_raw()),
            actor: Actor(BOOTSTRAP_ACTOR.into()),
            events: vec![
                AssistantCreated {
                    assistant: Assistant {
                        id,
                        builtin: true,
                        current: AssistantVersion {
                            version: 1,
                            config: builtin_iwork_config(),
                            created_at: clock.now(),
                        },
                    },
                }
                .into(),
            ],
            outbox: Vec::new(),
        })
        .await?;
    Ok(())
}

/// 第 3 步要的 `keys_for`：先读一遍待审批所在回合的对话与助手、算好各自的会话快照放进一张表，闭包只查表；
/// 算不出来的给 `None`（引擎对它们只许拒绝与作废，T07 6.10.5）。
pub async fn keys_for_pending<R: ReadModelPort>(
    read: &R,
    env: &HostEnv,
    fs: &impl PathResolver,
) -> Result<BTreeMap<(ConversationId, TurnId), SessionKeys>, StorageError> {
    let mut wanted: BTreeSet<TurnId> = BTreeSet::new();
    for item in read.pending_items().await? {
        if let PendingItem::Approval(a) = item {
            wanted.insert(a.turn_id);
        }
    }
    let mut keys = BTreeMap::new();
    for turn_id in wanted {
        let Some((turn, conversation)) = read.turn_with_conversation(turn_id).await? else {
            continue;
        };
        let inputs = match read.snapshot_inputs(conversation.id).await {
            Ok(i) => i,
            Err(StorageError::NotFound { .. }) => continue,
            Err(e) => return Err(e),
        };
        match prepare(env, fs, &inputs, &turn) {
            Ok(p) => {
                keys.insert((conversation.id, turn_id), p.keys);
            }
            Err(why) => {
                tracing::warn!(turn = %turn_id, %why, "算不出待审批所在回合的会话快照，它只能拒绝或作废")
            }
        }
    }
    Ok(keys)
}

/// 第 3、4 步：引擎折叠到位，恢复计划一笔提交。读到不认识的事件种类原样传上去（归退出码 12）。
pub async fn start_engine<C, G, S, A, T, R>(
    ports: EnginePorts<C, G, S, A, T, R>,
    keys: BTreeMap<(ConversationId, TurnId), SessionKeys>,
    store: &impl StoragePort,
    ids: &impl IdGenerator,
) -> Result<Arc<ApprovalEngine<C, G, S, A, T, R>>, EngineError>
where
    C: Clock + 'static,
    G: IdGenerator + 'static,
    S: StoragePort + 'static,
    A: AgentPort + 'static,
    T: Timer + 'static,
    R: PathResolver + 'static,
{
    let (engine, plan) =
        ApprovalEngine::start(ports, BTreeSet::new(), |c, t| keys.get(&(c, t)).cloned()).await?;
    if !plan.is_empty() {
        store
            .commit(CommitRequest {
                command_id: CommandId::from_raw(ids.next()),
                actor: Actor(ENGINE_ACTOR.into()),
                events: plan,
                outbox: Vec::new(),
            })
            .await?;
    }
    Ok(engine)
}

#[cfg(test)]
#[path = "assembly_tests.rs"]
mod tests;
