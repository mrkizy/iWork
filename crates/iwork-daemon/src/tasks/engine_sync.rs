//! `engine-sync`（T07 方案 6.10.8；T10a D9）：提交观察者里只叫醒，这个任务调 `engine.sync()`（追平 + 清孤儿），
//! 完了叫醒回合宿主（D7 ① 要读追平后的 `turns_to_resume`）。
//!
//! 可重启：它没有自己的状态，`sync()` 只看已提交的事件，可以随时丢弃。重起后第一件事是追平一次。出错、到时都不自己
//! 重试，下一笔提交再叫醒；没清完的孤儿留到下一次 `commit_with` 或重启。

use std::sync::Arc;
use std::time::Duration;

use tokio::sync::Notify;

use crate::host::ports::HostEngine;

/// 一次 `sync()` 最多等这么久，到时丢掉这一次。
pub const ENGINE_SYNC_LIMIT: Duration = Duration::from_secs(5);

pub async fn run<E: HostEngine>(engine: E, wake: Arc<Notify>, after_sync: impl Fn() + Send) {
    loop {
        match tokio::time::timeout(ENGINE_SYNC_LIMIT, engine.sync()).await {
            Ok(Ok(())) => {}
            Ok(Err(error)) => tracing::warn!(%error, "引擎追平失败，等下一笔提交再叫醒"),
            Err(_) => tracing::warn!("引擎追平 5 秒没完，丢掉这一次"),
        }
        after_sync();
        wake.notified().await;
    }
}
