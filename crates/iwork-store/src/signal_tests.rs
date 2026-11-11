//! 写线程对外的两个信号（T04 方案 2.6、2.10；S5、S6）：提交观察者、存储失效。

use std::future::Future;
use std::pin::pin;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll, Waker};
use std::time::Duration;

use iwork_app::codec::ProtoEventCodec;
use iwork_domain::common::Actor;
use iwork_domain::event::{DomainEvent, ProjectAdded};
use iwork_domain::id::{MessageId, Seq};
use iwork_ports::store::{
    CommitRequest, DeliveryState, OutboxId, Projector, StorageError, StoragePort,
};

use crate::fixtures::*;
use crate::writer::{Fault, WriteTx};
use crate::{CommitObserver, CommittedBatch, SqliteProjector, StoreConfig, StoreFailure};

fn added(n: u128) -> DomainEvent {
    ProjectAdded {
        project: project(n),
    }
    .into()
}

/// 观察者看到的一条事件：(seq, actor, 事件)。
type SeenEvent = (u64, String, DomainEvent);

/// 观察者看到的每一批：事件与 outbox 条数。
#[derive(Default)]
struct Seen {
    batches: Vec<(Vec<SeenEvent>, usize)>,
}

#[derive(Clone, Default)]
struct Recorder(Arc<Mutex<Seen>>);

impl CommitObserver for Recorder {
    fn committed(&mut self, batch: CommittedBatch<'_>) {
        let events = batch
            .events
            .iter()
            .map(|e| (e.seq.0, e.actor.0.clone(), e.event.clone()))
            .collect();
        self.0
            .lock()
            .unwrap()
            .batches
            .push((events, batch.outbox_added));
    }
}

impl Recorder {
    fn seqs(&self) -> Vec<u64> {
        let seen = self.0.lock().unwrap();
        seen.batches
            .iter()
            .flat_map(|(es, _)| es.iter().map(|e| e.0))
            .collect()
    }
    fn batches(&self) -> usize {
        self.0.lock().unwrap().batches.len()
    }
}

fn poll_once<F: Future>(f: std::pin::Pin<&mut F>) -> Poll<F::Output> {
    f.poll(&mut Context::from_waker(Waker::noop()))
}

fn wait_failure(store: &Store) -> StoreFailure {
    let (tx, rx) = std::sync::mpsc::channel();
    let f = store.failed();
    std::thread::spawn(move || tx.send(block(f)).unwrap());
    rx.recv_timeout(Duration::from_secs(10))
        .expect("10 秒内没有失效信号")
}

// ---------- 提交观察者（S6） ----------

/// 并发 64 个提交：观察者看到的 seq 严格递增、连续；任何一个回执到达时，它的事件已经被观察到。
#[test]
fn observer_called_in_seq_order_before_reply() {
    let env = Env::new();
    let store = Arc::new(env.open());
    let rec = Recorder::default();
    block(store.set_commit_observer(Box::new(rec.clone()))).unwrap();
    let barrier = Arc::new(std::sync::Barrier::new(64));
    let handles: Vec<_> = (0..64u128)
        .map(|i| {
            let (store, barrier, rec) = (store.clone(), barrier.clone(), rec.clone());
            std::thread::spawn(move || {
                barrier.wait();
                let r = block(store.commit(cmd(i + 1, vec![added(2 * i + 1), added(2 * i + 2)])))
                    .unwrap();
                let seen = rec.seqs();
                assert!(
                    seen.contains(&r.last_seq.0) && seen.contains(&r.first_seq.0),
                    "回执 {r:?} 到达时观察者还没看到它的事件"
                );
            })
        })
        .collect();
    for h in handles {
        h.join().unwrap();
    }
    assert_eq!(rec.seqs(), (1..=128).collect::<Vec<_>>());
    assert!(
        rec.batches() < 64,
        "并发提交应当成批回调（组提交），而不是每条一次"
    );
}

#[test]
fn observer_sees_actor_time_and_events_and_outbox_count() {
    let env = Env::new();
    let store = env.open();
    let rec = Recorder::default();
    block(store.set_commit_observer(Box::new(rec.clone()))).unwrap();
    let req = CommitRequest {
        command_id: cmd_id(1),
        actor: Actor("system:recovery".into()),
        events: vec![added(1), added(2)],
        outbox: vec![outbox("runtime:1", b"x"), outbox("runtime:2", b"y")],
    };
    block(store.commit(req)).unwrap();
    let seen = rec.0.lock().unwrap();
    assert_eq!(seen.batches.len(), 1);
    let (events, outbox_added) = &seen.batches[0];
    assert_eq!(*outbox_added, 2);
    assert_eq!(
        events,
        &vec![
            (1, "system:recovery".to_string(), added(1)),
            (2, "system:recovery".to_string(), added(2)),
        ]
    );
}

/// 名字为 `boom` 的项目投影失败（只这一条命令回滚），其余照常。
struct FailingProjector {
    inner: SqliteProjector<ProtoEventCodec>,
    armed: AtomicBool,
}

impl Projector for FailingProjector {
    type Tx = WriteTx;
    fn project(&self, tx: &mut WriteTx, seq: Seq, event: &DomainEvent) -> Result<(), StorageError> {
        if let DomainEvent::ProjectAdded(e) = event
            && e.project.name == "boom"
            && self.armed.load(Ordering::SeqCst)
        {
            return Err(StorageError::Projection {
                seq,
                message: "注入的投影失败".into(),
            });
        }
        self.inner.project(tx, seq, event)
    }
}

