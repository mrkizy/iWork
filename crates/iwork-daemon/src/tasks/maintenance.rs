//! 任务 `maintenance`（可重启）：每 10 分钟把 WAL 合回主库（`Busy` 就跳过）；启动 5 分钟后及此后每 24 小时回收制品
//! （T02 方案 2.10、S11）。没有要恢复的状态：计时从头算。

use std::time::Duration;

use iwork_ports::artifact::ArtifactStore;
use iwork_ports::store::StorageError;

use crate::boot::{Artifacts, Store};

pub const CHECKPOINT_EVERY: Duration = Duration::from_secs(10 * 60);
pub const FIRST_GC_AFTER: Duration = Duration::from_secs(5 * 60);
pub const GC_EVERY: Duration = Duration::from_secs(24 * 3600);

pub async fn run(store: std::sync::Arc<Store>, artifacts: Artifacts) {
    let mut checkpoint = tokio::time::interval_at(
        tokio::time::Instant::now() + CHECKPOINT_EVERY,
        CHECKPOINT_EVERY,
    );
    let mut gc = tokio::time::interval_at(tokio::time::Instant::now() + FIRST_GC_AFTER, GC_EVERY);
    loop {
        tokio::select! {
            _ = checkpoint.tick() => match store.maintenance().await {
                Ok(()) | Err(StorageError::Busy) => {}
                Err(e) => tracing::warn!(error = %e, "WAL 合回主库失败"),
            },
            _ = gc.tick() => match artifacts.collect_garbage().await {
                Ok(report) => tracing::info!(?report, "制品回收完成"),
                Err(e) => tracing::warn!(error = %e, "制品回收失败"),
            },
        }
    }
}
