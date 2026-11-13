//! 只在测试构建里有的观测与控制点：本 crate 的单测（`cfg(test)`）与打开 `test-hooks` feature 的测试
//! （后台的测试经 dev-dependency 打开）。发布构建里没有这个模块，`SqliteStore` 上也没有这几个方法。
//!
//! 用途是让交错测试用屏障和通知定先后，不靠 `sleep`（T10a 方案 7.2a 表下「实施这张表时的四条」）：
//! - [`SqliteStore::park_writer`]：压住写线程，之后入队的写操作都排在它后面；
//! - [`SqliteStore::on_receipt_enqueued`]：查回执这一项**成功入队之后**发通知（不是 future 返回之后）。

use std::sync::mpsc::{Receiver, SyncSender};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use iwork_domain::id::CommandId;
use iwork_ports::store::{EventCodec, StorageError};

use crate::store::SqliteStore;
use crate::writer::WriteOp;

type ReceiptHook = Arc<dyn Fn(CommandId) + Send + Sync>;

#[derive(Default)]
pub(crate) struct TestHooks {
    receipt_enqueued: Mutex<Option<ReceiptHook>>,
}

impl TestHooks {
    pub(crate) fn receipt_enqueued(&self, id: CommandId) {
        let hook = self
            .receipt_enqueued
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone();
        if let Some(hook) = hook {
            hook(id);
        }
    }
}

/// 压住写线程的把手。[`Self::wait_parked`] 等到写线程确实停在这里；放开用 [`Self::release`]，
/// 把手被丢掉也放开（测试 panic 时不把写线程永远压着）。
pub struct WriterPark {
    release: SyncSender<()>,
    parked: Receiver<()>,
}

impl WriterPark {
    /// 等写线程走到停靠点。`limit` 内没到 → panic 并点名（测试布置错了）。
    pub fn wait_parked(&self, limit: Duration) {
        self.parked
            .recv_timeout(limit)
            .unwrap_or_else(|e| panic!("写线程 {limit:?} 内没停到停靠点：{e}"));
    }

    pub fn release(self) {
        let _ = self.release.send(());
    }
}

impl<C: EventCodec + 'static> SqliteStore<C> {
    /// 压住写线程：入队一个独占操作，它停在那里直到把手放开。
    pub fn park_writer(&self) -> Result<WriterPark, StorageError> {
        let (release, wait_release) = std::sync::mpsc::sync_channel::<()>(1);
        let (signal_parked, parked) = std::sync::mpsc::sync_channel::<()>(1);
        self.writer
            .submit(WriteOp::Exclusive(Box::new(move |_conn, _now| {
                let _ = signal_parked.send(());
                // 放开或把手被丢掉（发送端没了）都往下走
                let _ = wait_release.recv();
                Box::new(|_cleanup| {})
            })))?;
        Ok(WriterPark { release, parked })
    }

    /// 查回执这一项成功入队之后调 `f`（带它查的 `command_id`）。只装一个，后装的替换先装的。
    pub fn on_receipt_enqueued(&self, f: impl Fn(CommandId) + Send + Sync + 'static) {
        *self
            .hooks
            .receipt_enqueued
            .lock()
            .unwrap_or_else(PoisonError::into_inner) = Some(Arc::new(f));
    }
}
