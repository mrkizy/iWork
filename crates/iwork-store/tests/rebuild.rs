//! 卡验收 3 / 方案 S6、2.9：投影重建与在线投影逐表一致；重建期间 outbox 无新投递；
//! 重建失败回滚到旧投影；投递开始后拒绝重建。

mod support;

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

use iwork_app::codec::ProtoEventCodec;
use iwork_domain::approval::{ApprovalState, Decider, ReviewState, VoidReason};
use iwork_domain::common::ArtifactArea;
use iwork_domain::event::*;
use iwork_domain::id::{ReviewId, Seq};
use iwork_domain::message::MessageBody;
use iwork_ports::store::{DeliveryState, Projector, StorageError, StoragePort};
use iwork_store::{SqliteProjector, WriteTx};
use support::*;

/// 重建时在第 `fail_at` 条事件上失败（只在 `armed` 后）。
struct FailAt {
    inner: SqliteProjector<ProtoEventCodec>,
    armed: AtomicBool,
    fail_at: AtomicU64,
}

impl Projector for FailAt {
    type Tx = WriteTx;
    fn project(&self, tx: &mut WriteTx, seq: Seq, event: &DomainEvent) -> Result<(), StorageError> {
        if self.armed.load(Ordering::SeqCst) && seq.0 == self.fail_at.load(Ordering::SeqCst) {
            return Err(StorageError::Projection {
                seq,
                message: "注入".into(),
            });
        }
        self.inner.project(tx, seq, event)
    }
}

fn history(store: &Store) {
    let mut long = user_message(31, 10, "");
    long.body = MessageBody::Artifact(artifact(1, ArtifactArea::Artifacts));
    let mut reviewed = approval(41, 20, 10);
    reviewed.review = ReviewState::Passed {
        review_id: ReviewId(tid(90)),
        reason: String::new(),
    };
    let mut req = cmd(
        1,
        vec![
            ProjectAdded {
                project: project(1),
            }
            .into(),
            AssistantCreated {
                assistant: assistant(1, true),
            }
            .into(),
            ConversationCreated {
                conversation: project_conversation(10, 1),
            }
            .into(),
            TurnQueued { turn: turn(20, 10) }.into(),
            MessageAppended { message: long }.into(),
            MessageAppended {
                message: assistant_message(32, 10, 20),
            }
            .into(),
            ApprovalRequested { approval: reviewed }.into(),
            ApprovalRequested {
                approval: approval(42, 20, 10),
            }
            .into(),
            RuntimeUpdated {
                runtime: runtime(1),
            }
            .into(),
        ],
    );
    req.outbox = vec![
        outbox("runtime", b"a"),
        outbox("runtime", b"b"),
        outbox("runtime", b"c"),
    ];
    block(store.commit(req)).unwrap();
    block(store.commit(cmd(
        2,
        vec![
                ApprovalDecided {
                    conversation_id: Some(cid(10)),
                    approval_id: apid(41),
                    state: ApprovalState::Approved {
                        decider: Decider::AiReview,
                    },
                    remember: None,
                }
                .into(),
                ApprovalVoided {
                    conversation_id: Some(cid(10)),
                    approval_id: apid(42),
                    reason: VoidReason::TurnCancelled,
                }
                .into(),
                LocalImportPrompted {
                    runtime_id: rtid(1),
                }
                .into(),
                RuntimeUpdated {
                    runtime: runtime(1),
                }
                .into(),
                GlobalSettingsChanged {
                    playful_ui: Some(false),
                    autostart: None,
                    projects_root: None,
                }
                .into(),
                ConversationRead {
                    conversation_id: cid(10),
                    last_read_seq: Seq(6),
                }
                .into(),
            ],
    )))
    .unwrap();
}

fn open_failing(env: &Env) -> (Store, Arc<FailAt>) {
    let p = Arc::new(FailAt {
        inner: SqliteProjector::new(Arc::new(ProtoEventCodec)),
        armed: AtomicBool::new(false),
        fail_at: AtomicU64::new(0),
    });
    (env.open_projector(p.clone(), Env::config()), p)
}

#[test]
fn rebuild_equals_online_projection_and_leaves_outbox_alone() {
    let env = Env::new();
    let store = env.open();
    history(&store);
    let online = dump_projections(&env.db());
    assert!(
        online.iter().filter(|(_, rows)| !rows.is_empty()).count() >= 8,
        "对照：多张表有数据"
    );
    let outbox = dump_table(&env.db(), "outbox", "id");
    let report = block(store.rebuild_projections()).unwrap();
    assert_eq!(report.events, 15);
    assert_eq!(report.last_seq, Seq(15));
    assert_eq!(
        dump_projections(&env.db()),
        online,
        "逐表一致（含 updated_seq）"
    );
    assert_eq!(
        dump_table(&env.db(), "outbox", "id"),
        outbox,
        "outbox 行数与内容不变"
    );
}

#[test]
fn failed_rebuild_rolls_back_to_old_projection() {
    let env = Env::new();
    let (store, p) = open_failing(&env);
    history(&store);
    let before = dump_projections(&env.db());
    p.fail_at.store(7, Ordering::SeqCst);
    p.armed.store(true, Ordering::SeqCst);
    let err = block(store.rebuild_projections());
    assert!(
        matches!(err, Err(StorageError::Projection { seq: Seq(7), .. })),
        "{err:?}"
    );
    assert_eq!(dump_projections(&env.db()), before, "旧投影原样保留");
    p.armed.store(false, Ordering::SeqCst);
    block(store.rebuild_projections()).unwrap();
    assert_eq!(dump_projections(&env.db()), before, "重跑即可");
}

/// 测试里的投递循环：取待发 → 计数 → 标为已派发。
fn deliver_all(store: &Store, delivered: &AtomicU64) {
    for item in block(store.outbox_pending(100)).unwrap() {
        delivered.fetch_add(1, Ordering::SeqCst);
        block(store.outbox_mark(item.id, DeliveryState::Dispatched)).unwrap();
    }
}

#[test]
fn rebuild_before_delivery_has_zero_deliveries() {
    let env = Env::new();
    let store = env.open();
    history(&store);
    assert_eq!(
        block(store.outbox_pending(100)).unwrap().len(),
        3,
        "对照：3 条待投递"
    );
    let delivered = AtomicU64::new(0);

    // T04 的顺序：重建 → mark_delivery_started → 起投递器
    block(store.rebuild_projections()).unwrap();
    assert_eq!(delivered.load(Ordering::SeqCst), 0, "重建返回前没有投递");
    store.mark_delivery_started();
    deliver_all(&store, &delivered);
    deliver_all(&store, &delivered);
    assert_eq!(delivered.load(Ordering::SeqCst), 3, "每条只投递一次");

    let before = dump_projections(&env.db());
    let err = block(store.rebuild_projections());
    assert!(
        matches!(&err, Err(StorageError::Io { message }) if message.contains("启动阶段")),
        "{err:?}"
    );
    assert_eq!(dump_projections(&env.db()), before, "投影表不变");
}
