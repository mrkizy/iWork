//! 每连接一条事件流：续传 / 快照、过滤、交接（验收 4 的单测部分；方案 2.8、7.1、7.2「流」「交接」）。

mod support;

use std::sync::atomic::Ordering;
use std::time::Duration;

use iwork_ipc::client::{ClientError, IncomingItem};
use iwork_proto::pb;
use support::*;

fn subscribed(item: &IncomingItem) -> &pb::Subscribed {
    match item {
        IncomingItem::Subscribed { subscribed, .. } => subscribed,
        other => panic!("期望 Subscribed，得到 {other:?}"),
    }
}

fn event_ids(item: &IncomingItem) -> (u64, Vec<u32>) {
    match item {
        IncomingItem::Event(pb::Event {
            subscription_ids,
            body: Some(pb::event::Body::Persisted(p)),
        }) => (p.seq, subscription_ids.clone()),
        other => panic!("期望持久事件，得到 {other:?}"),
    }
}

/// 收 n 条持久事件的 seq。
async fn take_seqs(inc: &mut iwork_ipc::client::Incoming, n: usize) -> Vec<u64> {
    let mut v = Vec::new();
    while v.len() < n {
        v.push(next_persisted(inc).await);
    }
    v
}

async fn assert_quiet(inc: &mut iwork_ipc::client::Incoming) {
    if let Ok(Some(item)) = tokio::time::timeout(Duration::from_millis(80), inc.next()).await {
        panic!("不该再有东西：{item:?}");
    }
}

#[tokio::test]
async fn subscribe_from_zero_gets_snapshot_then_live() {
    let h = start(server_cfg());
    h.upsert("project:p1", 1);
    h.upsert("conversation:c1", 2);
    let (client, mut inc) = h.connect_ok().await;
    client.subscribe(1, sidebar(), 0).await.unwrap();
    let s = subscribed(&next_item(&mut inc).await).clone();
    assert_eq!(s.snapshot_seq, 2);
    assert_eq!(s.snapshot.unwrap().results.len(), 1, "一个活动范围一部分");
    let seq = h.upsert("project:p1", 3);
    assert_eq!(next_persisted(&mut inc).await, seq);
}

#[tokio::test]
async fn first_subscribe_with_cursor_resumes_without_snapshot() {
    let h = start(server_cfg());
    for i in 1..=10 {
        h.upsert(
            if i % 2 == 0 {
                "project:p1"
            } else {
                "conversation:c9"
            },
            i,
        );
    }
    let (client, mut inc) = h.connect_ok().await;
    // 重连：游标 4，侧栏范围。
    client.subscribe(1, sidebar(), 4).await.unwrap();
    let s = subscribed(&next_item(&mut inc).await).clone();
    assert!(s.snapshot.is_none(), "游标对得上就续传，不给快照");
    assert_eq!(take_seqs(&mut inc, 6).await, (5..=10).collect::<Vec<_>>());
    assert_eq!(h.source.snapshot_calls.load(Ordering::SeqCst), 0);
    h.upsert("project:p1", 11);
    assert_eq!(next_persisted(&mut inc).await, 11);
}

#[tokio::test]
async fn subscribe_ahead_of_head_or_beyond_limit_gets_snapshot() {
    let mut cfg = server_cfg();
    cfg.replay_limit = 5;
    let h = start(cfg);
    for i in 1..=20 {
        h.upsert("project:p1", i);
    }
    let (c1, mut i1) = h.connect_ok().await;
    c1.subscribe(1, sidebar(), 99).await.unwrap();
    assert_eq!(
        subscribed(&next_item(&mut i1).await).snapshot_seq,
        20,
        "超前的游标走快照"
    );
    let (c2, mut i2) = h.connect_ok().await;
    c2.subscribe(1, sidebar(), 10).await.unwrap();
    assert_eq!(
        subscribed(&next_item(&mut i2).await).snapshot_seq,
        20,
        "落后超过 replay_limit 走快照"
    );
    let (c3, mut i3) = h.connect_ok().await;
    c3.subscribe(1, sidebar(), 16).await.unwrap();
    assert!(
        subscribed(&next_item(&mut i3).await).snapshot.is_none(),
        "对照：落后 4 条续传"
    );
    assert_eq!(take_seqs(&mut i3, 4).await, vec![17, 18, 19, 20]);
}

