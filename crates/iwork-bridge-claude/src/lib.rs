//! Claude Code 桥接：ACP agent 端 ↔ Claude Code CLI（`-p` + stream-json + hook）（T06 方案 2.2）。
//!
//! `setup`、`launch`（除 `session_dir`）、`deadlines` 是纯函数，单测与快照测试直接测；进程与通道的薄壳在 S6。

pub mod deadlines;
pub mod launch;
pub mod setup;
