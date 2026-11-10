//! 握手与双向校验（验收 3a；方案 2.4、2.4c、2.6、7.2「双向校验」「升级窗口」）。

mod support;

use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::Duration;

use bytes::BytesMut;
use iwork_ipc::client::{Backoff, Client, ConnectError, WaitPrompt};
use iwork_ipc::frame::{Direction, Frame, decode_frame, encode_frame};
use iwork_ipc::peer::RejectReason;
use iwork_ipc::transport::memory;
use iwork_ipc::transport::{Connection, Connector, Listener};
use iwork_proto::limits::MAX_FRAME_LEN;
use iwork_proto::pb;
use support::*;
use tokio::io::{AsyncReadExt, AsyncWriteExt};

fn hello(major: u32, minor: u32) -> Frame {
    Frame::Hello(pb::Hello {
        protocol_major: major,
        protocol_minor: minor,
        client_kind: pb::ClientKind::Desktop.into(),
        ..Default::default()
    })
}

fn bytes_of(frames: &[Frame]) -> Vec<u8> {
    let mut b = BytesMut::new();
    for f in frames {
        encode_frame(f, &mut b);
    }
    b.to_vec()
}

/// 读到对端关闭为止，解出全部帧。
async fn read_all_frames<R: tokio::io::AsyncRead + Unpin>(mut rd: R) -> Vec<Frame> {
    let mut raw = Vec::new();
    tokio::time::timeout(Duration::from_secs(10), rd.read_to_end(&mut raw))
        .await
        .expect("服务端应关闭连接")
        .ok();
    let mut buf = BytesMut::from(raw.as_slice());
    let mut out = Vec::new();
    while let Some(f) = decode_frame(&mut buf, MAX_FRAME_LEN, Direction::ToClient).unwrap() {
        out.push(f);
    }
    out
}

fn mismatch_details(frames: &[Frame]) -> (String, String) {
    match frames {
        [
            Frame::Response(pb::Response {
                request_id: 0,
                body: Some(pb::response::Body::Error(e)),
            }),
        ] => {
            assert_eq!(e.code(), pb::ErrorCode::ProtocolVersionMismatch);
            (
                e.details["client_major"].clone(),
                e.details["server_major"].clone(),
            )
        }
        other => panic!("期望恰好一帧版本不符错误，得到 {other:?}"),
    }
}

async fn major_rejected(server_major: u32, client_major: u32) {
    let mut cfg = server_cfg();
    cfg.protocol_major = server_major;
    let h = start(cfg);
    let conn = h.connector.connect(&h.endpoint).await.unwrap();
    let (rd, mut wr) = conn.into_split();
    // 同一次写入里紧跟 Hello 的业务请求不能到 handler。
    let req = Frame::Request(pb::Request {
        request_id: 1,
        body: Some(command("c1")),
    });
    wr.write_all(&bytes_of(&[hello(client_major, 0), req]))
        .await
        .unwrap();
    let frames = read_all_frames(rd).await;
    let (c, s) = mismatch_details(&frames);
    assert_eq!((c, s), (client_major.to_string(), server_major.to_string()));
    assert_eq!(
        h.handler.handled.load(Ordering::SeqCst),
        0,
        "拒绝前不处理任何业务帧"
    );
}

#[tokio::test]
async fn old_client_vs_new_server_major_rejected() {
    major_rejected(2, 1).await;
}

#[tokio::test]
async fn new_client_vs_old_server_major_rejected() {
    major_rejected(1, 2).await;
}

#[tokio::test]
async fn same_major_other_minor_gets_welcome() {
    let mut cfg = server_cfg();
    cfg.protocol_minor = 3;
    let h = start(cfg);
    let (client, _incoming) = h.connect_ok().await;
    assert_eq!(client.welcome().protocol_major, iwork_proto::PROTOCOL_MAJOR);
    assert_eq!(client.welcome().protocol_minor, 3);
    // 对照：客户端次版本不同也照常连通。
    let mut ccfg = client_cfg();
    ccfg.protocol_minor = 9;
    Client::connect(&h.connector, &h.endpoint, &client_own(), ccfg)
        .await
        .expect("次版本不同照常连通");
}

