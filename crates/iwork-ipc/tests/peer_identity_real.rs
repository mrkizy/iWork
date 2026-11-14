//! 识别对端超时的真实路径（审查 2026-11-14-fix-app-dialer-flake 1/F-1）：真实 Unix socket、真实 `LocalConn`，只在
//! 阻塞识别的闭包里用 `std::thread::sleep` 卡住（`test-hooks`，取消不掉，和慢机器上验签卡住一样）。
//!
//! - 超时就真的关掉通信：卡住的验签线程手里还有一份复制的 fd，对端也得马上读到 EOF；
//! - 在途的阻塞识别有上限：连续超时不会一直叠线程；拿不到名额的在截止时间按超时处理；任务真结束才归还名额。
//!
//! 钩子是进程级的：本文件的用例串行跑（[`serial`]）。
#![cfg(unix)]

mod support;

use std::time::{Duration, Instant};

use iwork_ipc::client::{Client, ClientConfig, ConnectError};
use iwork_ipc::identify_hooks as hooks;
use iwork_ipc::server::Server;
use iwork_ipc::transport::{Connection, Endpoint, Listener};
use iwork_ipc::{LocalConnector, LocalListener};
use support::*;
use tokio::io::AsyncReadExt;
use tokio::net::UnixStream;
use tokio::sync::Mutex;
use tokio_util::sync::CancellationToken;

/// 握手的截止时间。
const LIMIT: Duration = Duration::from_millis(300);
/// 阻塞识别卡多久：远长于截止时间。
const STUCK: Duration = Duration::from_secs(3);
/// 「很快」：截止时间之后这么久之内对端要读到 EOF。卡住的验签线程要到 STUCK 才放手，修之前过不了。
const PROMPT: Duration = Duration::from_millis(1000);

/// 串行跑本文件的用例：守卫要跨过 await，用异步锁。
async fn serial() -> tokio::sync::MutexGuard<'static, ()> {
    static LOCK: Mutex<()> = Mutex::const_new(());
    LOCK.lock().await
}

/// 用例结束时把钩子清掉、等卡住的识别都走完，不留给下一个用例。
struct Unstall;

impl Drop for Unstall {
    fn drop(&mut self) {
        hooks::set_stall(Duration::ZERO);
        let t = Instant::now();
        while hooks::running() > 0 && t.elapsed() < STUCK * 3 {
            std::thread::sleep(Duration::from_millis(20));
        }
    }
}

fn socket_dir() -> (tempfile::TempDir, std::path::PathBuf) {
    let dir = tempfile::Builder::new()
        .prefix("iwp")
        .tempdir_in("/tmp")
        .unwrap();
    let path = dir.path().join("s.sock");
    (dir, path)
}

fn client_cfg_with(limit: Duration) -> ClientConfig {
    let mut c = client_cfg();
    c.handshake_timeout = limit;
    c
}

/// 读到 EOF 用了多久；`within` 内没读到就 `None`。顺带断言一个字节都没收到。
async fn eof_within<R: tokio::io::AsyncRead + Unpin>(
    mut rd: R,
    within: Duration,
) -> Option<Duration> {
    let t = Instant::now();
    let mut buf = Vec::new();
    match tokio::time::timeout(within, rd.read_to_end(&mut buf)).await {
        Ok(_) => {
            assert!(buf.is_empty(), "超时前不该发出任何字节：{buf:?}");
            Some(t.elapsed())
        }
        Err(_) => None,
    }
}

/// 客户端识别服务端卡住：报 `Timeout` 之后，对端（这里是一个不说话的监听端）很快读到 EOF。
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn client_timeout_really_closes_the_socket() {
    let _s = serial().await;
    let _u = Unstall;
    let (_dir, path) = socket_dir();
    let mut listener = LocalListener::bind(&path).unwrap();
    let endpoint = Endpoint::Unix(path);
    let accepted = tokio::spawn(async move { listener.accept().await.unwrap() });
    hooks::set_stall(STUCK);
    let t = Instant::now();
    let err = Client::connect(
        &LocalConnector,
        &endpoint,
        &client_own(),
        client_cfg_with(LIMIT),
    )
    .await
    .err()
    .expect("识别卡住时不能连上");
    assert!(matches!(err, ConnectError::Timeout), "{err:?}");
    assert!(t.elapsed() < STUCK / 2, "按截止时间返回：{:?}", t.elapsed());
    let (rd, _wr) = accepted.await.unwrap().into_split();
    let took = eof_within(rd, PROMPT).await;
    assert!(
        took.is_some(),
        "客户端超时后 {PROMPT:?} 内对端没读到 EOF：通信没关掉"
    );
}

/// 服务端识别客户端卡住：截止时间一到就关，客户端很快读到 EOF、一个字节都收不到。
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn server_timeout_really_closes_the_socket() {
    let _s = serial().await;
    let _u = Unstall;
    let (_dir, path) = socket_dir();
    let listener = LocalListener::bind(&path).unwrap();
    let mut cfg = server_cfg();
    cfg.handshake_timeout = LIMIT;
    let world = World::default();
    let (server, _publisher) = Server::new(
        cfg,
        server_own(),
        TestHandler::new(world.clone()),
        WorldSource::new(world),
    );
    let shutdown = CancellationToken::new();
    let _guard = shutdown.clone().drop_guard();
    tokio::spawn(server.serve(listener, shutdown));
    hooks::set_stall(STUCK);
    let stream = UnixStream::connect(&path).await.unwrap();
    let (rd, _wr) = stream.into_split();
    let took = eof_within(rd, LIMIT + PROMPT).await;
    assert!(
        took.is_some(),
        "服务端识别超时后 {:?} 内客户端没读到 EOF：通信没关掉",
        LIMIT + PROMPT
    );
}

/// 连续超时：同时在途的阻塞识别不超过上限；名额满了的那几次也在截止时间按超时返回；卡住的识别都结束后名额全数归还。
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn stuck_identifications_are_bounded() {
    let _s = serial().await;
    let _u = Unstall;
    let (_dir, path) = socket_dir();
    let mut listener = LocalListener::bind(&path).unwrap();
    let endpoint = Endpoint::Unix(path);
    // 不说话的监听端：接下所有连接、留着
    let held = tokio::spawn(async move {
        let mut conns = Vec::new();
        while let Ok(c) = listener.accept().await {
            conns.push(c);
        }
    });
    hooks::set_stall(STUCK);
    hooks::reset_peak();
    let attempts = 8;
    for i in 0..attempts {
        let t = Instant::now();
        let err = Client::connect(
            &LocalConnector,
            &endpoint,
            &client_own(),
            client_cfg_with(LIMIT),
        )
        .await
        .err()
        .expect("识别卡住时不能连上");
        assert!(matches!(err, ConnectError::Timeout), "第 {i} 次：{err:?}");
        assert!(
            t.elapsed() < STUCK / 2,
            "第 {i} 次按截止时间返回：{:?}",
            t.elapsed()
        );
    }
    let peak = hooks::peak();
    assert!(
        peak <= hooks::SLOTS,
        "同时在途的阻塞识别 {peak} 个，超过上限 {}",
        hooks::SLOTS
    );
    assert!(peak >= 1, "对照：真的有识别卡住了");
    // 卡住的识别结束之后名额全数归还
    hooks::set_stall(Duration::ZERO);
    let t = Instant::now();
    while (hooks::running() > 0 || hooks::available() < hooks::SLOTS) && t.elapsed() < STUCK * 2 {
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert_eq!(hooks::running(), 0);
    assert_eq!(hooks::available(), hooks::SLOTS, "名额全数归还");
    held.abort();
}
