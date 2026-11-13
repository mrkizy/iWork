//! Claude Code 桥接：ACP agent 端 ↔ Claude Code CLI（`-p` + stream-json + hook）（T06 方案 2.2）。
//!
//! `setup`、`launch`（除 `session_dir`）、`deadlines`、`cli`（除 `process`）是纯函数，单测与快照测试直接测；
//! `hook_server`、`cli::process`、`startup` 是薄壳，用真的 hook 程序和假 CLI（`fake-cli` feature）测。`acp` 是 ACP 端的薄壳。

/// ACP agent 端（S6-3）
#[cfg(unix)]
pub mod acp;
pub mod cli;
pub mod deadlines;
/// 不等待的诊断输出（写 stderr 经队列和专门的写线程）
#[cfg(unix)]
pub mod diag;
/// hook 通道的服务端。M1 不支持 Windows（AGENTS.md），命名管道那一半不做
#[cfg(unix)]
pub mod hook_server;
pub mod launch;
/// 收尾线程（5.4a 第 5 条第二种），和 ACP 端一样只在 unix 上
#[cfg(unix)]
pub mod reaper;
pub mod rewrite;
pub mod setup;
/// 开会话（3.5 的次序），和 hook 通道一样只在 unix 上
#[cfg(unix)]
pub mod startup;
pub mod turn_gate;
