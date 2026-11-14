//! 识别对端有上限（app_dialer 偶发的根因）：识别对端（macOS 上是全量校验对端映像）和读第一帧共用握手的截止时间。
//! 超时一律算失败、不放行：客户端报 `Timeout`、不发 `Hello`；服务端关连接、不发任何帧、记一条拒绝审计。
//! 慢的那一侧在测试里包一层，只让 `peer()` 变慢；慢多久中途可改，用来证明恢复。

mod support;

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use iwork_ipc::client::{Client, ConnectError};
use iwork_ipc::peer::{PeerError, PeerIdentity};
use iwork_ipc::server::Server;
use iwork_ipc::transport::memory::{self, MemoryConn, MemoryConnector, MemoryListener};
use iwork_ipc::transport::{Connection, Connector, Endpoint, Listener};
use support::*;
use tokio_util::sync::CancellationToken;

/// 握手的截止时间：比慢的那一侧短得多。
const LIMIT: Duration = Duration::from_millis(300);
/// 「卡住」的识别要等多久。
const STUCK: Duration = Duration::from_secs(10);

/// 识别对端前多等多少毫秒（共享，可中途改）。
#[derive(Clone, Default)]
struct Delay(Arc<AtomicU64>);

impl Delay {
    fn set(&self, d: Duration) {
        self.0.store(d.as_millis() as u64, Ordering::SeqCst);
    }
    fn get(&self) -> Duration {
        Duration::from_millis(self.0.load(Ordering::SeqCst))
    }
}

struct SlowConn {
    inner: MemoryConn,
    delay: Duration,
}

impl Connection for SlowConn {
    type Read = <MemoryConn as Connection>::Read;
    type Write = <MemoryConn as Connection>::Write;

    async fn peer(&self) -> Result<PeerIdentity, PeerError> {
        tokio::time::sleep(self.delay).await;
        self.inner.peer().await
    }

    fn into_split(self) -> (Self::Read, Self::Write) {
        self.inner.into_split()
    }
}

/// 客户端一侧：看服务端时变慢。
struct SlowConnector {
    inner: MemoryConnector,
    delay: Delay,
}

impl Connector for SlowConnector {
    type Conn = SlowConn;

    async fn connect(&self, endpoint: &Endpoint) -> std::io::Result<SlowConn> {
        Ok(SlowConn {
            inner: self.inner.connect(endpoint).await?,
            delay: self.delay.get(),
        })
    }
}

/// 服务端一侧：看客户端时变慢。
struct SlowListener {
    inner: MemoryListener,
    delay: Delay,
}

impl Listener for SlowListener {
    type Conn = SlowConn;

    async fn accept(&mut self) -> std::io::Result<SlowConn> {
        Ok(SlowConn {
            inner: self.inner.accept().await?,
            delay: self.delay.get(),
        })
    }

    fn endpoint(&self) -> &Endpoint {
        self.inner.endpoint()
    }
}

struct Rig {
    handler: TestHandler,
    connector: SlowConnector,
    server_delay: Delay,
    endpoint: Endpoint,
    _shutdown: tokio_util::sync::DropGuard,
}

/// 起一个服务端，两侧各有一个可调的识别延迟（起始都是 0）。
fn rig() -> Rig {
    let mut cfg = server_cfg();
    cfg.handshake_timeout = LIMIT;
    let world = World::default();
    let handler = TestHandler::new(world.clone());
    let (server, _publisher) =
        Server::new(cfg, server_own(), handler.clone(), WorldSource::new(world));
    let (listener, connector) =
        memory::endpoint("slow-peer", client_peer(), Arc::new(|_| server_peer()));
    let server_delay = Delay::default();
    let listener = SlowListener {
        inner: listener,
        delay: server_delay.clone(),
    };
    let shutdown = CancellationToken::new();
    tokio::spawn(server.serve(listener, shutdown.clone()));
    Rig {
        handler,
        connector: SlowConnector {
            inner: connector,
            delay: Delay::default(),
        },
        server_delay,
        endpoint: Endpoint::Memory("slow-peer".into()),
        _shutdown: shutdown.drop_guard(),
    }
}

fn cfg() -> iwork_ipc::client::ClientConfig {
    let mut c = client_cfg();
    c.handshake_timeout = LIMIT;
    c
}

/// 客户端识别服务端卡住：在截止时间内报 `Timeout`，不算信任、不发 Hello（服务端没受理任何东西）；识别恢复正常后
/// 下一次拨号连得上（客户端会话按 `Timeout` 进诊断页、退避重拨，见 client-core 的 `on_dial_error`）。
#[tokio::test]
async fn client_times_out_on_stuck_peer_identification_then_recovers() {
    let (audit, _g) = Captured::install();
    let r = rig();
    r.connector.delay.set(STUCK);
    let t = Instant::now();
    let err = Client::connect(&r.connector, &r.endpoint, &client_own(), cfg())
        .await
        .err()
        .expect("识别卡住时不能连上");
    assert!(matches!(err, ConnectError::Timeout), "{err:?}");
    assert!(
        t.elapsed() < STUCK / 2,
        "在截止时间附近返回，不等识别做完：{:?}",
        t.elapsed()
    );
    assert_eq!(
        r.handler.handled.load(Ordering::SeqCst),
        0,
        "没有发出任何请求"
    );
    assert!(
        audit
            .events("peer_rejected")
            .iter()
            .any(|e| e["role"] == "client" && e["reason"].contains("超时")),
        "{:?}",
        audit.all()
    );

    r.connector.delay.set(Duration::ZERO);
    Client::connect(&r.connector, &r.endpoint, &client_own(), cfg())
        .await
        .expect("识别恢复后重拨连得上");
}

/// 服务端识别客户端卡住：截止时间一到就关连接，不发任何帧（客户端看到握手前被关），记一条拒绝审计；之后正常的
/// 客户端照常连上。
#[tokio::test]
async fn server_closes_on_stuck_peer_identification() {
    let (audit, _g) = Captured::install();
    let r = rig();
    r.server_delay.set(STUCK);
    let mut slow_client = cfg();
    // 客户端自己等得比服务端久：看到的必须是服务端先关，不是自己超时
    slow_client.handshake_timeout = STUCK;
    let t = Instant::now();
    let err = Client::connect(&r.connector, &r.endpoint, &client_own(), slow_client)
        .await
        .err()
        .expect("服务端识别卡住时不能连上");
    assert!(matches!(err, ConnectError::ClosedBeforeWelcome), "{err:?}");
    assert!(
        t.elapsed() < STUCK / 2,
        "服务端在截止时间附近关连接：{:?}",
        t.elapsed()
    );
    tokio::time::sleep(Duration::from_millis(20)).await;
    assert!(
        audit
            .events("peer_rejected")
            .iter()
            .any(|e| e["role"] == "server" && e["reason"].contains("超时")),
        "{:?}",
        audit.all()
    );
    assert_eq!(r.handler.handled.load(Ordering::SeqCst), 0);

    r.server_delay.set(Duration::ZERO);
    Client::connect(&r.connector, &r.endpoint, &client_own(), cfg())
        .await
        .expect("服务端识别恢复后连得上");
}
