//! 后台服务（T04）：唯一同时知道全部实现的 crate，在这里把实现注入端口。
//!
//! 入口是 [`boot::run`]：解析参数 → 启动表（[`boot`]）→ 运行 → 退出状态（[`exit::process_status`]）。

pub mod args;
pub mod boot;
#[cfg(unix)]
mod daemon;
pub mod dev;
pub mod exit;
pub mod handler;
pub mod instance;
pub mod logging;
pub mod source;
pub mod status;
pub mod time;

pub use boot::run;
