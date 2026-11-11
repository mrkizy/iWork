//! 客户端帧热路径基准（T09 方案第 6 节）：`cargo bench -p iwork-bench --bench view`。
//!
//! 新热路径，以这次的数为基线。全在进程内：量的是 client-core 自身（应用事件、derive、编码、分块），
//! 不含 ipc 与网页。每组一行 JSON 到 stdout。
//!
//! - `stream_16x100_frame`：16 个对话各 100 条消息流式到达（打开其中一个，其余落在侧栏摘要上），
//!   每 16ms 网页来取一帧；统计每帧 derive + 编码耗时、帧字节数。
//! - `stream_apply_to_handed`：一条增量从 `Store::apply` 到这一帧的最后一块交出的耗时（不含 ipc 往返）。

use std::time::{Duration, Instant, SystemTime};

use iwork_client_core::derive::Ctx;
use iwork_client_core::frame::{FRAME_CHUNK_MAX, FRAME_INTERVAL, FrameEngine, Pull};
use iwork_client_core::store::{Connection, Item, ScopeKind, Store, Visible};
use iwork_proto::pb;
use pb::domain_event::Event as E;

const CONVS: usize = 16;
const MSGS: usize = 100;
const DELTAS_PER_MSG: usize = 20;

fn persisted(seq: u64, sub: u32, event: E) -> Item {
    Item::Event(Box::new(pb::Event {
        subscription_ids: vec![sub],
        body: Some(pb::event::Body::Persisted(pb::PersistedEvent {
            seq,
            at: None,
            actor: "assistant".into(),
            event: Some(pb::DomainEvent { event: Some(event) }),
        })),
    }))
}

fn delta(sub: u32, id: &str, offset: u64, text: &str) -> Item {
    Item::Event(Box::new(pb::Event {
        subscription_ids: vec![sub],
        body: Some(pb::event::Body::Transient(pb::TransientEvent {
            event: Some(pb::transient_event::Event::MessageDelta(pb::MessageDelta {
                message_id: id.into(),
                offset,
                text: text.into(),
            })),
        })),
    }))
}

fn message(conv: &str, id: &str) -> E {
    E::MessageAppended(pb::MessageAppended {
        message: Some(pb::Message {
            id: id.into(),
            conversation_id: conv.into(),
            author: Some(pb::Author {
                kind: Some(pb::author::Kind::Assistant(pb::AssistantAuthor {
                    assistant_id: "a".into(),
                    name_snapshot: "iWork".into(),
                })),
            }),
            status: pb::MessageStatus::Streaming as i32,
            ..Default::default()
        }),
    })
}

fn pct(v: &mut [f64], p: f64) -> f64 {
    v.sort_by(f64::total_cmp);
    if v.is_empty() {
        return 0.0;
    }
    v[((v.len() - 1) as f64 * p).round() as usize]
}

fn main() {
    let mut store = Store::new();
    store.register_subscription(1, ScopeKind::Sidebar);
    store.register_subscription(2, ScopeKind::Conversation("c0".into()));
    store.set_visible(Visible {
        sidebar: true,
        conversation_id: Some("c0".into()),
        ..Default::default()
    });
    for id in [1, 2] {
        store.apply(Item::Subscribed(pb::Subscribed {
            subscription_id: id,
            snapshot: None,
            snapshot_seq: 0,
        }));
    }
    store.set_connection(Connection::Live);
    let mut engine = FrameEngine::new(FRAME_CHUNK_MAX);
    let epoch = engine.attach(&mut store);
    let ctx = Ctx {
        now: SystemTime::now(),
    };
    let mut applied: Option<(u64, u32)> = None;
    let mut clock = Instant::now();
    let mut seq = 0u64;
    let mut frame_us = Vec::new();
    let mut frame_bytes = Vec::new();
    let mut latency_us = Vec::new();
    let chunk = "流式输出的一小段文字，".repeat(2);

    let pull_all = |store: &mut Store,
                    engine: &mut FrameEngine,
                    epoch: u32,
                    applied: &mut Option<(u64, u32)>,
                    clock: Instant|
     -> (Duration, usize) {
        let start = Instant::now();
        let mut bytes = 0;
        while let Pull::Chunk(c) = engine.pull(store, epoch, *applied, clock, ctx) {
            bytes += c.data.len();
            *applied = Some((c.frame, c.index));
        }
        (start.elapsed(), bytes)
    };

    pull_all(&mut store, &mut engine, epoch, &mut applied, clock);
    for m in 0..MSGS {
        for c in 0..CONVS {
            seq += 1;
            let conv = format!("c{c}");
            store.apply(persisted(
                seq,
                if c == 0 { 2 } else { 1 },
                message(&conv, &format!("{conv}-m{m}")),
            ));
        }
        for d in 0..DELTAS_PER_MSG {
            for c in 0..CONVS {
                let conv = format!("c{c}");
                let t0 = Instant::now();
                store.apply(delta(
                    if c == 0 { 2 } else { 1 },
                    &format!("{conv}-m{m}"),
                    (d * chunk.len()) as u64,
                    &chunk,
                ));
                if c == 0 {
                    // 每轮 16 个对话各来一段增量后，网页来取一帧。
                    clock += FRAME_INTERVAL;
                    let (took, bytes) =
                        pull_all(&mut store, &mut engine, epoch, &mut applied, clock);
                    if bytes > 0 {
                        frame_us.push(took.as_secs_f64() * 1e6);
                        frame_bytes.push(bytes as f64);
                        latency_us.push(t0.elapsed().as_secs_f64() * 1e6);
                    }
                }
            }
        }
    }
    // 换代一次，量全量帧。
    let epoch = engine.attach(&mut store);
    applied = None;
    clock += FRAME_INTERVAL;
    let (full_took, full_bytes) = pull_all(&mut store, &mut engine, epoch, &mut applied, clock);

    let n = frame_us.len();
    println!(
        "{{\"group\":\"stream_16x100_frame\",\"frames\":{n},\"frame_us_p50\":{:.1},\"frame_us_p95\":{:.1},\"frame_us_max\":{:.1},\"frame_bytes_p50\":{:.0},\"frame_bytes_p95\":{:.0},\"full_frame_us\":{:.1},\"full_frame_bytes\":{full_bytes}}}",
        pct(&mut frame_us.clone(), 0.5),
        pct(&mut frame_us.clone(), 0.95),
        pct(&mut frame_us, 1.0),
        pct(&mut frame_bytes.clone(), 0.5),
        pct(&mut frame_bytes, 0.95),
        full_took.as_secs_f64() * 1e6,
    );
    println!(
        "{{\"group\":\"stream_apply_to_handed\",\"samples\":{},\"us_p50\":{:.1},\"us_p95\":{:.1}}}",
        latency_us.len(),
        pct(&mut latency_us.clone(), 0.5),
        pct(&mut latency_us, 0.95),
    );
}
