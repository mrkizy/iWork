//! 接线（T10a 方案 D9；T07 方案 6.10.13；任务卡验收 3，本卡的合入门槛）：真存储 + 真提交观察者 + `engine-sync`。

mod support;

use std::sync::Arc;
use std::time::Duration;

use iwork_daemon::assembly;
use iwork_daemon::gate::{GatedStore, SealGate};
use iwork_daemon::publish::{PublishObserver, PublishState};
use iwork_daemon::tasks::engine_sync;
use iwork_daemon::time::{MonotonicIds, SystemClock, TokioTimer};
use iwork_domain::event::{
    ApprovalRequested, AssistantCreated, AssistantDeleted, DomainEvent, GlobalSettingsChanged,
    TurnQueued, TurnStateChanged,
};
use iwork_domain::turn::TurnState;
use iwork_host::fs::OsPathResolver;
use iwork_ports::store::StoragePort;
use iwork_store::{CommitObserver, CommittedBatch};
use support::fixtures::*;
use support::turns::{FakeAgent, Nowhere, events, seed};

const HELPER: u128 = 40;
const TURN: u128 = 20;
const APPROVAL: u128 = 30;

/// 对话里有一个助手 HELPER 的回合在等审批。
fn orphan_setup() -> Vec<DomainEvent> {
    let mut t = turn(TURN, support::turns::CONV);
    t.assistant_id = aid(HELPER);
    let step = |from, to| -> DomainEvent {
        TurnStateChanged {
            turn_id: tid(TURN),
            conversation_id: Some(cid(support::turns::CONV)),
            from,
            to,
            reason: None,
        }
        .into()
    };
    let mut events = seed();
    events.extend([
        AssistantCreated {
            assistant: assistant(HELPER, false),
        }
        .into(),
        TurnQueued { turn: t }.into(),
        step(TurnState::Queued, TurnState::Running),
        step(TurnState::Running, TurnState::WaitingApproval),
        ApprovalRequested {
            approval: approval(APPROVAL, TURN, support::turns::CONV),
        }
        .into(),
    ]);
    events
}

type Engine = iwork_core::approval::engine::ApprovalEngine<
    SystemClock,
    MonotonicIds<SystemClock>,
    GatedStore<Store>,
    FakeAgent,
    TokioTimer,
    OsPathResolver,
>;

async fn engine_on(env: &Env, raw: &Arc<Store>) -> Arc<Engine> {
    let gated = GatedStore::new(raw.clone(), Arc::new(SealGate::default()));
    assembly::start_engine(
        iwork_core::approval::engine::EnginePorts {
            clock: SystemClock::new(),
            ids: MonotonicIds::new(SystemClock::new()),
            store: gated.clone(),
            reviewer: FakeAgent(Arc::default()),
            timer: TokioTimer,
            fs: OsPathResolver::new(env.tmp.path().into(), false),
        },
        Default::default(),
        &gated,
        &MonotonicIds::new(SystemClock::new()),
    )
    .await
    .unwrap()
}

fn voided(evs: &[(u64, DomainEvent)]) -> bool {
    evs.iter().any(
        |(_, e)| matches!(e, DomainEvent::ApprovalVoided(v) if v.approval_id == apid(APPROVAL)),
    )
}

/// 绕过引擎直接删掉助手：它名下那条审批成了孤儿。删除的提交限时返回，期间别的提交照常完成（写线程没被阻塞），
/// 随后 `engine-sync` 被观察者叫醒、把孤儿作废。
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn engine_sync_wiring_real_observer() {
    let env = Env::new();
    let raw = Arc::new(env.open());
    raw.commit(cmd(1, orphan_setup())).await.unwrap();
    let publishing = PublishState::new();
    raw.set_commit_observer(Box::new(PublishObserver::new(
        Arc::new(Nowhere),
        publishing.clone(),
    )))
    .await
    .unwrap();
    let engine = engine_on(&env, &raw).await;
    let woke_host = Arc::new(tokio::sync::Notify::new());
    let sync = tokio::spawn(engine_sync::run(
        engine.clone(),
        publishing.engine.clone(),
        {
            let w = woke_host.clone();
            move || w.notify_one()
        },
    ));
    // 启动时的那次追平先做完（孤儿这时还没有）
    tokio::time::timeout(Duration::from_secs(10), woke_host.notified())
        .await
        .expect("engine-sync 起来先追平一次");
    assert!(!voided(&events(&raw).await));

    let delete = tokio::time::timeout(
        Duration::from_secs(5),
        raw.commit(cmd(
            2,
            vec![
                AssistantDeleted {
                    assistant_id: aid(HELPER),
                    name_snapshot: "a40".into(),
                }
                .into(),
            ],
        )),
    )
    .await
    .expect("删除的提交在限定时间内返回");
    delete.unwrap();
    // 写线程没被阻塞：另一笔照常完成
    tokio::time::timeout(
        Duration::from_secs(5),
        raw.commit(cmd(
            3,
            vec![
                GlobalSettingsChanged {
                    playful_ui: Some(false),
                    autostart: None,
                    projects_root: None,
                }
                .into(),
            ],
        )),
    )
    .await
    .expect("别的提交照常完成")
    .unwrap();
    tokio::time::timeout(Duration::from_secs(10), async {
        while !voided(&events(&raw).await) {
            woke_host.notified().await;
        }
    })
    .await
    .expect("孤儿随后被作废");
    sync.abort();
}