/// 补发的上界是登记时的 head；补发期间的新提交经缓冲交接送达，不重不漏。
#[tokio::test]
async fn replay_terminates_under_continuous_commits() {
    let mut cfg = server_cfg();
    cfg.replay_page = 3;
    let h = start(cfg);
    for i in 1..=30 {
        h.upsert("project:p1", i);
    }
    *h.world.read_delay.lock().unwrap() = Duration::from_millis(5);
    let (client, mut inc) = h.connect_ok().await;
    client.subscribe_nowait(1, sidebar(), 1).unwrap();
    // 补发进行中持续提交。
    for i in 31..=60 {
        h.upsert("project:p1", i);
        tokio::time::sleep(Duration::from_millis(1)).await;
    }
    assert!(subscribed(&next_item(&mut inc).await).snapshot.is_none());
    assert_eq!(take_seqs(&mut inc, 59).await, (2..=60).collect::<Vec<_>>());
    assert_quiet(&mut inc).await;
}

/// 页内过滤后为空也继续：靠 next_from 判结束，不是「不足一页」。
#[tokio::test]
async fn replay_uses_next_from_not_count() {
    let mut cfg = server_cfg();
    cfg.replay_page = 2;
    let h = start(cfg);
    for i in 1..=10 {
        h.upsert("conversation:other", i);
    }
    h.upsert("conversation:c1", 11);
    let (client, mut inc) = h.connect_ok().await;
    client.subscribe(1, conversation("c1"), 1).await.unwrap();
    assert!(subscribed(&next_item(&mut inc).await).snapshot.is_none());
    assert_eq!(next_persisted(&mut inc).await, 11);
}

#[tokio::test]
async fn event_carries_all_matching_scope_ids_and_is_sent_once() {
    let h = start(server_cfg());
    let (client, mut inc) = h.connect_ok().await;
    client.subscribe(1, sidebar(), 0).await.unwrap();
    next_item(&mut inc).await;
    client.subscribe(2, conversation("c1"), 0).await.unwrap();
    next_item(&mut inc).await;
    h.upsert("conversation:c1", 1);
    h.upsert("conversation:c2", 2);
    assert_eq!(event_ids(&next_item(&mut inc).await), (1, vec![1, 2]));
    assert_eq!(event_ids(&next_item(&mut inc).await), (2, vec![1]));
    assert_quiet(&mut inc).await;
}

#[tokio::test]
async fn unsubscribe_narrows_filter_without_touching_cursor() {
    let h = start(server_cfg());
    let (client, mut inc) = h.connect_ok().await;
    client.subscribe(1, conversation("c1"), 0).await.unwrap();
    next_item(&mut inc).await;
    client.subscribe(2, conversation("c2"), 0).await.unwrap();
    next_item(&mut inc).await;
    h.upsert("conversation:c1", 1);
    h.upsert("conversation:c2", 2);
    assert_eq!(take_seqs(&mut inc, 2).await, vec![1, 2]);
    client.unsubscribe(2).await.unwrap();
    h.upsert("conversation:c2", 3);
    h.upsert("conversation:c1", 4);
    assert_eq!(next_persisted(&mut inc).await, 4);
    // 退订到空：游标保留，再用同一游标订阅可以续传。
    client.unsubscribe(1).await.unwrap();
    h.upsert("conversation:c1", 5);
    client.subscribe(3, conversation("c1"), 4).await.unwrap();
    assert!(
        subscribed(&next_item(&mut inc).await).snapshot.is_none(),
        "退订到空后游标仍在，续传"
    );
    assert_eq!(next_persisted(&mut inc).await, 5);
}

