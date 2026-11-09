//! 写线程的单测：组提交、savepoint 隔离、整批失败收尾、seq 连续（方案 2.4、S2、S3）。
//! 故障注入（`Fault`）只在单测里可用，所以放在 crate 内部。

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use iwork_app::codec::ProtoEventCodec;
use iwork_domain::event::{DomainEvent, ProjectAdded};
use iwork_domain::id::Seq;
use iwork_ports::store::{CommitReceipt, EventFilter, Projector, StorageError, StoragePort};

use crate::fixtures::*;
use crate::writer::{Fault, WriteTx};
use crate::{SqliteProjector, StoreConfig};

fn added(n: u128) -> DomainEvent {
    ProjectAdded {
        project: project(n),
    }
    .into()
}

fn wide_window() -> StoreConfig {
    StoreConfig {
        group_commit_window: Duration::from_millis(300),
        ..Env::config()
    }
}

fn receipt(first: u64, last: u64) -> CommitReceipt {
    CommitReceipt {
        first_seq: Seq(first),
        last_seq: Seq(last),
        replayed: false,
    }
}

fn seqs(env: &Env) -> Vec<i64> {
    let conn = rusqlite::Connection::open(env.db()).unwrap();
    let mut stmt = conn.prepare("SELECT seq FROM events ORDER BY seq").unwrap();
    stmt.query_map([], |r| r.get(0))
        .unwrap()
        .collect::<Result<_, _>>()
        .unwrap()
}

/// 名字为 `boom` 的项目投影失败（事务仍在），其余照常投影。
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

#[test]
fn batch_window_groups_ops() {
    let env = Env::new();
    let store = Arc::new(env.open_with(wide_window(), Default::default()).unwrap());
    let barrier = Arc::new(std::sync::Barrier::new(16));
    let handles: Vec<_> = (0..16u128)
        .map(|i| {
            let (store, barrier) = (store.clone(), barrier.clone());
            std::thread::spawn(move || {
                barrier.wait();
                block(store.commit(cmd(i + 1, vec![added(i + 1)]))).unwrap()
            })
        })
        .collect();
    let mut lasts: Vec<u64> = handles
        .into_iter()
        .map(|h| h.join().unwrap().last_seq.0)
        .collect();
    lasts.sort_unstable();
    assert_eq!(
        lasts,
        (1..=16).collect::<Vec<_>>(),
        "16 条命令各占一个号、连续"
    );
    let batches = store.committed_batches();
    assert!(batches < 16, "组提交：16 条命令只用了 {batches} 次 COMMIT");
    assert!(batches >= 1, "对照：确实提交过");
}

#[test]
fn failed_command_in_batch_does_not_affect_siblings() {
    let env = Env::new();
    let projector = Arc::new(FailingProjector {
        inner: SqliteProjector::new(Arc::new(ProtoEventCodec)),
        armed: AtomicBool::new(true),
    });
    let store = env.open_projector(projector, wide_window());
    let mut bad = project(2);
    bad.name = "boom".into();
    // 三条同时入队，落在同一批
    let a = store.commit(cmd(1, vec![added(1)]));
    let b = store.commit(cmd(2, vec![ProjectAdded { project: bad }.into()]));
    let c = store.commit(cmd(3, vec![added(3)]));
    assert_eq!(block(a), Ok(receipt(1, 1)));
    assert!(matches!(block(b), Err(StorageError::Projection { .. })));
    assert_eq!(block(c), Ok(receipt(2, 2)), "失败那条没占号");
    assert_eq!(store.committed_batches(), 1, "三条确实在同一批");
    assert_eq!(seqs(&env), vec![1, 2]);
    assert_eq!(
        scalar(&env.db(), "SELECT COUNT(*) FROM commands"),
        2,
        "失败的命令不记幂等"
    );
}

#[test]
fn dml_error_aborts_whole_batch_when_tx_lost() {
    let env = Env::new();
    let store = env.open_with(wide_window(), Default::default()).unwrap();
    // 第一条命令的事件插入照常，第二条的插入被中断：SQLite 自动回滚整个事务
    store.inject_fault(Fault::InterruptEventInsert { skip: 1 });
    let a = store.commit(cmd(1, vec![added(1)]));
    let b = store.commit(cmd(2, vec![added(2)]));
    assert!(
        matches!(block(a), Err(StorageError::Io { .. })),
        "已 RELEASE 的也作废"
    );
    assert!(matches!(block(b), Err(StorageError::Io { .. })));
    assert_eq!(seqs(&env), Vec::<i64>::new(), "两条都不在表里");
    assert_eq!(
        dump_table(&env.db(), "projects", "id"),
        Vec::<String>::new()
    );
    // 之后照常，seq 从持久 head 接上
    assert_eq!(
        block(store.commit(cmd(3, vec![added(3)]))),
        Ok(receipt(1, 1))
    );
    assert_eq!(seqs(&env), vec![1]);
}

