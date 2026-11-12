//! 开会话测试共用：照脚本起假 CLI，读它的记录。

use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::time::Duration;

use iwork_bridge_claude::launch::env::LaunchEnv;
use iwork_bridge_claude::launch::{BridgeEnv, Opening, Start};
use iwork_bridge_claude::setup::{AllowWrite, Constraints, Model, Network, Setup, Tools};
use iwork_bridge_claude::startup::{OpenError, OpenRequest, OpenSession, open};
use serde_json::{Value, json};

/// 一次开会话的现场：数据目录、假 CLI 的记录目录、工作目录。
pub struct World {
    pub data: tempfile::TempDir,
    pub record: tempfile::TempDir,
    pub work: tempfile::TempDir,
    pub script: Value,
    /// 交给 CLI 的环境里额外放的变量（P14：原样传过去）
    pub extra_env: Vec<(String, String)>,
    pub start: Start,
    pub startup_limit: Duration,
    pub hook_program: PathBuf,
}

pub const NEW_SESSION: &str = "11111111-2222-4333-8444-555555555555";

impl World {
    pub fn new(script: Value) -> Self {
        Self {
            data: tempfile::tempdir().unwrap(),
            record: tempfile::tempdir().unwrap(),
            work: tempfile::tempdir().unwrap(),
            script,
            extra_env: vec![("IWORK_TEST_PASSTHROUGH".into(), "kept".into())],
            start: Start::New {
                session_id: NEW_SESSION.into(),
            },
            startup_limit: Duration::from_secs(5),
            hook_program: super::hook_program().to_path_buf(),
        }
    }

    pub fn setup(&self) -> Setup {
        Setup {
            model: Model {
                id: "claude-haiku-5-5".into(),
                effort: "low",
            },
            constraints: Constraints {
                deny_read_write: vec![self.data.path().to_path_buf()],
                read_only: vec![],
                allow_write: AllowWrite::Whitelist(vec![self.work.path().to_path_buf()]),
                excluded_commands: vec![],
                allow_unsandboxed_retry: true,
                network: Network::Open,
            },
            tools: Tools::Default,
            disabled_tools: vec![],
            plugin_dirs: vec![],
            builtin_tool_allowlist: None,
        }
    }

    /// 桥接自己的环境：只放假 CLI 要的和测试指定的，`PATH` 让假 CLI 找得到 `sh`。
    pub fn inherited(&self) -> LaunchEnv {
        let mut vars = vec![
            ("PATH".to_owned(), "/usr/bin:/bin".to_owned()),
            (
                "IWORK_FAKE_CLAUDE_SCRIPT".to_owned(),
                self.script.to_string(),
            ),
            (
                "IWORK_FAKE_CLAUDE_RECORD".to_owned(),
                self.record.path().display().to_string(),
            ),
        ];
        vars.extend(self.extra_env.iter().cloned());
        LaunchEnv::from_vars(
            vars.into_iter()
                .map(|(k, v)| (OsString::from(k), OsString::from(v)))
                .collect(),
        )
    }

    pub async fn open(&self) -> Result<OpenSession, OpenError> {
        let setup = self.setup();
        let opening = Opening {
            cwd: self.work.path().to_path_buf(),
            system_prompt: String::new(),
            start: self.start.clone(),
        };
        let bridge = BridgeEnv {
            cli: super::fake_claude().to_path_buf(),
            hook_program: self.hook_program.display().to_string(),
            inherited: self.inherited(),
        };
        open(OpenRequest {
            data_dir: self.data.path(),
            setup: &setup,
            opening: &opening,
            bridge: &bridge,
            hook_timeout: Duration::from_secs(60),
            startup_limit: self.startup_limit,
        })
        .await
    }

    /// 假 CLI 的记录，按行。
    pub fn log(&self) -> Vec<Value> {
        read_log(self.record.path())
    }

    /// 等到记录里出现满足条件的一行（最多 10 秒）。
    pub async fn wait_log(&self, pred: impl Fn(&Value) -> bool) -> Value {
        for _ in 0..200 {
            if let Some(v) = self.log().into_iter().find(|v| pred(v)) {
                return v;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        panic!("10 秒内假 CLI 的记录里没出现要的那一行：{:#?}", self.log());
    }

    /// 数据目录下 `run/h` 里还剩几个会话目录。
    pub fn session_dirs(&self) -> usize {
        std::fs::read_dir(self.data.path().join("run").join("h"))
            .map(|d| d.count())
            .unwrap_or(0)
    }
}

pub fn read_log(record: &Path) -> Vec<Value> {
    std::fs::read_to_string(record.join("log.jsonl"))
        .unwrap_or_default()
        .lines()
        // 假 CLI 正在写的最后一行可能还不完整：只要解析得了的
        .filter_map(|l| serde_json::from_str(l).ok())
        .collect()
}

pub fn stdin_lines(log: &[Value]) -> Vec<Value> {
    log.iter()
        .filter(|v| v["kind"] == "stdin")
        .map(|v| serde_json::from_str(v["line"].as_str().unwrap()).unwrap())
        .collect()
}

/// 正常的开会话脚本：报到、读 initialize、答 `mode`，之后一直读 stdin。
pub fn happy_script(mode: Option<&str>) -> Value {
    json!([
        { "step": "session_start" },
        { "step": "read_line" },
        { "step": "answer_initialize", "mode": mode },
        { "step": "wait_stdin_eof" },
    ])
}
