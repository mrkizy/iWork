//! 时间线已满（默认上限 5000 行）之后，持久事件经 `Store::apply` 的单条耗时：
//! `cargo bench -p iwork-bench --bench timeline_apply`。
//!
//! 组 2b 审查第 6 轮（T09 方案 2.2 第 4 条、R14）：`Timeline::fit` 在每次写入之后都跑（同步继续卡片、重算重建
//! 得出的行、查上限），`view` 那条量不到它——那条的持久事件在计时段外。这里只量持久事件这一段，不取帧。
//! 只用 `Store` 的公开接口（`Store::new`、`apply`、订阅与可见声明），新旧两版同一份文件可以对比。
//!
//! 先用真实形状的回合灌满时间线（每个回合：用户一问、排队、若干次工具调用、助手一答、结束，模型交替，
//! 让重建得出的行也有事做），再计时：
//! - `message_appended`：末尾新来一条消息（时间线满着，每条都会从老的一端裁）；
//! - `tool_call_finished`：已加载的工具调用结束（改一行）。
//!
//! 每组一行 JSON 到 stdout。

use std::time::Instant;

use iwork_client_core::store::{Connection, Item, ScopeKind, Store, Visible};
use iwork_proto::pb;
use pb::domain_event::Event as E;

const CONV: &str = "c0";
const SUB: u32 = 2;
/// 灌满用的回合数（每个回合 1 + 1 + TOOLS + 1 + 1 行），超过上限一截，裁剪已经在发生。
const FILL_TURNS: usize = 600;
const TOOLS: usize = 8;
/// 计时的样本数。
const SAMPLES: usize = 2000;

struct Feed {
    store: Store,
    seq: u64,
}

impl Feed {
    fn apply(&mut self, e: E) {
        self.seq += 1;
        self.store.apply(Item::Event(Box::new(pb::Event {
            subscription_ids: vec![SUB],
            body: Some(pb::event::Body::Persisted(pb::PersistedEvent {
                seq: self.seq,
                at: None,
                actor: "assistant".into(),
                event: Some(pb::DomainEvent { event: Some(e) }),
            })),
        })));
    }

    fn timed(&mut self, e: E) -> f64 {
        let t = Instant::now();
        self.apply(e);
        t.elapsed().as_secs_f64() * 1e6
    }
}

fn message(id: &str, user: bool, turn: Option<&str>) -> E {
    let kind = if user {
        pb::author::Kind::User(pb::UserAuthor {})
    } else {
        pb::author::Kind::Assistant(pb::AssistantAuthor {
            assistant_id: "a".into(),
            name_snapshot: "iWork".into(),
        })
    };
    E::MessageAppended(pb::MessageAppended {
        message: Some(pb::Message {
            id: id.into(),
            conversation_id: CONV.into(),
            author: Some(pb::Author { kind: Some(kind) }),
            status: pb::MessageStatus::Completed as i32,
            turn_id: turn.map(Into::into),
            ..Default::default()
        }),
    })
}

fn turn_queued(id: &str, model: &str) -> E {
    E::TurnQueued(pb::TurnQueued {
        turn: Some(pb::Turn {
            id: id.into(),
            conversation_id: CONV.into(),
            assistant_id: "a".into(),
            assistant_version: 1,
            runtime_id: "rt".into(),
            model: Some(pb::ModelSelection {
                model_id: model.into(),
                effort: None,
            }),
            state: pb::TurnState::Queued as i32,
            kind: pb::TurnKind::Assistant as i32,
            ..Default::default()
        }),
    })
}

fn tool_started(turn: &str, call: &str) -> E {
    E::ToolCallStarted(pb::ToolCallStarted {
        turn_id: turn.into(),
        tool_call_id: call.into(),
        tool_name: "Bash".into(),
        action: None,
    })
}

fn tool_finished(turn: &str, call: &str) -> E {
    E::ToolCallFinished(pb::ToolCallFinished {
        turn_id: turn.into(),
        tool_call_id: call.into(),
        ok: true,
        output_artifact: None,
    })
}

fn turn_finished(turn: &str) -> E {
    E::TurnFinished(pb::TurnFinished {
        turn_id: turn.into(),
        state: pb::TurnState::Completed as i32,
        usage: None,
        celebrate: false,
        change_summary: None,
    })
}

fn pct(v: &mut [f64], p: f64) -> f64 {
    v.sort_by(f64::total_cmp);
    if v.is_empty() {
        return 0.0;
    }
    v[((v.len() - 1) as f64 * p).round() as usize]
}

fn report(name: &str, mut us: Vec<f64>, rows: usize) {
    let n = us.len();
    let mean = us.iter().sum::<f64>() / n.max(1) as f64;
    println!(
        "{{\"group\":\"timeline_apply\",\"event\":\"{name}\",\"samples\":{n},\"timeline_rows\":{rows},\"us_p50\":{:.2},\"us_p95\":{:.2},\"us_mean\":{mean:.2}}}",
        pct(&mut us.clone(), 0.5),
        pct(&mut us, 0.95),
    );
}

fn main() {
    let mut store = Store::new();
    store.register_subscription(SUB, ScopeKind::Conversation(CONV.into()));
    store.set_visible(Visible {
        conversation_id: Some(CONV.into()),
        ..Default::default()
    });
    store.apply(Item::Subscribed(pb::Subscribed {
        subscription_id: SUB,
        snapshot: None,
        snapshot_seq: 0,
    }));
    store.set_connection(Connection::Live);
    let mut f = Feed { store, seq: 0 };

    // 灌满：模型每隔一个回合换一次，重建得出的模型切换行跟着有
    for t in 0..FILL_TURNS {
        let turn = format!("t{t}");
        f.apply(message(&format!("q{t}"), true, None));
        f.apply(turn_queued(
            &turn,
            if t % 2 == 0 { "m-large" } else { "m-small" },
        ));
        for i in 0..TOOLS {
            f.apply(tool_started(&turn, &format!("{turn}-c{i}")));
            f.apply(tool_finished(&turn, &format!("{turn}-c{i}")));
        }
        f.apply(message(&format!("a{t}"), false, Some(&turn)));
        f.apply(turn_finished(&turn));
    }
    let rows = f.store.open.as_ref().map_or(0, |t| t.len());

    // 末尾新来消息：每条都让时间线超上限、从老的一端裁
    let appended: Vec<f64> = (0..SAMPLES)
        .map(|i| f.timed(message(&format!("n{i}"), i % 2 == 0, None)))
        .collect();
    report("message_appended", appended, rows);

    // 一个还在跑的回合：先开好 SAMPLES 个工具调用（都在加载范围里，旧的一端被裁掉的是更早的行），再逐个结束
    f.apply(turn_queued("live", "m-large"));
    let calls: Vec<String> = (0..SAMPLES).map(|i| format!("live-c{i}")).collect();
    for c in &calls {
        f.apply(tool_started("live", c));
    }
    let finished: Vec<f64> = calls
        .iter()
        .map(|c| f.timed(tool_finished("live", c)))
        .collect();
    let rows = f.store.open.as_ref().map_or(0, |t| t.len());
    report("tool_call_finished", finished, rows);
}