#[tokio::test]
async fn client_sees_version_mismatch_both_directions() {
    for (server_major, client_major) in [(2, 1), (1, 2)] {
        let mut cfg = server_cfg();
        cfg.protocol_major = server_major;
        let h = start(cfg);
        let mut ccfg = client_cfg();
        ccfg.protocol_major = client_major;
        let err = Client::connect(&h.connector, &h.endpoint, &client_own(), ccfg)
            .await
            .err()
            .unwrap();
        assert!(
            matches!(err, ConnectError::VersionMismatch { client_major: c, server_major: s } if c == client_major && s == server_major),
            "{err:?}"
        );
        assert_eq!(err.wait_prompt(), Some(WaitPrompt::SwitchingVersion));
    }
}

/// 假后台：不做握手门，建连就发合法 Welcome（主版本不同），客户端也要拦。
#[tokio::test]
async fn client_rechecks_welcome_major() {
    let (mut listener, connector) =
        memory::endpoint("fake", client_peer(), Arc::new(|_| server_peer()));
    let ep = listener.endpoint().clone();
    tokio::spawn(async move {
        let conn = listener.accept().await.unwrap();
        let (_rd, mut wr) = conn.into_split();
        let w = Frame::Welcome(pb::Welcome {
            protocol_major: iwork_proto::PROTOCOL_MAJOR + 1,
            ..Default::default()
        });
        wr.write_all(&bytes_of(&[w])).await.unwrap();
        tokio::time::sleep(Duration::from_secs(5)).await;
    });
    let err = Client::connect(&connector, &ep, &client_own(), client_cfg())
        .await
        .err()
        .unwrap();
    assert!(
        matches!(err, ConnectError::VersionMismatch { .. }),
        "{err:?}"
    );
}

/// 假后台：同用户、映像不在清单里，建连即发合法 Welcome。客户端返回 PeerRejected，假后台一个字节都收不到。
#[tokio::test]
async fn client_rejects_fake_server_with_valid_welcome() {
    let fake = peer(666, UID, 0xEE);
    let (mut listener, connector) =
        memory::endpoint("fake", client_peer(), Arc::new(move |_| fake.clone()));
    let ep = listener.endpoint().clone();
    let received = tokio::spawn(async move {
        let conn = listener.accept().await.unwrap();
        let (mut rd, mut wr) = conn.into_split();
        let w = Frame::Welcome(pb::Welcome {
            protocol_major: iwork_proto::PROTOCOL_MAJOR,
            ..Default::default()
        });
        let _ = wr.write_all(&bytes_of(&[w])).await;
        let mut got = Vec::new();
        let _ = rd.read_to_end(&mut got).await;
        got.len()
    });
    let err = Client::connect(&connector, &ep, &client_own(), client_cfg())
        .await
        .err()
        .unwrap();
    assert!(
        matches!(
            err,
            ConnectError::PeerRejected(RejectReason::ImageNotTrusted { .. })
        ),
        "{err:?}"
    );
    assert_eq!(
        received.await.unwrap(),
        0,
        "假后台不该收到任何字节（包括 Hello）"
    );
}

#[tokio::test]
async fn client_rejects_server_of_other_user() {
    let h = start_with(
        server_cfg(),
        World::default(),
        server_own(),
        client_peer(),
        Arc::new(|_| peer(1000, 0, SERVER_IMAGE)),
    );
    let err = h.connect().await.err().unwrap();
    assert!(
        matches!(
            err,
            ConnectError::PeerRejected(RejectReason::DifferentUser { .. })
        ),
        "{err:?}"
    );
}

