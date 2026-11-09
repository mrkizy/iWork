//! 慢消费者与背压（验收 5，按方案 7.1 的改写：停止读取的连接被服务端关闭，恢复后重连续传拿全事件）。

mod support;

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use iwork_ipc::client::{Client, Incoming, IncomingItem};
use support::*;

fn small_budget() -> iwork_ipc::server::ServerConfig {
    let mut cfg = server_cfg();
    cfg.budget_limit = 256 * 1024;
    cfg.replay_reserve = 32 * 1024;
    cfg
}

async fn connect_small(h: &Harness) -> (Client, Incoming) {
    let mut cfg = client_cfg();
    cfg.incoming_limit = 64 * 1024;
    Client::connect(&h.connector, &h.endpoint, &client_own(), cfg)
        .await
        .unwrap()
}

fn p95(mut v: Vec<Duration>) -> Duration {
    v.sort();
    v[(v.len() * 95 / 100).min(v.len() - 1)]
}

/// A 停止读取：服务端关 A（QueueFull），B 的推送延迟不受影响；A 恢复后以已应用的游标重连续传，
/// 三个范围一次写出、一次续传，收到的持久事件序列与 B 完全一致、无重复。
#[tokio::test]
async fn queue_full_fails_connection_and_reconnect_resumes() {
    let (audit, _g) = Captured::install();
    let h = start(small_budget());
    *h.world.pad.lock().unwrap() = 1000;
    let scopes = [sidebar(), conversation("c1"), conversation("c2")];

    let (a, mut a_inc) = connect_small(&h).await;
    let (b, mut b_inc) = connect_small(&h).await;
    for (i, s) in scopes.iter().enumerate() {
        a.subscribe(i as u32 + 1, s.clone(), 0).await.unwrap();
        b.subscribe(i as u32 + 1, s.clone(), 0).await.unwrap();
    }
    // 两边都先吃掉三个 Subscribed（快照 seq 0）。
    for _ in 0..3 {
        assert!(matches!(
            next_item(&mut a_inc).await,
            IncomingItem::Subscribed { .. }
        ));
        assert!(matches!(
            next_item(&mut b_inc).await,
            IncomingItem::Subscribed { .. }
        ));
    }

    // B 持续读并记录延迟；A 不读。
    let sent_at: Arc<Mutex<HashMap<u64, Instant>>> = Arc::default();
    let latencies: Arc<Mutex<Vec<Duration>>> = Arc::default();
    let b_seqs: Arc<Mutex<Vec<u64>>> = Arc::default();
    let total = 3000u64;
    let reader = {
        let (sent_at, latencies, b_seqs) = (sent_at.clone(), latencies.clone(), b_seqs.clone());
        tokio::spawn(async move {
            while (b_seqs.lock().unwrap().len() as u64) < total {
                let item = next_item(&mut b_inc).await;
                let seq = persisted_seq(&item).expect("B 只该收到持久事件");
                let t = sent_at.lock().unwrap()[&seq];
                latencies.lock().unwrap().push(t.elapsed());
                b_seqs.lock().unwrap().push(seq);
            }
            b_inc
        })
    };
    // 1000 事件 / 秒，3 秒。
    let entities = ["conversation:c1", "conversation:c2", "project:p1"];
    let start = Instant::now();
    for i in 0..total {
        let e = entities[(i % 3) as usize];
        let seq = h.world.commit(Op::Upsert {
            entity: e.into(),
            value: i,
        });
        sent_at.lock().unwrap().insert(seq.seq(), Instant::now());
        h.publisher
            .publish(iwork_ipc::server::Published::Persisted(seq));
        let due = start + Duration::from_micros(1000 * (i + 1));
        if let Some(wait) = due.checked_duration_since(Instant::now()) {
            tokio::time::sleep(wait).await;
        }
    }
    let _b_inc = tokio::time::timeout(Duration::from_secs(20), reader)
        .await
        .unwrap()
        .unwrap();
    let b_seqs = b_seqs.lock().unwrap().clone();
    assert_eq!(b_seqs, (1..=total).collect::<Vec<_>>());
    let p95_b = p95(latencies.lock().unwrap().clone());
    eprintln!("slow_consumer: B 推送延迟 P95 = {p95_b:?}（{total} 条，A 停止读取）");
    assert!(
        p95_b < Duration::from_millis(5),
        "B 的延迟受了 A 的影响：P95 {p95_b:?}"
    );

    let failed = audit.events("connection_failed");
    assert!(
        failed.iter().any(|e| e["cause"] == "QueueFull"),
        "A 应因 QueueFull 被关：{failed:?}"
    );

    // A 恢复读取：先把排着的应用完（游标只在应用时推进），直到 Closed。
    let mut a_seqs = Vec::new();
    loop {
        match next_item(&mut a_inc).await {
            IncomingItem::Closed(_) => break,
            item => a_seqs.push(persisted_seq(&item).expect("只有持久事件")),
        }
    }
    let cursor = *a_seqs.last().unwrap_or(&0);
    assert!((cursor as usize) < b_seqs.len(), "A 确实落后了");
    // 重连：三个范围一次写出，游标相同 → 一次续传。
    let replays_before = h
        .source
        .replay_calls
        .load(std::sync::atomic::Ordering::SeqCst);
    let (a2, mut a2_inc) = connect_small(&h).await;
    let acks: Vec<_> = scopes
        .iter()
        .enumerate()
        .map(|(i, s)| {
            a2.subscribe_nowait(i as u32 + 1, s.clone(), cursor)
                .unwrap()
        })
        .collect();
    let mut acks_seen = 0;
    while (a_seqs.len() as u64) < total || acks_seen < 3 {
        match next_item(&mut a2_inc).await {
            IncomingItem::Subscribed { subscribed, .. } => {
                assert!(subscribed.snapshot.is_none(), "同游标重连是续传");
                acks_seen += 1;
            }
            item => a_seqs.push(persisted_seq(&item).unwrap()),
        }
    }
    for r in acks {
        r.await.unwrap().unwrap();
    }
    assert_eq!(
        a_seqs, b_seqs,
        "A 最终收到的持久事件序列与 B 完全一致、无缺无重"
    );
    let replay_pages = h
        .source
        .replay_calls
        .load(std::sync::atomic::Ordering::SeqCst)
        - replays_before;
    assert!(
        replay_pages >= 1 && replay_pages <= (total as usize).div_ceil(1000) + 1,
        "一次续传：{replay_pages} 页"
    );
}

