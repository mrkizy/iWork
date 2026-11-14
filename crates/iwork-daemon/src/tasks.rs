//! 任务登记表里的各个长期任务（T04 方案 2.10）。每个任务每次启动都从真值重建自己的状态。

pub mod autostart_align;
pub mod engine_sync;
pub mod install_watch;
pub mod maintenance;
pub mod outbox;
pub mod publish_check;
#[cfg(unix)]
pub mod signals;
pub mod sleep_watch;
pub mod store_health;
