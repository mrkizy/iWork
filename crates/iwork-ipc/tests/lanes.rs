//! 入站请求的容量与取消（审查 F-5、F-6；方案 2.7、2.9）。

mod support;

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use bytes::BytesMut;
use iwork_ipc::client::IncomingItem;
use iwork_ipc::frame::{Frame, encode_frame};
use iwork_ipc::transport::{Connection, Connector};
use iwork_proto::pb;
use support::*;
use tokio::io::AsyncWriteExt;

const PAD: usize = 4000;

fn frame_bytes(f: &Frame) -> Vec<u8> {
    let mut b = BytesMut::new();
    encode_frame(f, &mut b);
    b.to_vec()
}

fn request(id: u64, kind: usize) -> Frame {
    let pad = "x".repeat(PAD);
    let body = match kind % 3 {
        0 => pb::request::Body::Command(pb::Command {
            command_id: pad,
            kind: None,
        }),
        1 => pb::request::Body::Query(pb::Query {
            kind: Some(pb::query::Kind::ListModels(pb::ListModels {
                runtime_id: pad,
            })),
        }),
        _ => pb::request::Body::Query(pb::Query {
            kind: Some(pb::query::Kind::GetConversation(pb::GetConversation {
                conversation_id: pad,
                ..Default::default()
            })),
        }),
    };
    Frame::Request(pb::Request {
        request_id: id,
        body: Some(body),
    })
}

/// handler 很慢、客户端不等响应持续发请求（命令、并行查询、会写视图的查询混着发）：后台只接受
/// `max_pending_requests` 个，之后停止读 socket，客户端被自己堵住；后台的积压有上界。
#[tokio::test]
async fn pending_requests_are_bounded_under_slow_handler() {
    let mut cfg = server_cfg();
    cfg.max_pending_requests = 8;
    let h = start(cfg);
    *h.handler.command_delay.lock().unwrap() = Duration::from_secs(30);
    *h.handler.side_query_delay.lock().unwrap() = Duration::from_secs(30);
    *h.handler.view_query_delay.lock().unwrap() = Duration::from_secs(30);
    let conn = h.connector.connect(&h.endpoint).await.unwrap();
    let (_rd, mut wr) = conn.into_split();
    wr.write_all(&frame_bytes(&Frame::Hello(pb::Hello {
        protocol_major: iwork_proto::PROTOCOL_MAJOR,
        ..Default::default()
    })))
    .await
    .unwrap();
    let written = Arc::new(AtomicUsize::new(0));
    let total = 300;
    let writer = {
        let written = written.clone();
        tokio::spawn(async move {
            for i in 0..total {
                if wr
                    .write_all(&frame_bytes(&request(i as u64 + 1, i)))
                    .await
                    .is_err()
                {
                    return;
                }
                written.fetch_add(1, Ordering::SeqCst);
            }
        })
    };
    tokio::time::sleep(Duration::from_secs(1)).await;
    let n = written.load(Ordering::SeqCst);
    // 8 个被接受 + 解码缓冲里的一两帧 + 内存管道 64KiB（约 16 帧）。没有上限时 300 条全写得进去。
    eprintln!("lanes: 后台停读前客户端写出了 {n} 条请求（上限 8）");
    assert!(n < 40, "后台没有停止读：客户端写出了 {n} 条");
    assert!(n > 8, "对照：上限之内的请求照常被接受");
    assert!(!writer.is_finished());
    writer.abort();
}

/// 准入票随响应释放：上限很小时，一串命令照样按顺序全部完成（停读之后能恢复读）。
#[tokio::test]
async fn admission_frees_as_requests_complete() {
    let mut cfg = server_cfg();
    cfg.max_pending_requests = 2;
    let h = start(cfg);
    *h.handler.command_delay.lock().unwrap() = Duration::from_millis(5);
    let (client, _inc) = h.connect_ok().await;
    let client = Arc::new(client);
    let mut tasks = Vec::new();
    for i in 0..20 {
        let c = client.clone();
        tasks.push(tokio::spawn(async move {
            c.request(command(&format!("c{i:02}"))).await
        }));
        tokio::task::yield_now().await;
    }
    for t in tasks {
        t.await.unwrap().unwrap();
    }
    let got = h.handler.commands.lock().unwrap().clone();
    let want: Vec<String> = (0..20).map(|i| format!("c{i:02}")).collect();
    assert_eq!(got, want);
}

/// 会写视图的查询永不返回：交接超时关连接后，handler 任务被中止（在飞计数回到 0）。
#[tokio::test]
async fn handover_timeout_aborts_never_returning_handler() {
    let mut cfg = server_cfg();
    cfg.handover_timeout = Duration::from_millis(50);
    let h = start(cfg);
    let (client, mut inc) = h.connect_ok().await;
    *h.world.read_hangs.lock().unwrap() = true;
    assert!(client.view_query(get_conversation("c1")).await.is_err());
    assert!(matches!(next_item(&mut inc).await, IncomingItem::Closed(_)));
    assert!(
        wait_count(&h.handler.in_flight, 0, Duration::from_secs(1)).await,
        "连接关了，永不返回的 handler 还在跑：{}",
        h.handler.in_flight.load(Ordering::SeqCst)
    );
}

/// 会写视图的查询比交接期限慢：连接关闭后 handler 被中止，读完之后的代码不会再执行。
#[tokio::test]
async fn handover_timeout_aborts_late_handler_before_it_finishes() {
    let mut cfg = server_cfg();
    cfg.handover_timeout = Duration::from_millis(20);
    let h = start(cfg);
    let (client, mut inc) = h.connect_ok().await;
    *h.world.read_delay.lock().unwrap() = Duration::from_millis(150);
    assert!(client.view_query(get_conversation("c1")).await.is_err());
    assert!(matches!(next_item(&mut inc).await, IncomingItem::Closed(_)));
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert_eq!(
        h.handler.reads_finished.load(Ordering::SeqCst),
        0,
        "迟到的 handler 不该跑完"
    );
    assert_eq!(h.handler.in_flight.load(Ordering::SeqCst), 0);
}

/// 连接关闭时在飞的命令与并行查询的 handler 也一起中止。
#[tokio::test]
async fn connection_close_aborts_in_flight_command_and_side_query() {
    let h = start(server_cfg());
    *h.handler.command_delay.lock().unwrap() = Duration::from_secs(30);
    *h.handler.side_query_delay.lock().unwrap() = Duration::from_secs(30);
    let (client, inc) = h.connect_ok().await;
    let client = Arc::new(client);
    let (c1, c2) = (client.clone(), client.clone());
    let cmd = tokio::spawn(async move { c1.request(command("slow")).await });
    let side = tokio::spawn(async move {
        c2.request(pb::request::Body::Query(pb::Query {
            kind: Some(pb::query::Kind::ListModels(pb::ListModels::default())),
        }))
        .await
    });
    assert!(
        wait_count(&h.handler.in_flight, 2, Duration::from_secs(1)).await,
        "两个 handler 应在飞"
    );
    // 客户端整体断开。
    cmd.abort();
    side.abort();
    drop(client);
    drop(inc);
    assert!(
        wait_count(&h.handler.in_flight, 0, Duration::from_secs(2)).await,
        "连接关了，命令与查询的 handler 还在跑：{}",
        h.handler.in_flight.load(Ordering::SeqCst)
    );
    assert!(wait_count(&h.handler.closed, 1, Duration::from_secs(1)).await);
}