/// 补发进行中，实时事件把缓冲撑满 → 关连接（ReplayBufferFull），另一条连接照常收事件；占用从不超过上限。
#[tokio::test]
async fn buffer_overflow_during_replay_fails_connection_others_unaffected() {
    let (audit, _g) = Captured::install();
    let h = start(small_budget());
    *h.world.pad.lock().unwrap() = 1000;
    for i in 0..100 {
        h.upsert("project:p1", i);
    }
    let (b, mut b_inc) = connect_small(&h).await;
    b.subscribe(1, sidebar(), 0).await.unwrap();
    next_item(&mut b_inc).await;
    // 补发很慢：第一页还没回来。
    *h.world.read_delay.lock().unwrap() = Duration::from_millis(500);
    let (a, mut a_inc) = connect_small(&h).await;
    let _ack = a.subscribe_nowait(1, sidebar(), 1).unwrap();
    tokio::time::sleep(Duration::from_millis(50)).await;
    // B 在自己的任务里照常读。
    let b_reader = tokio::spawn(async move {
        let mut got = Vec::new();
        while got.len() < 400 {
            got.push(next_persisted(&mut b_inc).await);
        }
        got
    });
    for i in 0..400 {
        h.upsert("project:p1", 1000 + i);
        tokio::task::yield_now().await;
    }
    assert_eq!(
        b_reader.await.unwrap(),
        (101..=500).collect::<Vec<_>>(),
        "B 照常收到全部 400 条"
    );
    loop {
        if let IncomingItem::Closed(_) = next_item(&mut a_inc).await {
            break;
        }
    }
    let failed = audit.events("connection_failed");
    assert!(
        failed.iter().any(|e| e["cause"] == "ReplayBufferFull"),
        "{failed:?}"
    );
    for e in &failed {
        let used: usize = e["used_bytes"].parse().unwrap();
        assert!(used <= 256 * 1024, "占用超过上限：{used}");
    }
    // 重连续传成功（读延迟去掉）。
    *h.world.read_delay.lock().unwrap() = Duration::ZERO;
    let (a2, mut a2_inc) = connect_small(&h).await;
    a2.subscribe(1, sidebar(), 1).await.unwrap();
    assert!(matches!(
        next_item(&mut a2_inc).await,
        IncomingItem::Subscribed { .. }
    ));
    let mut last = 1;
    while last < 500 {
        let s = next_persisted(&mut a2_inc).await;
        assert_eq!(s, last + 1);
        last = s;
    }
}

/// 补发往队列放不下、客户端又不读：等预算最多 2s，然后关连接（ReplayReserveTimeout），不无限等。
#[tokio::test(start_paused = true)]
async fn replay_reserve_timeout_fails_connection() {
    let (audit, _g) = Captured::install();
    let h = start(small_budget());
    *h.world.pad.lock().unwrap() = 1000;
    for i in 0..1000 {
        h.upsert("project:p1", i);
    }
    let (a, mut a_inc) = connect_small(&h).await;
    let _ack = a.subscribe_nowait(1, sidebar(), 1).unwrap();
    // 不读 Incoming：客户端 64KiB 满 → 内存管道 64KiB 满 → 服务端队列满 → 补发等预算。
    tokio::time::sleep(Duration::from_secs(5)).await;
    let failed = audit.events("connection_failed");
    assert!(
        failed.iter().any(|e| e["cause"] == "ReplayReserveTimeout"),
        "{failed:?}"
    );
    drop(a);
    loop {
        if let IncomingItem::Closed(_) = next_item(&mut a_inc).await {
            break;
        }
    }
}

/// `Incoming` 按字节有界：消费者停住时占用不超过上限，读任务停读，服务端在预算上限处关连接。
#[tokio::test]
async fn client_incoming_is_bounded_and_backpressures() {
    let (audit, _g) = Captured::install();
    let h = start(small_budget());
    *h.world.pad.lock().unwrap() = 2000;
    let (a, mut a_inc) = connect_small(&h).await;
    a.subscribe(1, sidebar(), 0).await.unwrap();
    next_item(&mut a_inc).await;
    for i in 0..400 {
        h.upsert("project:p1", i);
        tokio::task::yield_now().await;
    }
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert!(
        audit
            .events("connection_failed")
            .iter()
            .any(|e| e["cause"] == "QueueFull")
    );
    // 客户端最多积压 64KiB 的事件（每条约 2KiB）+ 内存管道里的。
    let mut n = 0;
    loop {
        match next_item(&mut a_inc).await {
            IncomingItem::Closed(_) => break,
            _ => n += 1,
        }
    }
    assert!(n < 400 && n > 0, "Incoming 有界：积压 {n} 条");
}
