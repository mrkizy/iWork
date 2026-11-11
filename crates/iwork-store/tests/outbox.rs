//! T04 方案 2.11：按去向挑待投递的条目；把停在「已派发」的条目收尾。

mod support;

use iwork_ports::store::{
    CommitRequest, DeliveryState, OutboxId, OutboxItem, OutboxSelector, OutboxTarget,
    RequeueReport, StoragePort,
};
use support::*;

/// 一次提交写入这些去向的条目（id 按顺序从 `first_id` 起）。
fn enqueue(store: &Store, n: u128, targets: &[&str]) {
    let mut req: CommitRequest = cmd(n, vec![]);
    req.outbox = targets.iter().map(|t| outbox(t, t.as_bytes())).collect();
    block(store.commit(req)).unwrap();
}

fn targets(items: &[OutboxItem]) -> Vec<String> {
    items.iter().map(|i| i.target.0.clone()).collect()
}

fn state_of(env: &Env, id: u64) -> (String, i64) {
    let conn = rusqlite::Connection::open(env.db()).unwrap();
    conn.query_row(
        "SELECT state, attempts FROM outbox WHERE id = ?1",
        [id as i64],
        |r| Ok((r.get(0)?, r.get(1)?)),
    )
    .unwrap()
}

/// 排在 70 条别的去向后面的那一条也挑得出来：筛选在存储里做，不靠先取全局前 N 条。
#[test]
fn outbox_pending_where_prefix_reaches_past_other_targets() {
    let env = Env::new();
    let store = env.open();
    let many: Vec<String> = (0..70).map(|i| format!("runtime:a/{i}")).collect();
    let refs: Vec<&str> = many.iter().map(String::as_str).collect();
    enqueue(&store, 1, &refs);
    enqueue(&store, 2, &["bridge:x", "runtime:b"]);

    let got =
        block(store.outbox_pending_where(OutboxSelector::Prefix("bridge:".into()), 16)).unwrap();
    assert_eq!(targets(&got), ["bridge:x"]);
    // 对照：全局前 16 条里没有它
    let first16 = block(store.outbox_pending(16)).unwrap();
    assert!(!targets(&first16).contains(&"bridge:x".to_string()));

    let runtime =
        block(store.outbox_pending_where(OutboxSelector::Prefix("runtime:".into()), 100)).unwrap();
    assert_eq!(runtime.len(), 71);
    assert!(runtime.windows(2).all(|w| w[0].id < w[1].id), "按 id 递增");
    let limited =
        block(store.outbox_pending_where(OutboxSelector::Prefix("runtime:".into()), 5)).unwrap();
    assert_eq!(limited.len(), 5);
}

#[test]
fn outbox_pending_where_none_of_picks_unclaimed() {
    let env = Env::new();
    let store = env.open();
    enqueue(
        &store,
        1,
        &["runtime:1", "bridge:1", "mystery:1", "runtime:2", "other"],
    );
    let got = block(store.outbox_pending_where(
        OutboxSelector::NoneOf(vec!["runtime:".into(), "bridge:".into()]),
        64,
    ))
    .unwrap();
    assert_eq!(targets(&got), ["mystery:1", "other"]);
    // 空列表 = 全部
    let all = block(store.outbox_pending_where(OutboxSelector::NoneOf(vec![]), 64)).unwrap();
    assert_eq!(all.len(), 5);
}

#[test]
fn outbox_pending_where_only_returns_queued() {
    let env = Env::new();
    let store = env.open();
    enqueue(&store, 1, &["runtime:1", "runtime:2", "runtime:3"]);
    block(store.outbox_mark(OutboxId(1), DeliveryState::Dispatched)).unwrap();
    block(store.outbox_mark(OutboxId(2), DeliveryState::Failed)).unwrap();
    let got =
        block(store.outbox_pending_where(OutboxSelector::Prefix("runtime:".into()), 64)).unwrap();
    assert_eq!(targets(&got), ["runtime:3"]);
}

/// SQL 里的筛选与 `OutboxSelector::matches` 是同一个语义，含前缀里的 `%`、`_` 与空前缀。
#[test]
fn outbox_selector_agrees_with_sql() {
    let env = Env::new();
    let store = env.open();
    let all = [
        "a%b:1", "axb:1", "a_c", "abc", "a", "", "ab", "b:1", "a%", "ä:1", "äx",
    ];
    enqueue(&store, 1, &all);
    let selectors = [
        OutboxSelector::Prefix("a%".into()),
        OutboxSelector::Prefix("a_".into()),
        OutboxSelector::Prefix("a".into()),
        OutboxSelector::Prefix("".into()),
        OutboxSelector::Prefix("ä".into()),
        OutboxSelector::Prefix("abcd".into()),
        OutboxSelector::NoneOf(vec!["a".into()]),
        OutboxSelector::NoneOf(vec!["a%".into(), "b:".into()]),
        OutboxSelector::NoneOf(vec!["".into()]),
        OutboxSelector::NoneOf(vec!["ä".into(), "x".into()]),
    ];
    for sel in selectors {
        let from_sql = targets(&block(store.outbox_pending_where(sel.clone(), 100)).unwrap());
        let expected: Vec<String> = all
            .iter()
            .filter(|t| sel.matches(&OutboxTarget((**t).to_string())))
            .map(|t| t.to_string())
            .collect();
        assert_eq!(from_sql, expected, "{sel:?}");
    }
}

#[test]
fn outbox_requeue_dispatched_by_prefix() {
    let env = Env::new();
    let store = env.open();
    enqueue(
        &store,
        1,
        &[
            "runtime:1",
            "runtime:2",
            "bridge:1",
            "mystery:1",
            "runtime:3",
        ],
    );
    for id in [1, 3, 4, 5] {
        block(store.outbox_mark(OutboxId(id), DeliveryState::Dispatched)).unwrap();
    }
    block(store.outbox_mark(OutboxId(5), DeliveryState::AcceptedByRuntime)).unwrap();

    let report =
        block(store.outbox_requeue_dispatched(vec!["runtime:".into(), "bridge:".into()])).unwrap();
    assert_eq!(
        report,
        RequeueReport {
            requeued: 2,
            failed: 1
        }
    );
    assert_eq!(
        state_of(&env, 1),
        ("queued".into(), 1),
        "可重投：回到 queued，次数不变"
    );
    assert_eq!(
        state_of(&env, 2),
        ("queued".into(), 0),
        "本来就是 queued：不动"
    );
    assert_eq!(state_of(&env, 3), ("queued".into(), 1));
    assert_eq!(
        state_of(&env, 4),
        ("failed".into(), 1),
        "没有去向认领：标 failed"
    );
    assert_eq!(
        state_of(&env, 5),
        ("accepted_by_runtime".into(), 1),
        "已有结果的不动"
    );

    // 幂等：再跑一次什么都不改
    let again = block(store.outbox_requeue_dispatched(vec!["runtime:".into()])).unwrap();
    assert_eq!(again, RequeueReport::default());
}

#[test]
fn outbox_requeue_dispatched_without_prefixes_fails_everything_in_flight() {
    let env = Env::new();
    let store = env.open();
    enqueue(&store, 1, &["runtime:1", "bridge:1"]);
    block(store.outbox_mark(OutboxId(1), DeliveryState::Dispatched)).unwrap();
    block(store.outbox_mark(OutboxId(2), DeliveryState::Dispatched)).unwrap();
    let report = block(store.outbox_requeue_dispatched(vec![])).unwrap();
    assert_eq!(
        report,
        RequeueReport {
            requeued: 0,
            failed: 2
        }
    );
    assert_eq!(state_of(&env, 1).0, "failed");
}
