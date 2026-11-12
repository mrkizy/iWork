//! 端到端：真实的 `iwork-agent-acp` 客户端 → 桥接进程（`acp` 子命令）→ 假 CLI → 真的 hook 程序。

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use iwork_agent_acp::{AcpRuntime, RuntimeLaunch};
use iwork_domain::common::{EffortLevel, ModelId, ModelSelection, ToolGroup};
use iwork_domain::session::{AllowWrite, NetworkPolicy, SessionConstraints, SessionToolSet};
use iwork_domain::turn::TurnKind;
use iwork_ports::agent::{
    AgentError, AgentPort, AgentSession, AgentUpdate, ContentBlock, PermissionDecision,
    PermissionHandler, PermissionRequest, SessionSpec, UpdateStream,
};
use serde_json::{Value, json};

pub const BRIDGE: &str = env!("CARGO_BIN_EXE_iwork-bridge-claude");
pub const MODEL: &str = "claude-haiku-5-5";

/// 一次端到端的现场。
pub struct Rig {
    pub data: tempfile::TempDir,
    pub record: tempfile::TempDir,
    pub work: tempfile::TempDir,
    pub launch: RuntimeLaunch,
}

impl Rig {
    pub fn new(script: Value) -> Self {
        // hook 程序必须在桥接同一目录下（方案 3.2）：两者都在这个 target 目录里，先按当前源码构建出来
        let hook = super::hook_program();
        assert_eq!(
            hook.parent(),
            Path::new(BRIDGE).parent(),
            "hook 与桥接在同一个目录"
        );
        let data = tempfile::tempdir().unwrap();
        let record = tempfile::tempdir().unwrap();
        let work = tempfile::tempdir().unwrap();
        let launch = RuntimeLaunch {
            program: PathBuf::from(BRIDGE),
            args: vec!["acp".into()],
            env: vec![
                ("IWORK_DATA_DIR".into(), data.path().into()),
                ("IWORK_CLAUDE_CLI".into(), super::fake_claude().into()),
                ("IWORK_FAKE_CLAUDE_SCRIPT".into(), script.to_string().into()),
                ("IWORK_FAKE_CLAUDE_RECORD".into(), record.path().into()),
            ],
        };
        Self {
            data,
            record,
            work,
            launch,
        }
    }

    pub fn spec(&self) -> SessionSpec {
        SessionSpec {
            cwd: self.work.path().to_path_buf(),
            model: ModelSelection {
                model_id: ModelId(MODEL.into()),
                effort: Some(EffortLevel("low".into())),
            },
            system_prompt: "你是测试助手".into(),
            mcp: Vec::new(),
            constraints: SessionConstraints {
                deny_read_write: vec![self.data.path().to_path_buf()],
                read_only: Vec::new(),
                allow_write: AllowWrite::Whitelist(vec![self.work.path().to_path_buf()]),
                excluded_commands: Vec::new(),
                allow_unsandboxed_retry: true,
                network: NetworkPolicy::Open,
            },
            tool_set: SessionToolSet {
                enabled_groups: BTreeSet::from([ToolGroup::ReadAndSearch, ToolGroup::RunCommands]),
                include_always_on: true,
            },
            disabled_tools: Vec::new(),
            plugin_dirs: Vec::new(),
            resume: None,
            kind: TurnKind::Assistant,
        }
    }

    pub async fn open<H: PermissionHandler + 'static>(
        &self,
        spec: SessionSpec,
        handler: H,
    ) -> Result<impl AgentSession, AgentError> {
        AcpRuntime::new(self.launch.clone())
            .open_session(spec, handler)
            .await
    }

    pub fn log(&self) -> Vec<Value> {
        super::startup::read_log(self.record.path())
    }
}

/// 发一条文字消息，收完这一轮的全部更新。
pub async fn turn(session: &impl AgentSession, text: &str) -> Result<Vec<AgentUpdate>, AgentError> {
    let mut updates = session
        .prompt(vec![ContentBlock::Text(text.into())])
        .await?;
    let mut all = Vec::new();
    while let Some(u) = tokio::time::timeout(std::time::Duration::from_secs(30), updates.next())
        .await
        .expect("30 秒内有更新或结束")
    {
        all.push(u);
    }
    Ok(all)
}

/// 照答复表答权限请求，记下收到的请求。
#[derive(Default)]
pub struct Answers {
    pub decision: Mutex<Option<PermissionDecision>>,
    pub seen: Mutex<Vec<PermissionRequest>>,
}

pub struct Handler(pub std::sync::Arc<Answers>);

impl PermissionHandler for Handler {
    async fn on_permission(&self, req: PermissionRequest) -> PermissionDecision {
        self.0.seen.lock().unwrap().push(req);
        self.0
            .decision
            .lock()
            .unwrap()
            .clone()
            .unwrap_or(PermissionDecision::Allow)
    }
}

// ---------- 假 CLI 的脚本片段 ----------

/// 开会话：报到、答 initialize。
pub fn opening() -> Vec<Value> {
    vec![
        json!({ "step": "session_start" }),
        json!({ "step": "read_line" }),
        json!({ "step": "answer_initialize", "mode": "dontAsk" }),
    ]
}

pub fn emit(line: Value) -> Value {
    json!({ "step": "emit", "line": line })
}

/// 这一轮开头的 `system/init`（`plugins` 只有 CLI 自带的一个）。
pub fn init() -> Value {
    init_with(
        json!([{ "name": "builtin-a", "path": "builtin-a", "source": "builtin-a@builtin" }]),
        None,
    )
}

pub fn init_with(plugins: Value, memory_auto: Option<&str>) -> Value {
    let mut v = json!({
        "type": "system", "subtype": "init", "session_id": "11111111-2222-4333-8444-555555555555",
        "model": MODEL, "permissionMode": "dontAsk", "tools": ["Bash", "Read"], "plugins": plugins,
    });
    if let Some(m) = memory_auto {
        v["memory_paths"] = json!({ "auto": m });
    }
    emit(v)
}

pub fn text(id: &str, t: &str) -> Value {
    emit(
        json!({ "type": "assistant", "message": { "id": id, "content": [{ "type": "text", "text": t }] } }),
    )
}

pub fn tool_use(id: &str, name: &str, input: Value) -> Value {
    emit(
        json!({ "type": "assistant", "message": { "id": format!("m-{id}"),
        "content": [{ "type": "tool_use", "id": id, "name": name, "input": input }] } }),
    )
}

pub fn tool_result(id: &str, ok: bool, out: &str) -> Value {
    emit(json!({ "type": "user", "message": { "content": [
        { "type": "tool_result", "tool_use_id": id, "is_error": !ok, "content": out } ] } }))
}

pub fn result_ok() -> Value {
    result(json!({ "subtype": "success", "is_error": false }))
}

pub fn result(mut v: Value) -> Value {
    v["type"] = json!("result");
    v["usage"] = json!({ "input_tokens": 3, "output_tokens": 4 });
    emit(v)
}

pub fn read_user() -> Value {
    json!({ "step": "read_line" })
}

pub fn wait_eof() -> Value {
    json!({ "step": "wait_stdin_eof" })
}

pub fn ends(updates: &[AgentUpdate]) -> Vec<&AgentUpdate> {
    updates
        .iter()
        .filter(|u| matches!(u, AgentUpdate::Finished(_) | AgentUpdate::Failed(_)))
        .collect()
}
