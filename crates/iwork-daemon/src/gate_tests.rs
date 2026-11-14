//! 闸与入队的边界（T10a 方案 0a.9 E4 ①，7.2a 的 K12、K13）。两个真线程加同步屏障定先后，不用异步等待点代替：
//! 假的真存储在 `commit` 的**同步部分**里停在屏障上，那一下就是「入队」。

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::mpsc;
use std::time::Duration;

use iwork_domain::common::Actor;
use iwork_domain::id::RawUlid;

use super::*;

/// 只认 `commit` 的假存储：调用的同步部分里数一次入队；装了屏障就先停在屏障上（人在闸的临界区里）。
#[derive(Default)]
struct Enqueues {
    count: AtomicUsize,
    /// `(到了屏障时发一声, 等放行)`
    barrier: Mutex<Option<(mpsc::Sender<()>, mpsc::Receiver<()>)>>,
}

impl StoragePort for Enqueues {
    fn commit(
        &self,
        _req: CommitRequest,
    ) -> impl Future<Output = Result<CommitReceipt, StorageError>> + Send {
        if let Some((arrived, release)) = self.barrier.lock().unwrap().take() {
            arrived.send(()).unwrap();
            release.recv().unwrap();
        }
        let n = self.count.fetch_add(1, Ordering::SeqCst) as u64 + 1;
        std::future::ready(Ok(CommitReceipt {
            first_seq: Seq(n),
            last_seq: Seq(n),
            replayed: false,
        }))
    }
    async fn command_receipt(&self, _: CommandId) -> Result<Option<CommitReceipt>, StorageError> {
        unimplemented!()
    }
    async fn head_seq(&self) -> Result<Seq, StorageError> {
        unimplemented!()
    }
    async fn read_events(
        &self,
        _: EventFilter,
        _: Seq,
        _: usize,
    ) -> Result<Vec<StoredEvent>, StorageError> {
        unimplemented!()
    }
    async fn checkpoint_message(&self, _: MessageId, _: u64, _: &str) -> Result<(), StorageError> {
        unimplemented!()
    }
    async fn outbox_pending(&self, _: usize) -> Result<Vec<OutboxItem>, StorageError> {
        unimplemented!()
    }
    async fn outbox_mark(&self, _: OutboxId, _: DeliveryState) -> Result<(), StorageError> {
        unimplemented!()
    }
    async fn outbox_requeue_dispatched(
        &self,
        _: Vec<String>,
        _: Vec<String>,
    ) -> Result<RequeueReport, StorageError> {
        unimplemented!()
    }
    async fn outbox_pending_where(
        &self,
        _: OutboxSelector,
        _: usize,
    ) -> Result<Vec<OutboxItem>, StorageError> {
        unimplemented!()
    }
    async fn rebuild_projections(&self) -> Result<RebuildReport, StorageError> {
        unimplemented!()
    }
}

fn request(n: u128) -> CommitRequest {
    CommitRequest {
        command_id: CommandId::from_raw(RawUlid::from_u128(n)),
        actor: Actor("t".into()),
        events: vec![],
        outbox: vec![],
    }
}

fn gated() -> (Arc<GatedStore<Enqueues>>, Arc<Enqueues>) {
    let inner = Arc::new(Enqueues::default());
    let store = Arc::new(GatedStore::new(
        inner.clone(),
        Arc::new(SealGate::default()),
    ));
    (store, inner)
}

/// K12：封口先到。之后的提交回 `Busy`，真存储一次都没被调。对照（裸存储）这一格入了队。
#[test]
fn gate_vs_enqueue_seal_first() {
    let (store, inner) = gated();
    store.gate().seal();
    let r = pollster::block_on(store.commit(request(1)));
    assert_eq!(r, Err(StorageError::Busy));
    assert_eq!(inner.count.load(Ordering::SeqCst), 0, "封口后入队次数为 0");
    // 对照：没封口时同一个调用入了队，说明上面的 0 不是假存储没在数
    let (open, open_inner) = gated();
    pollster::block_on(open.commit(request(2))).unwrap();
    assert_eq!(open_inner.count.load(Ordering::SeqCst), 1);
}

/// K13：入队先到。甲停在真存储的同步部分里（在闸的临界区里），乙这时封口：乙回不来，直到甲入完队；
/// 之后再提交回 `Busy`。
#[test]
fn gate_vs_enqueue_enqueue_first() {
    let (store, inner) = gated();
    let (arrived_tx, arrived) = mpsc::channel();
    let (release, release_rx) = mpsc::channel();
    *inner.barrier.lock().unwrap() = Some((arrived_tx, release_rx));

    let a = {
        let store = store.clone();
        std::thread::spawn(move || pollster::block_on(store.commit(request(1))))
    };
    arrived.recv_timeout(Duration::from_secs(10)).unwrap();
    let (sealed_tx, sealed) = mpsc::channel();
    let b = {
        let store = store.clone();
        std::thread::spawn(move || {
            store.gate().seal();
            sealed_tx.send(()).unwrap();
        })
    };
    // 唯一一处带时间的断言：它断言的是「没发生」（方案 7.2a 表下第三条）
    assert!(
        sealed.recv_timeout(Duration::from_millis(200)).is_err(),
        "甲还在临界区里，乙置封口不该先返回"
    );
    assert_eq!(inner.count.load(Ordering::SeqCst), 0);
    release.send(()).unwrap();
    assert!(a.join().unwrap().is_ok(), "入队先赢的那一笔照常落盘");
    sealed.recv_timeout(Duration::from_secs(10)).unwrap();
    b.join().unwrap();
    assert_eq!(inner.count.load(Ordering::SeqCst), 1);
    assert_eq!(
        pollster::block_on(store.commit(request(2))),
        Err(StorageError::Busy),
        "封口返回之后没有新入队"
    );
    assert_eq!(inner.count.load(Ordering::SeqCst), 1);
}
