//! 方案第 3 步：`StoragePort` 在 SQLite 上的契约。`store_commit_and_rebuild_project_same_event`
//! 是 `iwork-ports/tests/fakes.rs` 同名测试搬到真实实现上。

mod support;

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use iwork_app::codec::ProtoEventCodec;
use iwork_domain::event::{DomainEvent, ProjectAdded, StreamId, StreamKind};
use iwork_domain::id::Seq;
use iwork_ports::read_model::ReadModelPort;
use iwork_ports::store::{
    CommitReceipt, DeliveryState, EventFilter, OutboxId, Projector, RebuildReport, StorageError,
    StoragePort, StreamSelector,
};
use iwork_store::{SqliteProjector, WriteTx};
use support::*;

/// 计数的投影器：包一层真实投影器。
struct Counting {
    inner: SqliteProjector<ProtoEventCodec>,
    calls: AtomicUsize,
}

impl Projector for Counting {
    type Tx = WriteTx;
    fn project(&self, tx: &mut WriteTx, seq: Seq, event: &DomainEvent) -> Result<(), StorageError> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        self.inner.project(tx, seq, event)
    }
}

fn added(n: u128) -> DomainEvent {
    ProjectAdded {
        project: project(n),
    }
    .into()
}

#[test]
fn store_commit_and_rebuild_project_same_event() {
    let env = Env::new();
    let projector = Arc::new(Counting {
        inner: SqliteProjector::new(Arc::new(ProtoEventCodec)),
        calls: AtomicUsize::new(0),
    });
    let store = env.open_projector(projector.clone(), Env::config());
    let event = added(42);
    let mut req = cmd(7, vec![event.clone()]);
    req.outbox = vec![outbox("runtime", &[1])];

    let receipt = block(store.commit(req.clone())).unwrap();
    assert_eq!(
        receipt,
        CommitReceipt {
            first_seq: Seq(1),
            last_seq: Seq(1),
            replayed: false
        }
    );
    let stored = block(store.read_events(EventFilter::All, Seq(0), 10)).unwrap();
    assert_eq!(stored.len(), 1);
    assert_eq!(stored[0].event, event, "读回来的是解码后的同一个事件");
    assert_eq!(stored[0].at.0, T0, "信封时间来自注入的时钟");

    let online = dump_projections(&env.db());
    let projects = block(read_model(&store).projects()).unwrap();
    assert_eq!(projects.len(), 1);
    assert_eq!(projects[0].project, project(42));
    assert_eq!(projector.calls.load(Ordering::SeqCst), 1);

    // 重发同一 command_id：什么都不写，返回上次的序号
    let again = block(store.commit(req)).unwrap();
    assert_eq!(
        again,
        CommitReceipt {
            replayed: true,
            ..receipt
        }
    );
    assert_eq!(projector.calls.load(Ordering::SeqCst), 1);

    let outbox_before = dump_table(&env.db(), "outbox", "id");
    assert_eq!(outbox_before.len(), 1);
    let report = block(store.rebuild_projections()).unwrap();
    assert_eq!(
        report,
        RebuildReport {
            events: 1,
            last_seq: Seq(1)
        }
    );
    assert_eq!(
        dump_projections(&env.db()),
        online,
        "重建后的表与在线提交后逐行相同"
    );
    assert_eq!(
        projector.calls.load(Ordering::SeqCst),
        2,
        "两条路径走同一个 projector"
    );
    assert_eq!(
        dump_table(&env.db(), "outbox", "id"),
        outbox_before,
        "重建不写 outbox"
    );
}

#[test]
fn read_after_commit_sees_it() {
    let env = Env::new();
    let store = Arc::new(env.open());
    let handles: Vec<_> = (0..16u128)
        .map(|t| {
            let store = store.clone();
            std::thread::spawn(move || {
                for i in 0..20u128 {
                    let n = t * 100 + i + 1;
                    let r = block(store.commit(cmd(n, vec![added(n)]))).unwrap();
                    let from = Seq(r.last_seq.0 - 1);
                    let got = block(store.read_events(EventFilter::All, from, 1)).unwrap();
                    assert_eq!(got.len(), 1, "提交返回后马上读得到");
                    assert_eq!(got[0].seq, r.last_seq);
                    assert_eq!(got[0].event, added(n));
                }
            })
        })
        .collect();
    for h in handles {
        h.join().unwrap();
    }
    assert_eq!(block(store.head_seq()), Ok(Seq(320)));
}