#[tokio::test]
async fn unsubscribe_unknown_is_not_found() {
    let h = start(server_cfg());
    let (client, _inc) = h.connect_ok().await;
    match client.unsubscribe(42).await {
        Err(ClientError::Server(e)) => assert_eq!(e.code(), pb::ErrorCode::NotFound),
        other => panic!("{other:?}"),
    }
}

#[tokio::test]
async fn empty_scope_is_invalid_argument() {
    let h = start(server_cfg());
    let (client, _inc) = h.connect_ok().await;
    match client
        .subscribe(1, pb::SubscriptionScope::default(), 0)
        .await
    {
        Err(ClientError::Server(e)) => assert_eq!(e.code(), pb::ErrorCode::InvalidArgument),
        other => panic!("{other:?}"),
    }
}

/// Live 之后加一个范围：游标对不上 → 全范围快照，游标推到 snapshot_seq，之后流仍单调。
#[tokio::test]
async fn add_scope_after_live_takes_snapshot_and_keeps_stream_monotonic() {
    let h = start(server_cfg());
    let (client, mut inc) = h.connect_ok().await;
    client.subscribe(1, conversation("c1"), 0).await.unwrap();
    next_item(&mut inc).await;
    h.upsert("conversation:c1", 1);
    h.upsert("conversation:c2", 2);
    assert_eq!(next_persisted(&mut inc).await, 1);
    client.subscribe(2, conversation("c2"), 0).await.unwrap();
    let s = subscribed(&next_item(&mut inc).await).clone();
    assert_eq!(s.snapshot_seq, 2);
    assert_eq!(s.snapshot.unwrap().results.len(), 2, "快照覆盖全部活动范围");
    h.upsert("conversation:c2", 3);
    assert_eq!(next_persisted(&mut inc).await, 3);
}

#[tokio::test]
async fn resubscribe_same_id_with_cursor_resumes_other_from_takes_snapshot() {
    let h = start(server_cfg());
    let (client, mut inc) = h.connect_ok().await;
    client.subscribe(1, conversation("c1"), 0).await.unwrap();
    next_item(&mut inc).await;
    h.upsert("conversation:c1", 1);
    assert_eq!(next_persisted(&mut inc).await, 1);
    client.subscribe(1, conversation("c1"), 1).await.unwrap();
    assert!(
        subscribed(&next_item(&mut inc).await).snapshot.is_none(),
        "from == cursor：续传"
    );
    client.subscribe(1, conversation("c1"), 0).await.unwrap();
    assert!(
        subscribed(&next_item(&mut inc).await).snapshot.is_some(),
        "from ≠ cursor：快照"
    );
}

#[tokio::test]
async fn snapshot_is_single_read_transaction_or_retried() {
    let h = start(server_cfg());
    h.upsert("project:p1", 1);
    *h.world.snapshot_tear_once.lock().unwrap() = true;
    let (client, mut inc) = h.connect_ok().await;
    client.subscribe(1, sidebar(), 0).await.unwrap();
    let s = subscribed(&next_item(&mut inc).await).clone();
    assert!(
        s.snapshot
            .unwrap()
            .results
            .iter()
            .all(|r| r.seq == s.snapshot_seq),
        "客户端看不到两个 seq 的拼接"
    );
    assert_eq!(
        h.source.snapshot_calls.load(Ordering::SeqCst),
        2,
        "撕裂的那次被丢弃重取"
    );
}