#[test]
fn commit_failure_rolls_back_then_reseeds() {
    for tx_still_open in [true, false] {
        let env = Env::new();
        let store = env.open();
        block(store.commit(cmd(1, vec![added(1), added(2)]))).unwrap();
        store.inject_fault(Fault::CommitFails { tx_still_open });
        let failed = block(store.commit(cmd(2, vec![added(3)])));
        assert!(
            matches!(failed, Err(StorageError::Io { .. })),
            "{tx_still_open}: {failed:?}"
        );
        assert_eq!(seqs(&env), vec![1, 2], "{tx_still_open}: 失败那批没落盘");
        // 下一批 BEGIN IMMEDIATE 成功，seq 接在持久 MAX(seq) 后面
        assert_eq!(
            block(store.commit(cmd(3, vec![added(4)]))),
            Ok(receipt(3, 3))
        );
        assert_eq!(seqs(&env), vec![1, 2, 3]);
        // 失败的命令可以原样重试
        assert_eq!(
            block(store.commit(cmd(2, vec![added(3)]))),
            Ok(receipt(4, 4))
        );
    }
}

#[test]
fn rollback_failure_marks_store_unusable() {
    let env = Env::new();
    let store = env.open();
    store.inject_fault(Fault::CommitFails {
        tx_still_open: true,
    });
    store.inject_fault(Fault::RollbackFails);
    assert!(block(store.commit(cmd(1, vec![added(1)]))).is_err());
    let after = block(store.commit(cmd(2, vec![added(2)])));
    match after {
        Err(StorageError::Io { message }) => assert!(message.contains("写连接已失效"), "{message}"),
        other => panic!("应当立即失败：{other:?}"),
    }
    assert!(block(store.rebuild_projections()).is_err(), "重建也拒绝");
}

// ---------- seq（方案 2.4 的四个用例） ----------

#[test]
fn first_event_in_empty_db_is_seq_1() {
    let env = Env::new();
    let store = env.open();
    assert_eq!(block(store.head_seq()), Ok(Seq(0)));
    assert_eq!(
        block(store.commit(cmd(1, vec![added(1)]))),
        Ok(receipt(1, 1))
    );
}

#[test]
fn reopen_appends_after_max_seq() {
    let env = Env::new();
    {
        let store = env.open();
        block(store.commit(cmd(1, vec![added(1), added(2), added(3)]))).unwrap();
        store.close().unwrap();
    }
    let store = env.open();
    assert_eq!(
        block(store.commit(cmd(2, vec![added(4)]))),
        Ok(receipt(4, 4))
    );
    assert_eq!(seqs(&env), vec![1, 2, 3, 4]);
}

#[test]
fn commit_without_events_returns_head() {
    let env = Env::new();
    let store = env.open();
    assert_eq!(
        block(store.commit(cmd(1, vec![]))),
        Ok(receipt(0, 0)),
        "空库上空命令"
    );
    block(store.commit(cmd(2, vec![added(1), added(2), added(3)]))).unwrap();
    assert_eq!(block(store.commit(cmd(3, vec![]))), Ok(receipt(3, 3)));
    assert_eq!(seqs(&env), vec![1, 2, 3], "空命令不写事件");
}

#[test]
fn append_after_whole_batch_rollback_is_contiguous() {
    let env = Env::new();
    let store = env.open();
    block(store.commit(cmd(1, vec![added(1)]))).unwrap();
    store.inject_fault(Fault::CommitFails {
        tx_still_open: false,
    });
    assert!(block(store.commit(cmd(2, vec![added(2), added(3)]))).is_err());
    assert_eq!(
        block(store.commit(cmd(3, vec![added(4)]))),
        Ok(receipt(2, 2))
    );
    let events = block(store.read_events(EventFilter::All, Seq(0), 10)).unwrap();
    let got: Vec<u64> = events.iter().map(|e| e.seq.0).collect();
    assert_eq!(got, vec![1, 2]);
}