/// 幂等重发、单条失败回滚、不产生事件的写操作都不交给观察者；整批回滚时一条都不交。
#[test]
fn observer_skips_replayed_failed_and_non_event_writes() {
    let env = Env::new();
    let projector = Arc::new(FailingProjector {
        inner: SqliteProjector::new(Arc::new(ProtoEventCodec)),
        armed: AtomicBool::new(true),
    });
    let store = Arc::new(env.open_projector(
        projector,
        StoreConfig {
            group_commit_window: Duration::from_millis(200),
            ..Env::config()
        },
    ));
    let rec = Recorder::default();
    block(store.set_commit_observer(Box::new(rec.clone()))).unwrap();
    block(store.commit(cmd(1, vec![added(1)]))).unwrap();

    // 同一批里：重发 cmd 1、一条会投影失败的、一条正常的
    let mut boom = project(9);
    boom.name = "boom".into();
    let s2 = store.clone();
    let replay = std::thread::spawn(move || block(s2.commit(cmd(1, vec![added(1)]))));
    let s3 = store.clone();
    let fail = std::thread::spawn(move || {
        block(s3.commit(cmd(2, vec![ProjectAdded { project: boom }.into()])))
    });
    std::thread::sleep(Duration::from_millis(20));
    let ok = block(store.commit(cmd(3, vec![added(3)]))).unwrap();
    assert!(replay.join().unwrap().unwrap().replayed);
    assert!(fail.join().unwrap().is_err());
    assert_eq!(rec.seqs(), vec![1, ok.first_seq.0], "只有真正写进去的");

    let before = rec.batches();
    block(store.checkpoint_message(
        MessageId::from_raw(iwork_domain::id::RawUlid::from_u128(5)),
        1,
        "x",
    ))
    .unwrap();
    let mut req = cmd(4, vec![]);
    req.outbox = vec![outbox("runtime:1", b"x")];
    block(store.commit(req)).unwrap();
    assert_eq!(
        rec.batches(),
        before + 1,
        "只写 outbox 的命令也回调（叫醒投递）"
    );
    block(store.outbox_mark(OutboxId(1), DeliveryState::Dispatched)).unwrap();
    block(store.rebuild_projections()).unwrap();
    assert_eq!(
        rec.batches(),
        before + 1,
        "检查点、outbox 标记、重建都不回调"
    );

    // 整批回滚：一条都不交
    store.inject_fault(Fault::CommitFails {
        tx_still_open: false,
    });
    assert!(block(store.commit(cmd(5, vec![added(5)]))).is_err());
    assert_eq!(rec.batches(), before + 1);
}

#[test]
fn observer_installs_once() {
    let env = Env::new();
    let store = env.open();
    block(store.set_commit_observer(Box::new(Recorder::default()))).unwrap();
    let second = block(store.set_commit_observer(Box::new(Recorder::default())));
    assert!(matches!(second, Err(StorageError::Io { .. })), "{second:?}");
}

// ---------- 存储失效（S5） ----------

#[test]
fn failed_signals_writer_broken() {
    let env = Env::new();
    let store = env.open();
    let mut pending = pin!(store.failed());
    assert!(
        poll_once(pending.as_mut()).is_pending(),
        "对照：好好的存储不完成"
    );
    store.inject_fault(Fault::CommitFails {
        tx_still_open: true,
    });
    store.inject_fault(Fault::RollbackFails);
    assert!(block(store.commit(cmd(1, vec![added(1)]))).is_err());
    assert_eq!(wait_failure(&store), StoreFailure::WriterBroken);
    assert_eq!(
        block(pending),
        StoreFailure::WriterBroken,
        "先拿到的 future 也完成"
    );
}

#[test]
fn failed_signals_writer_gone() {
    let env = Env::new();
    let store = env.open();
    store.inject_fault(Fault::Panic);
    assert_eq!(wait_failure(&store), StoreFailure::WriterGone);
    let after = block(store.commit(cmd(1, vec![added(1)])));
    assert!(matches!(after, Err(StorageError::Io { .. })), "{after:?}");
}

#[test]
fn failed_signals_reader_gone() {
    let env = Env::new();
    let store = env.open();
    let r = block(
        store
            .readers
            .run(|_conn| -> Result<(), StorageError> { panic!("注入的读线程 panic") }),
    );
    assert!(r.is_err());
    assert_eq!(wait_failure(&store), StoreFailure::ReaderGone);
}

#[test]
fn close_does_not_signal() {
    let env = Env::new();
    let store = env.open();
    block(store.commit(cmd(1, vec![added(1)]))).unwrap();
    let mut pending = pin!(store.failed());
    store.close().unwrap();
    // 读线程在队列断开后才退出：给它们一点时间
    std::thread::sleep(Duration::from_millis(200));
    assert!(poll_once(pending.as_mut()).is_pending(), "正常关闭不算失效");

    let env = Env::new();
    let store = env.open();
    let mut pending = pin!(store.failed());
    drop(store);
    std::thread::sleep(Duration::from_millis(200));
    assert!(poll_once(pending.as_mut()).is_pending(), "直接 drop 也不算");
}

/// 等待者被丢弃时摘掉自己的 waker：反复新建、poll、丢弃，登记的 waker 不越积越多（第 1 轮审查 1/F-7）。
#[test]
fn dropped_waiters_do_not_accumulate() {
    let env = Env::new();
    let store = env.open();
    for _ in 0..100 {
        let mut f = pin!(store.failed());
        assert!(poll_once(f.as_mut()).is_pending());
    }
    assert_eq!(store.failure_waiting(), 0, "丢弃之后不该留下 waker");
    let mut kept = pin!(store.failed());
    assert!(poll_once(kept.as_mut()).is_pending());
    assert!(poll_once(kept.as_mut()).is_pending());
    assert_eq!(
        store.failure_waiting(),
        1,
        "对照：活着的等待者登记一项，重复 poll 不重复登记"
    );
}