/// 同一次读到的多条同 from_seq 的 Subscribe 合并成一次续传，过滤是并集。
#[tokio::test]
async fn subscribes_in_one_read_merge_into_one_resume() {
    let h = start(server_cfg());
    for (i, e) in [
        "conversation:c1",
        "conversation:c2",
        "project:p1",
        "conversation:c3",
    ]
    .iter()
    .enumerate()
    {
        h.upsert(e, i as u64);
    }
    let (client, mut inc) = h.connect_ok().await;
    // 重连：三个范围一次写出，游标 1。
    let rx: Vec<_> = [conversation("c1"), conversation("c2"), conversation("c3")]
        .into_iter()
        .enumerate()
        .map(|(i, s)| client.subscribe_nowait(i as u32 + 1, s, 1).unwrap())
        .collect();
    for r in rx {
        r.await.unwrap().unwrap();
    }
    let mut seqs = Vec::new();
    let mut acks = 0;
    while acks < 3 || seqs.len() < 2 {
        match next_item(&mut inc).await {
            IncomingItem::Subscribed { subscribed, .. } => {
                assert!(subscribed.snapshot.is_none(), "合并后是一次续传，不是快照");
                acks += 1;
            }
            item => seqs.push(event_ids(&item).0),
        }
    }
    assert_eq!(seqs, vec![2, 4]);
    assert_eq!(h.source.snapshot_calls.load(Ordering::SeqCst), 0);
    assert_eq!(
        h.source.replay_calls.load(Ordering::SeqCst),
        1,
        "一次补发覆盖并集"
    );
}

/// 续传结束后才到的 Subscribe，from_seq 已不等于游标 → 快照（正确但多做一次）。
#[tokio::test]
async fn subscribe_in_later_read_after_resume_takes_snapshot() {
    let h = start(server_cfg());
    h.upsert("conversation:c1", 1);
    h.upsert("conversation:c2", 2);
    let (client, mut inc) = h.connect_ok().await;
    client.subscribe(1, conversation("c1"), 0).await.unwrap();
    next_item(&mut inc).await;
    h.upsert("conversation:c1", 3);
    assert_eq!(next_persisted(&mut inc).await, 3);
    client.subscribe(2, conversation("c2"), 1).await.unwrap();
    assert!(subscribed(&next_item(&mut inc).await).snapshot.is_some());
}

#[tokio::test]
async fn too_many_scopes_is_protocol_violation() {
    let mut cfg = server_cfg();
    cfg.max_scopes = 3;
    let h = start(cfg);
    let (client, mut inc) = h.connect_ok().await;
    for i in 0..3 {
        client
            .subscribe(i, conversation(&format!("c{i}")), 0)
            .await
            .unwrap();
        next_item(&mut inc).await;
    }
    let _ = client.subscribe(9, conversation("c9"), 0).await;
    match next_item(&mut inc).await {
        IncomingItem::ServerError(e) => assert_eq!(e.code(), pb::ErrorCode::MalformedFrame),
        other => panic!("{other:?}"),
    }
    assert!(matches!(next_item(&mut inc).await, IncomingItem::Closed(_)));
}

// ------------------------------------------------------------ 交接（会写视图的查询）

/// 查询在 S 读到结果后、返回前提交了 S+1：客户端收到的顺序是 结果、S+1。
#[tokio::test]
async fn view_query_result_sits_at_its_seq_in_stream() {
    let h = start(server_cfg());
    let (client, mut inc) = h.connect_ok().await;
    client.subscribe(1, conversation("c1"), 0).await.unwrap();
    next_item(&mut inc).await;
    for i in 1..=10 {
        h.upsert("conversation:c1", i);
    }
    let (world, publisher) = (h.world.clone(), h.publisher.clone());
    *h.handler.after_read.lock().unwrap() = Some(Box::new(move || {
        world.commit_publish(
            &publisher,
            Op::Upsert {
                entity: "conversation:c1".into(),
                value: 11,
            },
        );
    }));
    client.view_query(get_conversation("c1")).await.unwrap();
    let mut order = Vec::new();
    while order.len() < 12 {
        match next_item(&mut inc).await {
            IncomingItem::ViewResult { result, .. } => order.push(format!("R{}", result.seq)),
            item => order.push(format!("E{}", event_ids(&item).0)),
        }
    }
    let expect: Vec<String> = (1..=10)
        .map(|i| format!("E{i}"))
        .chain(["R10".into(), "E11".into()])
        .collect();
    assert_eq!(order, expect);
}

