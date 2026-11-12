//! 任务 `store-health`（致命）：等存储的失效信号（方案 2.10、S5）。失效后任务结束，监督器按致命任务整体退出，
//! 停机时跳过关存储。

use std::future::Future;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use iwork_store::StoreFailure;

/// `failed` 是存储的失效信号（`SqliteStore::failed()`）。
pub async fn run(failed: impl Future<Output = StoreFailure>, store_failed: Arc<AtomicBool>) {
    let failure = failed.await;
    store_failed.store(true, Ordering::SeqCst);
    tracing::error!(?failure, "存储不可用，整体退出");
}
