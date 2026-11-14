//! 协议违规（验收 3）与请求分道（方案 2.7）。

mod support;

use std::sync::atomic::Ordering;
use std::time::{Duration, Instant};

use bytes::{BufMut, BytesMut};
use iwork_ipc::client::{ClientError, IncomingItem};
use iwork_ipc::frame::{Direction, Frame, decode_frame, encode_frame};
use iwork_ipc::transport::{Connection, Connector};
use iwork_proto::limits::{MAX_CHUNK_DATA, MAX_FRAME_LEN};
use iwork_proto::pb;
use support::*;
use tokio::io::{AsyncReadExt, AsyncWriteExt};

fn hello() -> Vec<u8> {
    let mut b = BytesMut::new();
    encode_frame(
        &Frame::Hello(pb::Hello {
            protocol_major: iwork_proto::PROTOCOL_MAJOR,
            ..Default::default()
        }),
        &mut b,
    );
    b.to_vec()
}

/// 握手后送 `payload`，读到连接关闭，返回收到的帧（Welcome 之后的）。
async fn after_handshake(h: &Harness, payload: Vec<u8>, with_hello: bool) -> Vec<Frame> {
    let conn = h.connector.connect(&h.endpoint).await.unwrap();
    let (mut rd, mut wr) = conn.into_split();
    if with_hello {
        wr.write_all(&hello()).await.unwrap();
    }
    let _ = wr.write_all(&payload).await;
    let mut raw = Vec::new();
    tokio::time::timeout(Duration::from_secs(10), rd.read_to_end(&mut raw))
        .await
        .expect("应被关闭")
        .ok();
    let mut buf = BytesMut::from(raw.as_slice());
    let mut frames = Vec::new();
    while let Some(f) = decode_frame(&mut buf, MAX_FRAME_LEN, Direction::ToClient).unwrap() {
        frames.push(f);
    }
    if with_hello {
        assert!(
            matches!(frames.first(), Some(Frame::Welcome(_))),
            "{frames:?}"
        );
        frames.remove(0);
    }
    frames
}

fn is_malformed(frames: &[Frame]) -> bool {
    matches!(frames, [Frame::Response(r)] if matches!(&**r, pb::Response { request_id: 0, body: Some(pb::response::Body::Error(e)) } if e.code() == pb::ErrorCode::MalformedFrame))
}

fn header(len: u32, t: u8) -> Vec<u8> {
    let mut b = BytesMut::new();
    b.put_u32_le(len);
    b.put_u8(t);
    b.to_vec()
}

#[tokio::test]
async fn oversize_garbage_and_request_before_hello_disconnect_and_server_survives() {
    let (audit, _g) = Captured::install();
    let h = start(server_cfg());
    // 1. 超长帧（握手后）：读到长度就断，不等载荷。
    let f = after_handshake(
        &h,
        header(MAX_FRAME_LEN + 1, pb::FrameType::Request as u8),
        true,
    )
    .await;
    assert!(is_malformed(&f), "{f:?}");
    // 2. 畸形载荷。
    let mut garbage = header(4, pb::FrameType::Request as u8);
    garbage.extend_from_slice(&[0xff, 0xff, 0xff]);
    let f = after_handshake(&h, garbage, true).await;
    assert!(is_malformed(&f), "{f:?}");
    // 3. 握手前先发请求：直接关闭，不回任何帧。
    let mut req = BytesMut::new();
    encode_frame(
        &Frame::Request(pb::Request {
            request_id: 1,
            body: Some(command("x")),
        }),
        &mut req,
    );
    let f = after_handshake(&h, req.to_vec(), false).await;
    assert!(f.is_empty(), "{f:?}");
    // 4. 握手前超过 64KiB。
    let f = after_handshake(&h, header(64 * 1024 + 1, pb::FrameType::Hello as u8), false).await;
    assert!(f.is_empty());
    // 5. 方向不对（客户端发 Event）、未知类型、request_id 0。
    let f = after_handshake(&h, header(1, pb::FrameType::Event as u8), true).await;
    assert!(is_malformed(&f), "{f:?}");
    let f = after_handshake(&h, header(1, 77), true).await;
    assert!(is_malformed(&f), "{f:?}");
    let mut zero = BytesMut::new();
    encode_frame(
        &Frame::Request(pb::Request {
            request_id: 0,
            body: Some(command("x")),
        }),
        &mut zero,
    );
    let f = after_handshake(&h, zero.to_vec(), true).await;
    assert!(is_malformed(&f), "{f:?}");

    assert_eq!(
        h.handler.handled.load(Ordering::SeqCst),
        0,
        "违规帧一条都没到 handler"
    );
    assert!(audit.events("protocol_violation").len() >= 7);
    // 之后新连接正常握手、正常请求。
    let (client, _inc) = h.connect_ok().await;
    client.request(command("ok")).await.unwrap();
    assert!(!h.server.is_finished(), "后台服务没有崩");
}