/// 读事务看到 S，但 S 的发布晚 50ms：结果要等 S 发出之后才发（不会排到 S 前面）。
#[tokio::test]
async fn handover_waits_for_published_head() {
    let h = start(server_cfg());
    let (client, mut inc) = h.connect_ok().await;
    client.subscribe(1, conversation("c1"), 0).await.unwrap();
    next_item(&mut inc).await;
    let late = h.world.commit(Op::Upsert {
        entity: "conversation:c1".into(),
        value: 1,
    });
    let publisher = h.publisher.clone();
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(50)).await;
        publisher.publish(iwork_ipc::server::Published::Persisted(late));
    });
    client.view_query(get_conversation("c1")).await.unwrap();
    assert_eq!(next_persisted(&mut inc).await, 1);
    assert!(
        matches!(next_item(&mut inc).await, IncomingItem::ViewResult { result, .. } if result.seq == 1)
    );
}

#[tokio::test]
async fn view_query_error_resumes_live_without_cursor_change() {
    let h = start(server_cfg());
    let (client, mut inc) = h.connect_ok().await;
    client.subscribe(1, conversation("c1"), 0).await.unwrap();
    next_item(&mut inc).await;
    match client.view_query(get_conversation("missing")).await {
        Err(ClientError::Server(e)) => assert_eq!(e.code(), pb::ErrorCode::NotFound),
        other => panic!("{other:?}"),
    }
    h.upsert("conversation:c1", 1);
    assert_eq!(next_persisted(&mut inc).await, 1, "业务错误之后流照常");
}

#[tokio::test(start_paused = true)]
async fn committed_but_never_published_fails_handover() {
    let (audit, _g) = Captured::install();
    let h = start(server_cfg());
    let (client, mut inc) = h.connect_ok().await;
    client.subscribe(1, conversation("c1"), 0).await.unwrap();
    next_item(&mut inc).await;
    h.world.commit(Op::Upsert {
        entity: "conversation:c1".into(),
        value: 1,
    }); // 永不发布
    assert!(client.view_query(get_conversation("c1")).await.is_err());
    assert!(
        matches!(next_item(&mut inc).await, IncomingItem::Closed(_)),
        "迟到的结果不出现，连接被关"
    );
    let failed = audit.events("connection_failed");
    assert!(
        failed
            .iter()
            .any(|e| e["cause"] == "PublishStalled" && e["level"] == "ERROR"),
        "{failed:?}"
    );
}

#[tokio::test(start_paused = true)]
async fn read_never_returns_fails_handover_without_buffer_pressure() {
    let (audit, _g) = Captured::install();
    let h = start(server_cfg());
    let (client, mut inc) = h.connect_ok().await;
    client.subscribe(1, conversation("c1"), 0).await.unwrap();
    next_item(&mut inc).await;
    *h.world.read_hangs.lock().unwrap() = true;
    assert!(client.view_query(get_conversation("c1")).await.is_err());
    assert!(matches!(next_item(&mut inc).await, IncomingItem::Closed(_)));
    assert!(
        audit
            .events("connection_failed")
            .iter()
            .any(|e| e["cause"] == "HandoverTimeout")
    );
    // 新连接不受影响。
    *h.world.read_hangs.lock().unwrap() = false;
    let (c2, mut i2) = h.connect_ok().await;
    c2.subscribe(1, conversation("c1"), 0).await.unwrap();
    assert!(matches!(
        next_item(&mut i2).await,
        IncomingItem::Subscribed { .. }
    ));
}

