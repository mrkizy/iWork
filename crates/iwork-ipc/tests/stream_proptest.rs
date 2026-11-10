//! 断线续传 proptest（验收 4；方案 7.1）。
//!
//! 随机：提交（写入 / 删除，单路由与多路由键）、增删范围（含重叠范围、同 id 替换）、会写视图的查询、主动断线重连、
//! 小预算让服务端 `fail_connection`、快照两部分之间夹提交（必须被拒绝重取）、补发中途提交。
//! 客户端模型按方案 2.11 的契约实现：只从 `Incoming` 按到达顺序应用，按实体 id 幂等写入，快照与查询结果只替换视图
//! 成员，游标在应用时推进；连接被关就重连并一次写出全部 `Subscribe`，同游标第二次被关改用 `from_seq = 0`。
//!
//! 范围变化前后用查询屏障切段；提交、查询、主动断线与小预算断线仍可交错。每段完整持久事件序列与独立日志逐项对照，
//! 快照与查询结果核对真实载荷；MessageDelta 检查字节偏移连续、文字完整，多路由检查匹配范围与单次投递。

#[path = "stream_proptest/model.rs"]
mod model;
mod support;
use model::Model;

use std::time::Duration;

use iwork_ipc::server::Published;
use iwork_proto::pb;
use proptest::prelude::*;
use support::*;

const ENTITIES: [&str; 7] = [
    "conversation:c1",
    "conversation:c2",
    "conversation:c3",
    "project:p1",
    "project:p2",
    "turn:t1@c1",
    "turn:t2@c2",
];

fn scope_of(i: u8) -> pb::SubscriptionScope {
    match i % 4 {
        0 => sidebar(),
        1 => conversation("c1"),
        2 => conversation("c2"),
        _ => conversation("c3"),
    }
}

#[derive(Debug, Clone)]
enum Act {
    Commit { entity: u8, delete: bool },
    Burst { n: u8 },
    Subscribe { id: u8, scope: u8 },
    Unsubscribe { id: u8 },
    ViewQuery { conv: u8 },
    Disconnect,
    TearNextSnapshot,
    Pump,
    Deltas { conv: u8, n: u8 },
}

fn act() -> impl Strategy<Value = Act> {
    prop_oneof![
        4 => (0u8..7, prop::bool::weighted(0.2)).prop_map(|(entity, delete)| Act::Commit { entity, delete }),
        1 => (5u8..40).prop_map(|n| Act::Burst { n }),
        2 => (1u8..4, 0u8..4).prop_map(|(id, scope)| Act::Subscribe { id, scope }),
        1 => (1u8..4).prop_map(|id| Act::Unsubscribe { id }),
        1 => (1u8..4).prop_map(|conv| Act::ViewQuery { conv }),
        1 => Just(Act::Disconnect),
        1 => Just(Act::TearNextSnapshot),
        3 => Just(Act::Pump),
        2 => (1u8..4, 1u8..8).prop_map(|(conv, n)| Act::Deltas { conv, n }),
    ]
}

async fn run(acts: Vec<Act>, small_budget: bool) {
    let mut cfg = server_cfg();
    cfg.replay_page = 4;
    cfg.handover_timeout = Duration::from_secs(5);
    if small_budget {
        cfg.budget_limit = 8 * 1024;
        cfg.replay_reserve = 1024;
    }
    let h = start(cfg);
    *h.world.pad.lock().unwrap() = 200;
    // 非空快照和连续覆盖同一实体的中间事件是每一例的阳性对照。
    for (i, entity) in ENTITIES.iter().enumerate() {
        h.upsert(entity, i as u64 + 1);
    }
    let mut m = Model::default();
    m.connect(&h, false).await;
    m.subscribe(&h, 1, sidebar()).await;
    m.subscribe(&h, 2, conversation("c1")).await;
    for value in 8..11 {
        h.upsert("turn:t1@c1", value);
    }
    m.settle(&h).await;
    let mut value = 10u64;
    let mut message = 0;
    for a in std::iter::once(Act::Deltas { conv: 1, n: 3 }).chain(acts) {
        match a {
            Act::Commit { entity, delete } => {
                value += 1;
                let e = ENTITIES[entity as usize].to_owned();
                h.publish(if delete {
                    Op::Delete { entity: e }
                } else {
                    Op::Upsert { entity: e, value }
                });
            }
            Act::Burst { n } => {
                // 边提交边让出：提交落在补发、快照、交接的各个中间点上。
                for i in 0..n {
                    value += 1;
                    h.upsert(ENTITIES[(i as usize) % ENTITIES.len()], value);
                    if i % 3 == 0 {
                        tokio::task::yield_now().await;
                    }
                }
            }
            Act::Subscribe { id, scope } => m.subscribe(&h, id as u32, scope_of(scope)).await,
            Act::Unsubscribe { id } => m.unsubscribe(&h, id as u32).await,
            Act::ViewQuery { conv } => {
                m.query(&format!("c{conv}"));
            }
            Act::Disconnect => m.connect(&h, false).await,
            Act::Deltas { conv, n } => {
                m.settle(&h).await;
                message += 1;
                let id = format!("message{message}");
                let entity = format!("conversation:c{conv}");
                let generation = m.reconnects;
                let mut text = String::new();
                for _ in 0..n {
                    h.publisher.publish(Published::Transient {
                        event: pb::TransientEvent {
                            event: Some(pb::transient_event::Event::MessageDelta(
                                pb::MessageDelta {
                                    message_id: id.clone(),
                                    offset: text.len() as u64,
                                    text: "字a".into(),
                                },
                            )),
                        },
                        routes: routes_of(&entity),
                    });
                    text.push_str("字a");
                }
                m.settle(&h).await;
                if generation == m.reconnects {
                    assert_eq!(
                        m.view.texts.get(&id),
                        covers(&m.view.active(), &entity).then_some(&text),
                        "瞬时增量完整且偏移连续"
                    );
                }
            }
            Act::TearNextSnapshot => *h.world.snapshot_tear_once.lock().unwrap() = true,
            Act::Pump => m.pump(&h).await,
        }
    }

    m.settle(&h).await;
    assert_eq!(m.view.cursor, h.world.head(), "游标追平");
}

proptest! {
    // 默认 64 例；PROPTEST_CASES=2000 跑完整一轮。
    #![proptest_config(ProptestConfig {
        cases: std::env::var("PROPTEST_CASES").ok().and_then(|v| v.parse().ok()).unwrap_or(64),
        ..ProptestConfig::default()
    })]

    #[test]
    fn stream_never_loses_or_duplicates_across_reconnects(
        acts in prop::collection::vec(act(), 1..60),
        small_budget in prop::bool::weighted(0.4),
    ) {
        tokio::runtime::Builder::new_current_thread()
            .enable_time()
            .build()
            .unwrap()
            .block_on(run(acts, small_budget));
    }
}
