//! 握手前喂任意字节：服务端不崩、handler 一次都不被调用（拒绝前不处理任何业务帧）。
#![no_main]

mod common;

use std::sync::atomic::Ordering;
use std::time::Duration;

use iwork_ipc::frame::{Direction, Frame, decode_frame};
use iwork_ipc::transport::{Connection, Connector};
use iwork_proto::limits::MAX_HELLO_LEN;
use libfuzzer_sys::fuzz_target;
use tokio::io::{AsyncReadExt, AsyncWriteExt};

fuzz_target!(|data: &[u8]| {
    common::runtime().block_on(async {
        let handler = common::Counting::default();
        let (connector, ep, server, token) = common::start(handler.clone());
        let conn = connector.connect(&ep).await.unwrap();
        let (mut rd, mut wr) = conn.into_split();
        let _ = wr.write_all(data).await;
        let _ = wr.shutdown().await;
        let mut sink = Vec::new();
        let _ = tokio::time::timeout(Duration::from_secs(1), rd.read_to_end(&mut sink)).await;
        token.cancel();
        server.await.expect("服务端任务不能 panic");
        // 只有输入恰好以一个合法、主版本相同的 Hello 开头时，握手才会成功、之后的帧才可能到 handler。
        let mut buf = bytes::BytesMut::from(data);
        let valid_hello = matches!(
            decode_frame(&mut buf, MAX_HELLO_LEN, Direction::ToServer),
            Ok(Some(Frame::Hello(h))) if h.protocol_major == iwork_proto::PROTOCOL_MAJOR
        );
        if !valid_hello {
            assert_eq!(handler.handled.load(Ordering::SeqCst), 0, "握手前处理了业务帧");
        }
    });
});