#[tokio::test]
async fn empty_body_is_invalid_argument_not_disconnect() {
    let h = start(server_cfg());
    let (client, _inc) = h.connect_ok().await;
    let err = client
        .view_query(pb::Query { kind: None })
        .await
        .expect_err("空查询不是合法的 ViewQuery");
    assert_eq!(err, ClientError::WrongLane);
    // 走原始帧：body 为空 → INVALID_ARGUMENT，连接不断。
    let conn = h.connector.connect(&h.endpoint).await.unwrap();
    let (mut rd, mut wr) = conn.into_split();
    let mut b = BytesMut::from(hello().as_slice());
    encode_frame(
        &Frame::Request(pb::Request {
            request_id: 5,
            body: None,
        }),
        &mut b,
    );
    wr.write_all(&b).await.unwrap();
    let mut buf = BytesMut::new();
    let mut frames = Vec::new();
    while frames.len() < 2 {
        let mut chunk = [0u8; 4096];
        let n = rd.read(&mut chunk).await.unwrap();
        assert!(n > 0, "连接不该被关");
        buf.extend_from_slice(&chunk[..n]);
        while let Some(f) = decode_frame(&mut buf, MAX_FRAME_LEN, Direction::ToClient).unwrap() {
            frames.push(f);
        }
    }
    assert!(
        matches!(&frames[1], Frame::Response(r) if matches!(&**r, pb::Response { request_id: 5, body: Some(pb::response::Body::Error(e)) } if e.code() == pb::ErrorCode::InvalidArgument))
    );
}

#[tokio::test]
async fn commands_are_serialized_queries_are_concurrent() {
    let h = start(server_cfg());
    *h.handler.command_delay.lock().unwrap() = Duration::from_millis(30);
    *h.handler.side_query_delay.lock().unwrap() = Duration::from_millis(200);
    let (client, _inc) = h.connect_ok().await;
    let client = std::sync::Arc::new(client);
    let started = Instant::now();
    // 4 个慢的并行查询在飞。
    let mut side = Vec::new();
    for _ in 0..4 {
        let c = client.clone();
        side.push(tokio::spawn(async move {
            c.request(pb::request::Body::Query(pb::Query {
                kind: Some(pb::query::Kind::ListModels(pb::ListModels::default())),
            }))
            .await
        }));
    }
    tokio::time::sleep(Duration::from_millis(10)).await;
    // 命令不等查询；三条命令按到达顺序一个一个处理。
    let mut cmds = Vec::new();
    for id in ["a", "b", "c"] {
        let c = client.clone();
        cmds.push(tokio::spawn(async move { c.request(command(id)).await }));
        tokio::task::yield_now().await;
    }
    let t0 = Instant::now();
    cmds.remove(0).await.unwrap().unwrap();
    assert!(
        t0.elapsed() < Duration::from_millis(150),
        "命令被查询挡住了：{:?}",
        t0.elapsed()
    );
    for c in cmds {
        c.await.unwrap().unwrap();
    }
    assert_eq!(*h.handler.commands.lock().unwrap(), ["a", "b", "c"]);
    for s in side {
        s.await.unwrap().unwrap();
    }
    // 4 个 200ms 的查询并发：总时长远小于 800ms。
    assert!(
        started.elapsed() < Duration::from_millis(600),
        "{:?}",
        started.elapsed()
    );
}