#[tokio::test]
async fn server_rejects_other_user_and_unlisted_image_without_sending_a_byte() {
    let (audit, _g) = Captured::install();
    for (client, kind) in [
        (peer(2000, 0, CLIENT_IMAGE), "DifferentUser"),
        (peer(2000, UID, 0xEE), "ImageNotTrusted"),
    ] {
        let h = start_with(
            server_cfg(),
            World::default(),
            server_own(),
            client,
            Arc::new(|_| server_peer()),
        );
        let conn = h.connector.connect(&h.endpoint).await.unwrap();
        let (rd, mut wr) = conn.into_split();
        let _ = wr
            .write_all(&bytes_of(&[hello(iwork_proto::PROTOCOL_MAJOR, 0)]))
            .await;
        assert!(
            read_all_frames(rd).await.is_empty(),
            "{kind}：被拒的对端收不到任何帧"
        );
        assert_eq!(h.handler.handled.load(Ordering::SeqCst), 0);
        // 审计里有这条拒绝。
        tokio::time::sleep(Duration::from_millis(20)).await;
        let rejected = audit.events("peer_rejected");
        assert!(
            rejected.iter().any(|e| e["role"] == "server"
                && e["reason"].starts_with(kind)
                && e["pid"] == "2000"),
            "{kind}：{rejected:?}"
        );
        // 对照：后台仍能接受合法客户端（同一个服务端换客户端身份）。
        let ok = h.connector.clone().with_client_identity(client_peer());
        Client::connect(&ok, &h.endpoint, &client_own(), client_cfg())
            .await
            .expect("合法客户端照常连上");
    }
}

/// 两端判定之前不写任何字节：服务端 peer() 延迟期间，客户端没有写出 Hello。
#[tokio::test]
async fn client_does_not_send_hello_before_judging() {
    let (mut listener, connector) =
        memory::endpoint("slow", client_peer(), Arc::new(|_| peer(1, UID, 0xEE)));
    let connector = connector.with_peer_delay(Duration::from_millis(200));
    let ep = listener.endpoint().clone();
    let server = tokio::spawn(async move {
        let conn = listener.accept().await.unwrap();
        let (mut rd, _wr) = conn.into_split();
        let mut b = [0u8; 1];
        tokio::time::timeout(Duration::from_millis(500), rd.read(&mut b)).await
    });
    let err = Client::connect(&connector, &ep, &client_own(), client_cfg())
        .await
        .err()
        .unwrap();
    assert!(matches!(err, ConnectError::PeerRejected(_)));
    // 读到 0（对端关闭）或超时，都说明没写；读到 1 字节就是写了 Hello。
    if let Ok(Ok(n)) = server.await.unwrap() {
        assert_eq!(n, 0, "判定不过却写出了字节");
    }
}

#[tokio::test]
async fn server_and_client_use_same_judge() {
    // 同一组身份：服务端看客户端、客户端看服务端，结论一致。
    for (img, ok) in [(CLIENT_IMAGE, true), (0xEE, false)] {
        let p = peer(5, UID, img);
        let a = iwork_ipc::peer::judge_peer(
            &server_own(),
            &p,
            iwork_ipc::peer::TrustPolicy::RequireSigner,
        );
        let b = iwork_ipc::peer::judge_peer(
            &client_own(),
            &p,
            iwork_ipc::peer::TrustPolicy::RequireSigner,
        );
        assert_eq!(a, b);
        assert_eq!(
            matches!(a, iwork_ipc::peer::PeerVerdict::Accepted { .. }),
            ok
        );
    }
}

#[tokio::test(start_paused = true)]
async fn handshake_timeout_closes() {
    let (audit, _g) = Captured::install();
    let h = start(server_cfg());
    let conn = h.connector.connect(&h.endpoint).await.unwrap();
    let (mut rd, _wr) = conn.into_split();
    let mut b = Vec::new();
    // 5s 不发 Hello：服务端关闭（时间快进）。
    let n = tokio::time::timeout(Duration::from_secs(6), rd.read_to_end(&mut b))
        .await
        .expect("5s 后应关闭")
        .unwrap();
    assert_eq!(n, 0);
    assert!(
        audit
            .events("protocol_violation")
            .iter()
            .any(|e| e["reason"] == "握手超时")
    );
}

#[tokio::test]
async fn client_reports_closed_before_welcome() {
    // 服务端认不出客户端（清单里没有它）：服务端先关，客户端看到 ClosedBeforeWelcome。
    let h = start_with(
        server_cfg(),
        World::default(),
        own(SERVER_IMAGE, &[SERVER_IMAGE]),
        client_peer(),
        Arc::new(|_| server_peer()),
    );
    let err = h.connect().await.err().unwrap();
    assert!(matches!(err, ConnectError::ClosedBeforeWelcome), "{err:?}");
    assert_eq!(err.wait_prompt(), Some(WaitPrompt::VersionMismatch));
}