#[test]
fn read_events_filters_by_stream_and_respects_from_and_limit() {
    let env = Env::new();
    let store = env.open();
    block(store.commit(cmd(1, vec![added(1), added(2), added(3)]))).unwrap();
    block(store.commit(cmd(
        2,
        vec![iwork_domain::event::RuntimeUpdated { runtime: runtime(1) }.into()],
    )))
    .unwrap();
    let project2 = StreamSelector {
        kind: StreamKind::Project,
        id: Some(StreamId(pid(2).to_string())),
    };
    let got = block(store.read_events(EventFilter::Streams(vec![project2]), Seq(0), 10)).unwrap();
    assert_eq!(got.iter().map(|e| e.seq.0).collect::<Vec<_>>(), vec![2]);

    let all_projects = StreamSelector {
        kind: StreamKind::Project,
        id: None,
    };
    let runtimes = StreamSelector {
        kind: StreamKind::Runtime,
        id: None,
    };
    let got = block(store.read_events(
        EventFilter::Streams(vec![all_projects.clone(), runtimes]),
        Seq(1),
        10,
    ))
    .unwrap();
    assert_eq!(
        got.iter().map(|e| e.seq.0).collect::<Vec<_>>(),
        vec![2, 3, 4]
    );
    let got =
        block(store.read_events(EventFilter::Streams(vec![all_projects]), Seq(0), 2)).unwrap();
    assert_eq!(
        got.iter().map(|e| e.seq.0).collect::<Vec<_>>(),
        vec![1, 2],
        "limit"
    );
    let none = block(store.read_events(EventFilter::Streams(vec![]), Seq(0), 10)).unwrap();
    assert!(none.is_empty());
}

#[test]
fn outbox_pending_returns_queued_in_order() {
    let env = Env::new();
    let store = env.open();
    let mut a = cmd(1, vec![added(1)]);
    a.outbox = vec![outbox("t1", b"a"), outbox("t2", b"b")];
    let mut b = cmd(2, vec![]);
    b.outbox = vec![outbox("t3", b"c")];
    block(store.commit(a)).unwrap();
    block(store.commit(b)).unwrap();

    let pending = block(store.outbox_pending(10)).unwrap();
    let targets: Vec<_> = pending.iter().map(|o| o.target.0.as_str()).collect();
    assert_eq!(targets, vec!["t1", "t2", "t3"]);
    assert!(
        pending
            .iter()
            .all(|o| o.state == DeliveryState::Queued && o.attempts == 0)
    );

    block(store.outbox_mark(pending[0].id, DeliveryState::Dispatched)).unwrap();
    let pending2 = block(store.outbox_pending(10)).unwrap();
    assert_eq!(pending2.len(), 2, "派发出去的不再待发");
    assert_eq!(block(store.outbox_pending(1)).unwrap().len(), 1, "limit");
    let attempts = scalar(
        &env.db(),
        &format!("SELECT attempts FROM outbox WHERE id = {}", pending[0].id.0),
    );
    assert_eq!(attempts, 1, "进入已派发算一次尝试");

    let missing = block(store.outbox_mark(OutboxId(999), DeliveryState::Failed));
    assert!(matches!(
        missing,
        Err(StorageError::NotFound {
            entity: "outbox",
            ..
        })
    ));
}

#[test]
fn wal_truncates_after_maintenance() {
    let env = Env::new();
    let store = env.open();
    for i in 0..50u128 {
        let events = (0..100).map(|j| added(i * 1000 + j + 1)).collect();
        block(store.commit(cmd(i + 1, events))).unwrap();
    }
    assert_eq!(block(store.head_seq()), Ok(Seq(5000)));
    let wal = std::path::PathBuf::from(format!("{}-wal", env.db().display()));
    let before = std::fs::metadata(&wal).unwrap().len();
    assert!(before > 0, "写了 5000 条后 WAL 非空");
    block(store.maintenance()).unwrap();
    let after = std::fs::metadata(&wal).unwrap().len();
    assert!(
        after <= 64 * 1024 * 1024,
        "maintenance 后 WAL 不超过 journal_size_limit"
    );
    assert!(after < before, "截短了：{before} → {after}");
}
