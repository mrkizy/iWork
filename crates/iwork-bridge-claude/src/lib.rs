//! Claude Code 桥接：ACP agent 端 ↔ Claude Code CLI（`-p` + stream-json + hook）（T06 方案 2.2）。
//!
//! `setup`、`launch`（除 `session_dir`）、`deadlines`、`cli` 是纯函数，单测与快照测试直接测；`hook_server` 是通道的薄壳；ACP 端与 CLI 进程在 S6 的后几批。

pub mod cli;
pub mod deadlines;
/// hook 通道的服务端。M1 不支持 Windows（AGENTS.md），命名管道那一半不做
#[cfg(unix)]
pub mod hook_server;
pub mod launch;
pub mod setup;