/// 升级窗口：前 3 次对端是旧后台（映像不在新客户端的清单里）→ PeerRejected、不发 Hello；
/// 之后对端是新后台 → 握手成功。每次重连都判定，退避递增。
#[tokio::test(start_paused = true)]
async fn new_client_vs_old_server_recovers_when_server_restarts() {
    let old_server = peer(1000, UID, 0x01);
    let h = start_with(
        server_cfg(),
        World::default(),
        server_own(),
        client_peer(),
        Arc::new(move |attempt| {
            if attempt < 3 {
                old_server.clone()
            } else {
                server_peer()
            }
        }),
    );
    let mut backoff = Backoff::new();
    let mut delays = Vec::new();
    let mut prompts = Vec::new();
    let client = loop {
        match h.connect().await {
            Ok((c, _)) => break c,
            Err(e) => {
                prompts.push(e.wait_prompt());
                let d = backoff.next_delay();
                delays.push(d);
                tokio::time::sleep(d).await;
            }
        }
    };
    assert_eq!(h.connector.attempts(), 4, "每次重连都重新建连并判定");
    assert_eq!(prompts, vec![Some(WaitPrompt::VersionMismatch); 3]);
    assert!(
        delays.windows(2).all(|w| w[0] < w[1]),
        "退避递增：{delays:?}"
    );
    assert!(!client.welcome().session_id.is_empty());
}

/// 旧客户端 × 新后台：无论先收到 PeerRejected 还是 ClosedBeforeWelcome，提示同一句、同两个出口，
/// 一直被拒、从不发 Hello（PeerRejected 那一侧）。
#[tokio::test]
async fn old_client_vs_new_server_shows_neutral_prompt_with_exits() {
    // 旧客户端的清单只认 v1 后台；新后台的清单只认 v2 客户端。
    let old_client = own(0x01, &[0x01, 0x02]);
    let new_server_own = own(0x22, &[0x21, 0x22]);
    let h = start_with(
        server_cfg(),
        World::default(),
        new_server_own,
        peer(2000, UID, 0x01),
        Arc::new(|_| peer(1000, UID, 0x22)),
    );
    let mut seen = Vec::new();
    for _ in 0..3 {
        let err = Client::connect(&h.connector, &h.endpoint, &old_client, client_cfg())
            .await
            .err()
            .unwrap();
        assert!(
            matches!(
                err,
                ConnectError::PeerRejected(_) | ConnectError::ClosedBeforeWelcome
            ),
            "{err:?}"
        );
        seen.push(err.wait_prompt().unwrap());
    }
    // 服务端这侧先关的情形：客户端信任新后台，但新后台不认旧客户端。
    let trusting_old_client = own(0x01, &[0x01, 0x22]);
    let err = Client::connect(
        &h.connector,
        &h.endpoint,
        &trusting_old_client,
        client_cfg(),
    )
    .await
    .err()
    .unwrap();
    assert!(matches!(err, ConnectError::ClosedBeforeWelcome), "{err:?}");
    seen.push(err.wait_prompt().unwrap());
    assert!(seen.iter().all(|p| *p == WaitPrompt::VersionMismatch));
    assert_eq!(
        WaitPrompt::VersionMismatch.exits(),
        ["重新打开 iWork", "查看诊断"]
    );
    assert_eq!(h.handler.handled.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn client_keeps_rejecting_on_every_retry() {
    let h = start_with(
        server_cfg(),
        World::default(),
        server_own(),
        client_peer(),
        Arc::new(|_| peer(1, UID, 0xEE)),
    );
    for _ in 0..5 {
        assert!(matches!(
            h.connect().await,
            Err(ConnectError::PeerRejected(_))
        ));
    }
    assert_eq!(h.connector.attempts(), 5);
}

#[tokio::test]
async fn connect_to_missing_endpoint_is_io_error() {
    let h = start(server_cfg());
    let err = Client::connect(
        &h.connector,
        &iwork_ipc::transport::Endpoint::Memory("nope".into()),
        &client_own(),
        client_cfg(),
    )
    .await
    .err()
    .unwrap();
    assert!(matches!(err, ConnectError::Io(_)));
    assert_eq!(err.wait_prompt(), None);
}
