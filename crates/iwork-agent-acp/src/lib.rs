//! 适配层：ACP 客户端端，实现 `AgentPort`（技术设计 6.1、6.5）。
//!
//! 后台服务只经这里和任何运行时说 ACP：每条会话起一个运行时进程（桥接或原生 ACP 适配器），
//! 进程树由 [`process`] 监管；运行时的原始更新在 [`convert`] 里标准化成 `AgentUpdate`，
//! 原始消息只进 [`diagnostics`]（脱敏、限量）；版本登记与不兼容组合在 [`registry`]。

mod client;
pub mod convert;
pub mod diagnostics;
mod frame;
pub mod process;
pub mod registry;
mod turns;

pub use client::{AcpRuntime, AcpSession, AcpUpdates, MAX_FRAME_BYTES, MCP_UNSUPPORTED};
pub use process::{ResourceSample, RuntimeLaunch};
