//! Claude Code PreToolUse hook 小程序与它和桥接之间的线格式（T06 方案 2.3、2.4）。
//!
//! lib 部分供 `iwork-bridge-claude` 共用线格式与对端凭据；不得依赖 tokio（冷启动体积计入基准）。
//! hook 只在收到一条格式正确、来自已校验对端的「允许」时输出允许，别的任何情况都输出拒绝。
//! 它管不到自己没跑起来、被杀、整个没被调用的情况：那些靠「执行前失败即关闭」的硬前提（方案 3.4）。

pub mod client;
pub mod deadline;
pub mod output;
pub mod peer;
pub mod verdict;
pub mod wire;
