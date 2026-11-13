//! 助手配置自动保存闸在主循环里的开销（T09 组 3）：`cargo bench -p iwork-bench --bench assistant_edits`。
//!
//! 会话层每轮主循环对「还有没落定改动」的助手推一次闸（`drive_assistant_edits` → `Store::step_assistant_edit`），
//! 然后网页按 16ms 来取帧。这里模拟这两步：助手主页开着、一个助手有一次改动在等摘要追上（闸忙着、什么都没变），
//! 量每轮「推一次闸 + 取完这一帧」的耗时与交出的字节。闸没变化时不该让助手主页重算、重发：字节应当是 0。
//! 每组一行 JSON 到 stdout；只报相对值时拿两个版本在同一时段交替跑。

use std::time::{Instant, SystemTime};

use iwork_client_core::derive::Ctx;
use iwork_client_core::frame::{FRAME_CHUNK_MAX, FRAME_INTERVAL, FrameEngine, Pull};
use iwork_client_core::store::assistant_edits::Field;
use iwork_client_core::store::{Connection, Item, ScopeKind, Store, Visible};
use iwork_proto::pb;

const ROUNDS: usize = 20_000;
const ID: &str = "a";

fn pct(v: &mut [f64], p: f64) -> f64 {
    v.sort_by(f64::total_cmp);
    v.get(((v.len().saturating_sub(1)) as f64 * p).round() as usize)
        .copied()
        .unwrap_or(0.0)
}

fn main() {
    let mut store = Store::new();
    store.register_subscription(1, ScopeKind::Sidebar);
    store.apply(Item::Subscribed(pb::Subscribed {
        subscription_id: 1,
        snapshot: None,
        snapshot_seq: 0,
    }));
    store.summary.assistants.push(pb::Assistant {
        id: ID.into(),
        version: 1,
        config: Some(pb::AssistantConfig {
            name: "阿森".into(),
            prompt: "你是开发助手。".repeat(200),
            runtime_id: "claude-code".into(),
            ..Default::default()
        }),
        ..Default::default()
    });
    store.set_visible(Visible {
        sidebar: true,
        assistant_id: Some(ID.into()),
        ..Default::default()
    });
    store.set_connection(Connection::Live);
    let mut engine = FrameEngine::new(FRAME_CHUNK_MAX);
    let epoch = engine.attach(&mut store);
    let ctx = Ctx {
        now: SystemTime::now(),
    };
    let mut applied: Option<(u64, u32)> = None;
    let mut clock = Instant::now();
    let mut pull = |store: &mut Store, clock: Instant| -> usize {
        let mut bytes = 0;
        while let Pull::Chunk(c) = engine.pull(store, epoch, applied, clock, ctx) {
            bytes += c.data.len();
            applied = Some((c.frame, c.index));
        }
        bytes
    };
    pull(&mut store, clock);
    // 改一项：闸发出一次，在等受理与摘要（之后每轮推闸都不会有变化）
    let sent =
        store.step_assistant_edit(ID, |e, b| e.edit(ID, Field::Prompt("新的提示词".into()), b));
    assert!(sent.is_some(), "对照：第一次改动发出去了");
    clock += FRAME_INTERVAL;
    pull(&mut store, clock);

    let mut us = Vec::with_capacity(ROUNDS);
    let mut bytes_total = 0usize;
    let mut frames_with_bytes = 0usize;
    for _ in 0..ROUNDS {
        clock += FRAME_INTERVAL;
        let start = Instant::now();
        let again = store.step_assistant_edit(ID, |e, b| e.observed(ID, b));
        assert!(again.is_none(), "闸忙着、摘要没变：不该再发");
        let bytes = pull(&mut store, clock);
        us.push(start.elapsed().as_secs_f64() * 1e6);
        bytes_total += bytes;
        frames_with_bytes += usize::from(bytes > 0);
    }
    println!(
        "{{\"group\":\"busy_assistant_idle_round\",\"rounds\":{ROUNDS},\"p50_us\":{:.3},\"p90_us\":{:.3},\"bytes_per_round\":{:.1},\"rounds_with_bytes\":{frames_with_bytes}}}",
        pct(&mut us.clone(), 0.5),
        pct(&mut us, 0.9),
        bytes_total as f64 / ROUNDS as f64,
    );
}