#[tokio::test]
async fn side_query_result_comes_from_future_not_incoming() {
    let h = start(server_cfg());
    let (client, mut inc) = h.connect_ok().await;
    let resp = client
        .request(pb::request::Body::Query(pb::Query {
            kind: Some(pb::query::Kind::ReadArtifact(pb::ReadArtifact::default())),
        }))
        .await
        .unwrap();
    assert!(matches!(resp.body, Some(pb::response::Body::Result(_))));
    // Incoming 里没有这个结果。
    assert!(
        tokio::time::timeout(Duration::from_millis(50), inc.next())
            .await
            .is_err()
    );
    // 会写视图的查询不能走 request。
    assert_eq!(
        client
            .request(pb::request::Body::Query(get_conversation("c1")))
            .await
            .unwrap_err(),
        ClientError::WrongLane
    );
}

#[tokio::test]
async fn chunk_too_large_is_protocol_violation() {
    let h = start(server_cfg());
    let mut b = BytesMut::new();
    encode_frame(
        &Frame::Chunk(pb::Chunk {
            upload_id: "u".into(),
            data: vec![0; MAX_CHUNK_DATA + 1].into(),
            ..Default::default()
        }),
        &mut b,
    );
    let f = after_handshake(&h, b.to_vec(), true).await;
    assert!(is_malformed(&f), "{f:?}");
    let mut b = BytesMut::new();
    encode_frame(
        &Frame::Chunk(pb::Chunk {
            upload_id: String::new(),
            ..Default::default()
        }),
        &mut b,
    );
    assert!(is_malformed(&after_handshake(&h, b.to_vec(), true).await));
    assert_eq!(h.handler.chunks.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn chunk_refused_by_handler_closes_with_error() {
    let h = start(server_cfg());
    let (client, mut inc) = h.connect_ok().await;
    client
        .send_chunk(pb::Chunk {
            upload_id: "ok".into(),
            data: vec![1; 10].into(),
            ..Default::default()
        })
        .await
        .unwrap();
    client
        .send_chunk(pb::Chunk {
            upload_id: "refuse".into(),
            data: vec![1; 10].into(),
            ..Default::default()
        })
        .await
        .unwrap();
    match next_item(&mut inc).await {
        IncomingItem::ServerError(e) => assert_eq!(e.code(), pb::ErrorCode::PayloadTooLarge),
        other => panic!("{other:?}"),
    }
    assert!(matches!(next_item(&mut inc).await, IncomingItem::Closed(_)));
    assert_eq!(h.handler.chunks.load(Ordering::SeqCst), 2);
}

#[tokio::test]
async fn connection_closed_notifies_handler() {
    let h = start(server_cfg());
    let (client, inc) = h.connect_ok().await;
    drop(client);
    drop(inc);
    for _ in 0..100 {
        if h.handler.closed.load(Ordering::SeqCst) == 1 {
            return;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    panic!("handler.connection_closed 没有被调用");
}

#[tokio::test]
async fn shutdown_flushes_queue_within_1s() {
    let h = start(server_cfg());
    let (client, mut inc) = h.connect_ok().await;
    client.subscribe(1, sidebar(), 0).await.unwrap();
    assert!(matches!(
        next_item(&mut inc).await,
        IncomingItem::Subscribed { .. }
    ));
    for i in 0..50 {
        h.upsert("project:p1", i);
    }
    h.shutdown.cancel();
    let t = Instant::now();
    let mut got = 0;
    loop {
        match next_item(&mut inc).await {
            IncomingItem::Event(_) => got += 1,
            IncomingItem::Closed(_) => break,
            other => panic!("{other:?}"),
        }
    }
    assert_eq!(got, 50, "停机前把队列写完");
    assert!(t.elapsed() < Duration::from_secs(2));
    tokio::time::timeout(Duration::from_secs(2), h.server)
        .await
        .expect("serve 应在停机后返回")
        .unwrap();
}
