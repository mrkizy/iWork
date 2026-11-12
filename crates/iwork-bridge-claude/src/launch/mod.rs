//! 起 CLI 要用的一切：参数、设置文件、环境变量（T06 方案 3.1–3.3）。[`build_launch`] 是纯函数：
//! 桥接拿到什么约束就翻译什么，不另算一份（T07 6.9）；快照测试锁住它的产物（`tests/launch_snapshot.rs`）。

pub mod args;
pub mod session_dir;
pub mod settings;

use std::path::PathBuf;
use std::time::Duration;

use iwork_claude_hook::client::{ENV_BRIDGE_PID, ENV_DEADLINE, ENV_SOCK, ENV_TOKEN};
use serde_json::Value;

use crate::setup::{Setup, SetupInvalid};

/// 桥接这个进程的事实，和会话无关。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BridgeEnv {
    /// `claude` 可执行文件
    pub cli: PathBuf,
    /// hook 程序的绝对路径（桥接所在目录下的 `iwork-claude-hook`）。要写进设置文件的 shell 命令，
    /// 所以是字符串：不是 UTF-8 的路径在建 `BridgeEnv` 时就拒了
    pub hook_program: String,
}

/// 这条会话的 hook 通道：会话目录与交给 hook 的四个环境变量的值。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Channel {
    /// 会话目录（`session_dir`），socket、设置文件、MCP 配置都在里面
    pub dir: PathBuf,
    /// 64 位小写十六进制的令牌
    pub token_hex: String,
    pub bridge_pid: u32,
    /// hook 自己的期限（`deadlines().hook`）
    pub hook_deadline: Duration,
}

/// ACP 请求给的、`SessionSetup` 之外的东西。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Opening {
    pub cwd: PathBuf,
    /// ACP `systemPrompt` 的文本；为空不传
    pub system_prompt: String,
    pub start: Start,
}

/// 新开还是续接。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Start {
    /// `session/new`：桥接生成的 UUID，经 `--session-id` 交给 CLI，当场就能回会话 id
    New { session_id: String },
    /// `session/load`：要续接的会话 id
    Resume { session_id: String },
}

/// 起 CLI 的完整计划。设置文件与 MCP 配置由薄壳写进会话目录（0600）。
#[derive(Debug, Clone, PartialEq)]
pub struct LaunchPlan {
    pub program: PathBuf,
    pub args: Vec<String>,
    pub cwd: PathBuf,
    /// 在继承桥接自己的环境之上再加的这几个（3.1 末段）
    pub env: Vec<(&'static str, String)>,
    pub settings: Value,
    pub mcp: Value,
}

/// 会话配置 + 请求 + 进程事实 + 通道 → 起 CLI 的计划。
pub fn build_launch(
    setup: &Setup,
    opening: &Opening,
    env: &BridgeEnv,
    channel: &Channel,
) -> Result<LaunchPlan, SetupInvalid> {
    if !opening.cwd.is_absolute() {
        return Err(SetupInvalid { field: "cwd" });
    }
    let files = session_dir::Files::in_dir(&channel.dir);
    Ok(LaunchPlan {
        program: env.cli.clone(),
        args: args::cli_args(setup, opening, &files),
        cwd: opening.cwd.clone(),
        env: vec![
            (ENV_SOCK, files.socket.display().to_string()),
            (ENV_TOKEN, channel.token_hex.clone()),
            (ENV_BRIDGE_PID, channel.bridge_pid.to_string()),
            (ENV_DEADLINE, channel.hook_deadline.as_secs().to_string()),
        ],
        settings: settings::settings_json(&setup.constraints, &env.hook_program),
        mcp: settings::mcp_json(),
    })
}
