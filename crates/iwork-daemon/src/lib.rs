//! 后台服务（T04）：唯一同时知道全部实现的 crate，在这里把实现注入端口。
//!
//! 入口是 [`boot::run`]：解析参数 → 启动表（[`boot`]）→ 运行 → 退出状态（[`exit::process_status`]）。

pub mod args;
pub mod assembly;
pub mod boot;
#[cfg(unix)]
mod daemon;
pub mod dev;
#[cfg(all(unix, feature = "dev"))]
mod dev_parts;
pub mod exit;
pub mod gate;
pub mod handler;
pub mod host;
pub mod install;
pub mod instance;
pub mod logging;
pub mod publish;
pub mod recovery;
pub mod source;
pub mod status;
pub mod stop;
pub mod supervisor;
pub mod tasks;
pub mod time;
pub mod work;

pub use boot::run;

// 存储的测试夹具（实体构造、假时钟、逐表导出），与 iwork-store 的测试共用一份
#[cfg(test)]
#[path = "../../iwork-store/tests/support/fixtures.rs"]
mod fixtures;
