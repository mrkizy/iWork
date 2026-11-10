//! 先正确握手，再喂任意字节：服务端不崩，body 为空的 Request 从不到 handler。
#![no_main]

mod common;

use std::sync::atomic::Ordering;
use std::time::Duration;

use bytes::BytesMut;
use iwork_ipc::frame::{Frame, encode_frame};
use iwork_ipc::transport::{Connection, Connector};
use iwork_proto::pb;
use libfuzzer_sys::fuzz_target;
use tokio::io::{AsyncReadExt, AsyncWriteExt};

fuzz_target!(|data: &[u8]| {
    common::runtime().block_on(async {
        let handler = common::Counting::default();
        let (connector, ep, server, token) = common::start(handler.clone());
        let conn = connector.connect(&ep).await.unwrap();
        let (mut rd, mut wr) = conn.into_split();
        let mut hello = BytesMut::new();
        encode_frame(&Frame::Hello(pb::Hello { protocol_major: iwork_proto::PROTOCOL_MAJOR, ..Default::default() }), &mut hello);
        wr.write_all(&hello).await.unwrap();
        let _ = wr.write_all(data).await;
        let _ = wr.shutdown().await;
        let mut sink = Vec::new();
        let _ = tokio::time::timeout(Duration::from_secs(1), rd.read_to_end(&mut sink)).await;
        token.cancel();
        server.await.expect("服务端任务不能 panic");
        assert_eq!(handler.empty_body_seen.load(Ordering::SeqCst), 0, "body 为空的请求到了 handler");
    });
});
