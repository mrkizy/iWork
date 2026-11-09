//! 进程内传输：`tokio::io::duplex` 两端，对端身份由调用方指定（两端各自指定）。
//!
//! 只在同一个进程里可达，没有任何操作系统端点，用于测试与调试工具：订阅、背压、握手、双向校验的测试
//! 都跑在它上面，不碰文件系统、不依赖平台。

use std::io;
use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::Duration;

use tokio::io::{DuplexStream, ReadHalf, WriteHalf};
use tokio::sync::mpsc;

use super::{Connection, Connector, Endpoint, Listener};
use crate::peer::{PeerError, PeerIdentity};

/// 每条连接每个方向的管道缓冲。小一点，慢消费者测试里「对端不读」能很快表现为写不动。
pub const DEFAULT_PIPE_BYTES: usize = 64 * 1024;

/// 客户端每次建连时，客户端一侧看到的服务端身份。参数是第几次建连（从 0 起），测试据此模拟「前 N 次是旧后台」。
pub type ServerIdentityFn = dyn Fn(u32) -> PeerIdentity + Send + Sync;

/// 一个进程内端点：建好后拿到监听端与建连端。
pub fn endpoint(
    name: &str,
    client_as_seen_by_server: PeerIdentity,
    server_as_seen_by_client: Arc<ServerIdentityFn>,
) -> (MemoryListener, MemoryConnector) {
    let (tx, rx) = mpsc::unbounded_channel();
    let endpoint = Endpoint::Memory(name.to_owned());
    (
        MemoryListener {
            rx,
            endpoint: endpoint.clone(),
        },
        MemoryConnector {
            tx,
            endpoint,
            client_identity: client_as_seen_by_server,
            server_identity: server_as_seen_by_client,
            attempts: Arc::new(AtomicU32::new(0)),
            pipe_bytes: DEFAULT_PIPE_BYTES,
            peer_delay: Duration::ZERO,
        },
    )
}

pub struct MemoryListener {
    rx: mpsc::UnboundedReceiver<MemoryConn>,
    endpoint: Endpoint,
}

impl Listener for MemoryListener {
    type Conn = MemoryConn;

    async fn accept(&mut self) -> io::Result<MemoryConn> {
        self.rx
            .recv()
            .await
            .ok_or_else(|| io::Error::new(io::ErrorKind::BrokenPipe, "内存端点的建连端已全部关闭"))
    }

    fn endpoint(&self) -> &Endpoint {
        &self.endpoint
    }
}

#[derive(Clone)]
pub struct MemoryConnector {
    tx: mpsc::UnboundedSender<MemoryConn>,
    endpoint: Endpoint,
    client_identity: PeerIdentity,
    server_identity: Arc<ServerIdentityFn>,
    attempts: Arc<AtomicU32>,
    pipe_bytes: usize,
    peer_delay: Duration,
}

impl MemoryConnector {
    /// 每个方向的管道缓冲字节数。
    pub fn with_pipe_bytes(mut self, bytes: usize) -> Self {
        self.pipe_bytes = bytes;
        self
    }

    /// 让两端的 `peer()` 都晚这么久才返回（测试「判定之前不写任何字节」）。
    pub fn with_peer_delay(mut self, delay: Duration) -> Self {
        self.peer_delay = delay;
        self
    }

    /// 改服务端看到的客户端身份（同一个端点上模拟不同的客户端）。
    pub fn with_client_identity(mut self, id: PeerIdentity) -> Self {
        self.client_identity = id;
        self
    }

    /// 到目前为止建连了几次。
    pub fn attempts(&self) -> u32 {
        self.attempts.load(Ordering::SeqCst)
    }
}

impl Connector for MemoryConnector {
    type Conn = MemoryConn;

    async fn connect(&self, endpoint: &Endpoint) -> io::Result<MemoryConn> {
        if endpoint != &self.endpoint {
            return Err(io::Error::new(
                io::ErrorKind::NotFound,
                format!("没有这个内存端点：{endpoint:?}"),
            ));
        }
        let attempt = self.attempts.fetch_add(1, Ordering::SeqCst);
        let (client_side, server_side) = tokio::io::duplex(self.pipe_bytes);
        let server_conn = MemoryConn {
            stream: server_side,
            peer: self.client_identity.clone(),
            peer_delay: self.peer_delay,
        };
        self.tx
            .send(server_conn)
            .map_err(|_| io::Error::new(io::ErrorKind::ConnectionRefused, "内存端点没有在监听"))?;
        Ok(MemoryConn {
            stream: client_side,
            peer: (self.server_identity)(attempt),
            peer_delay: self.peer_delay,
        })
    }
}

pub struct MemoryConn {
    stream: DuplexStream,
    peer: PeerIdentity,
    peer_delay: Duration,
}

impl Connection for MemoryConn {
    type Read = ReadHalf<DuplexStream>;
    type Write = WriteHalf<DuplexStream>;

    async fn peer(&self) -> Result<PeerIdentity, PeerError> {
        if !self.peer_delay.is_zero() {
            tokio::time::sleep(self.peer_delay).await;
        }
        Ok(self.peer.clone())
    }

    fn into_split(self) -> (Self::Read, Self::Write) {
        tokio::io::split(self.stream)
    }
}
