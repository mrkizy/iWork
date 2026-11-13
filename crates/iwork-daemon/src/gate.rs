//! 封口闸与带闸的存储（T10a 方案 E4、2.8 第 0 步）。
//!
//! 封口之后全后台没有新入队的提交。闸放在存储端口的入队点上，不放在各个调用点上：组装处把存储包成
//! [`GatedStore`]，对外只给出这一个（受理函数、引擎、宿主、会话线、各任务、outbox 投递）。裸的只留给
//! 组装处自己、关存储与启动时的恢复。
//!
//! 每一笔提交和封口只有两种先后：`commit` 在闸的锁里看封没封口，没封就**在同一个临界区里**调真存储的
//! `commit`（那一下同步入队，`StoragePort::commit` 的契约），出锁后在锁外等回执；封了就不入队、回 `Busy`。
//! [`SealGate::seal`] 用同一把锁，所以它要么等正在入队的那一笔出了临界区，要么先到、让后来的入不了队。
//! 封口前已经入队的那一笔不撤，它记的是封口前就成立的事实。读不过闸。

use std::future::Future;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

use futures::future::Either;
use iwork_domain::id::{CommandId, MessageId, Seq};
use iwork_ports::store::{
    CommitReceipt, CommitRequest, DeliveryState, EventFilter, OutboxId, OutboxItem, OutboxSelector,
    RebuildReport, RequeueReport, StorageError, StoragePort, StoredEvent,
};

/// 封口闸：一把小锁里一个布尔值。`true` = 已封口，只会从假变真。
#[derive(Default)]
pub struct SealGate {
    sealed: Mutex<bool>,
    /// 测试的可观测点：一笔提交过了闸、入了队（在临界区里调用）。发布构建里没有
    #[cfg(any(test, feature = "test-hooks"))]
    on_enqueued: Mutex<Option<Arc<dyn Fn(CommandId) + Send + Sync>>>,
}

impl SealGate {
    fn lock(&self) -> MutexGuard<'_, bool> {
        self.sealed.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// 测试用：每一笔过了闸、入了队的提交调一次 `f`（带它的 `command_id`）。后装的替换先装的。
    #[cfg(any(test, feature = "test-hooks"))]
    pub fn on_enqueued(&self, f: impl Fn(CommandId) + Send + Sync + 'static) {
        *self
            .on_enqueued
            .lock()
            .unwrap_or_else(PoisonError::into_inner) = Some(Arc::new(f));
    }

    /// 封口。正有一笔在临界区里入队时，等它出来再返回。
    pub fn seal(&self) {
        *self.lock() = true;
    }

    pub fn is_sealed(&self) -> bool {
        *self.lock()
    }
}

/// 带封口闸的存储。只有 `commit` 过闸，别的方法原样转给真存储。
pub struct GatedStore<S> {
    inner: Arc<S>,
    gate: Arc<SealGate>,
}

impl<S> Clone for GatedStore<S> {
    fn clone(&self) -> Self {
        Self {
            inner: self.inner.clone(),
            gate: self.gate.clone(),
        }
    }
}

impl<S> GatedStore<S> {
    pub fn new(inner: Arc<S>, gate: Arc<SealGate>) -> Self {
        Self { inner, gate }
    }

    pub fn gate(&self) -> &Arc<SealGate> {
        &self.gate
    }
}

impl<S: StoragePort> StoragePort for GatedStore<S> {
    // 不能写成 async fn：入队必须发生在调用的这一下、在锁里，不能拖到返回的 future 第一次被推进时
    fn commit(
        &self,
        req: CommitRequest,
    ) -> impl Future<Output = Result<CommitReceipt, StorageError>> + Send {
        let sealed = self.gate.lock();
        if *sealed {
            return Either::Left(std::future::ready(Err(StorageError::Busy)));
        }
        #[cfg(any(test, feature = "test-hooks"))]
        let command_id = req.command_id;
        // 真存储在这一下同步入队（契约），所以出锁时入没入队已经定了
        let receipt = self.inner.commit(req);
        #[cfg(any(test, feature = "test-hooks"))]
        if let Some(f) = self
            .gate
            .on_enqueued
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
        {
            f(command_id);
        }
        drop(sealed);
        Either::Right(receipt)
    }

    fn command_receipt(
        &self,
        command_id: CommandId,
    ) -> impl Future<Output = Result<Option<CommitReceipt>, StorageError>> + Send {
        self.inner.command_receipt(command_id)
    }

    fn head_seq(&self) -> impl Future<Output = Result<Seq, StorageError>> + Send {
        self.inner.head_seq()
    }

    fn read_events(
        &self,
        filter: EventFilter,
        from: Seq,
        limit: usize,
    ) -> impl Future<Output = Result<Vec<StoredEvent>, StorageError>> + Send {
        self.inner.read_events(filter, from, limit)
    }

    fn checkpoint_message(
        &self,
        message_id: MessageId,
        offset: u64,
        text: &str,
    ) -> impl Future<Output = Result<(), StorageError>> + Send {
        self.inner.checkpoint_message(message_id, offset, text)
    }

    fn outbox_pending(
        &self,
        limit: usize,
    ) -> impl Future<Output = Result<Vec<OutboxItem>, StorageError>> + Send {
        self.inner.outbox_pending(limit)
    }

    fn outbox_mark(
        &self,
        id: OutboxId,
        state: DeliveryState,
    ) -> impl Future<Output = Result<(), StorageError>> + Send {
        self.inner.outbox_mark(id, state)
    }

    fn outbox_requeue_dispatched(
        &self,
        redeliver_prefixes: Vec<String>,
        fail_prefixes: Vec<String>,
    ) -> impl Future<Output = Result<RequeueReport, StorageError>> + Send {
        self.inner
            .outbox_requeue_dispatched(redeliver_prefixes, fail_prefixes)
    }

    fn outbox_pending_where(
        &self,
        selector: OutboxSelector,
        limit: usize,
    ) -> impl Future<Output = Result<Vec<OutboxItem>, StorageError>> + Send {
        self.inner.outbox_pending_where(selector, limit)
    }

    fn rebuild_projections(
        &self,
    ) -> impl Future<Output = Result<RebuildReport, StorageError>> + Send {
        self.inner.rebuild_projections()
    }
}

#[cfg(test)]
#[path = "gate_tests.rs"]
mod tests;