/// D9：`engine-sync` 任务没了又重起（任务表照 restartable 重起它）：重起后第一件事是追平一次。任务不在的时候删掉助手、
/// 留下孤儿审批；重起的任务拿一个没人叫的唤醒，所以孤儿只可能被这次起步追平清掉。对照：任务不在时孤儿留着。
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn engine_sync_task_restarts_and_catches_up() {
    let env = Env::new();
    let raw = Arc::new(env.open());
    raw.commit(cmd(1, orphan_setup())).await.unwrap();
    let publishing = PublishState::new();
    raw.set_commit_observer(Box::new(PublishObserver::new(
        Arc::new(Nowhere),
        publishing.clone(),
    )))
    .await
    .unwrap();
    let engine = engine_on(&env, &raw).await;
    let synced = Arc::new(tokio::sync::Notify::new());
    let after_sync = {
        let s = synced.clone();
        move || s.notify_one()
    };
    let first = tokio::spawn(engine_sync::run(
        engine.clone(),
        publishing.engine.clone(),
        after_sync.clone(),
    ));
    tokio::time::timeout(Duration::from_secs(10), synced.notified())
        .await
        .expect("第一次起来先追平一次");
    // 任务没了
    first.abort();
    let _ = first.await;
    raw.commit(cmd(
        2,
        vec![
            AssistantDeleted {
                assistant_id: aid(HELPER),
                name_snapshot: "a40".into(),
            }
            .into(),
        ],
    ))
    .await
    .unwrap();
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert!(!voided(&events(&raw).await), "对照：任务不在时孤儿留着");
    // 重起：唤醒是新的、没人叫
    let restarted = tokio::spawn(engine_sync::run(
        engine.clone(),
        Arc::new(tokio::sync::Notify::new()),
        after_sync,
    ));
    tokio::time::timeout(Duration::from_secs(10), synced.notified())
        .await
        .expect("重起后追平了一次");
    assert!(voided(&events(&raw).await), "重起后的那次追平把孤儿清了");
    restarted.abort();
}

/// K15 的对照：把 `sync()` 直接写进写线程上的提交观察者。有孤儿要清时它要提交，排在写队列里等写线程，写线程却停在
/// 回调里：删除那一笔的回执发不出来。正确接线（上一格）在同样的布置下限时返回。
struct SyncInObserver {
    engine: Arc<Engine>,
}

impl CommitObserver for SyncInObserver {
    fn committed(&mut self, batch: CommittedBatch<'_>) {
        if batch
            .events
            .iter()
            .any(|e| iwork_core::approval::engine::fold::in_engine_streams(&e.event))
        {
            let engine = self.engine.clone();
            let _ = std::thread::spawn(move || pollster::block_on(engine.sync())).join();
        }
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn engine_sync_in_observer_blocks_the_writer() {
    let env = Env::new();
    let raw = Arc::new(env.open());
    raw.commit(cmd(1, orphan_setup())).await.unwrap();
    let engine = engine_on(&env, &raw).await;
    raw.set_commit_observer(Box::new(SyncInObserver {
        engine: engine.clone(),
    }))
    .await
    .unwrap();
    let delete = tokio::time::timeout(
        Duration::from_secs(3),
        raw.commit(cmd(
            2,
            vec![
                AssistantDeleted {
                    assistant_id: aid(HELPER),
                    name_snapshot: "a40".into(),
                }
                .into(),
            ],
        )),
    )
    .await;
    assert!(delete.is_err(), "坏接线下删除的回执发不出来：{delete:?}");
    // 写线程永远停在回调里；库与线程随测试进程结束
    std::mem::forget(raw);
    std::mem::forget(env);
}