/// 非空库重启、零新增提交：第一次交接立即成功（发布水位以库里的 head 为基线）。
#[tokio::test(start_paused = true)]
async fn published_head_baseline_from_store() {
    let world = World::default();
    for i in 1..=5 {
        world.commit(Op::Upsert {
            entity: "conversation:c1".into(),
            value: i,
        });
    }
    let h = start_with(
        server_cfg(),
        world,
        server_own(),
        client_peer(),
        std::sync::Arc::new(|_| server_peer()),
    );
    let (client, mut inc) = h.connect_ok().await;
    let t = tokio::time::Instant::now();
    client.view_query(get_conversation("c1")).await.unwrap();
    assert!(
        matches!(next_item(&mut inc).await, IncomingItem::ViewResult { result, .. } if result.seq == 5)
    );
    assert!(t.elapsed() < Duration::from_secs(1), "没有等交接超时");
    client.subscribe(1, sidebar(), 0).await.unwrap();
    assert_eq!(subscribed(&next_item(&mut inc).await).snapshot_seq, 5);
}

/// 一个快照和两个查询同时到：按到达顺序一个接一个，流始终单调。
#[tokio::test]
async fn handover_fifo_serializes_snapshot_and_view_queries() {
    let h = start(server_cfg());
    *h.handler.view_query_delay.lock().unwrap() = Duration::from_millis(20);
    let (client, mut inc) = h.connect_ok().await;
    let client = std::sync::Arc::new(client);
    let sub = client.subscribe_nowait(1, sidebar(), 0).unwrap();
    let (c1, c2) = (client.clone(), client.clone());
    let q1 = tokio::spawn(async move { c1.view_query(get_conversation("c1")).await });
    let q2 = tokio::spawn(async move { c2.view_query(get_conversation("c2")).await });
    for i in 1..=20 {
        h.upsert("project:p1", i);
        tokio::time::sleep(Duration::from_millis(2)).await;
    }
    sub.await.unwrap().unwrap();
    q1.await.unwrap().unwrap();
    q2.await.unwrap().unwrap();
    let mut last = 0;
    let mut payloads = 0;
    while payloads < 3 || last < 20 {
        let item = next_item(&mut inc).await;
        let seq = match &item {
            IncomingItem::Subscribed { subscribed, .. } => {
                payloads += 1;
                subscribed.snapshot_seq
            }
            IncomingItem::ViewResult { result, .. } => {
                payloads += 1;
                result.seq
            }
            other => {
                let s = event_ids(other).0;
                assert!(s > last, "持久事件严格递增：{s} 在 {last} 之后");
                s
            }
        };
        assert!(seq >= last, "全序单调：{seq} < {last}");
        last = seq;
    }
}

#[tokio::test]
#[should_panic(expected = "乱序发布")]
async fn publish_out_of_order_is_reported() {
    let h = start(server_cfg());
    let e2 = h.world.commit(Op::Upsert {
        entity: "project:p".into(),
        value: 2,
    });
    h.publisher
        .publish(iwork_ipc::server::Published::Persisted(e2.clone()));
    h.publisher
        .publish(iwork_ipc::server::Published::Persisted(e2));
}

/// 已提交、未发布的事件被启动基线读到，随后的正常发布不能被当成乱序（基线与「最后发布」是两回事）。
#[tokio::test]
async fn publish_after_baseline_is_not_reported_out_of_order() {
    let world = World::default();
    let e1 = world.commit(Op::Upsert {
        entity: "project:p1".into(),
        value: 1,
    });
    let h = start_with(
        server_cfg(),
        world,
        server_own(),
        client_peer(),
        std::sync::Arc::new(|_| server_peer()),
    );
    let (client, mut inc) = h.connect_ok().await; // 连上说明 serve 已经设好基线
    client.subscribe(1, sidebar(), 1).await.unwrap();
    next_item(&mut inc).await;
    h.publisher
        .publish(iwork_ipc::server::Published::Persisted(e1));
    assert_eq!(h.upsert("project:p1", 2), 2);
    assert_eq!(next_persisted(&mut inc).await, 2);
}
