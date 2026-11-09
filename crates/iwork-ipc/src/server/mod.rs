//! 服务端（方案 2.6–2.10）：握手、命令串行、查询分道、每连接一条事件流、背压。
//!
//! T04 的组合顺序：`SelfIdentity::detect()` → `LocalEndpoint::for_current_user()` → 拿 `run/daemon.lock` →
//! 打开存储、跑崩溃恢复 → [`Server::new`]（拿到 [`Publisher`]，此后才允许提交把事件发布进来）→
//! `LocalListener::bind` → [`Server::serve`]。

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use tokio::sync::watch;
use tokio_util::sync::CancellationToken;

mod budget;
mod connection;
mod handler;
mod stream;

pub use handler::{
    ChunkRefused, ConnectionContext, EventSource, PeerSummary, Published, ReplayPage,
    RequestHandler, RoutedEvent, SourceError,
};
pub use stream::FailCause;

use crate::peer::{SelfIdentity, TrustPolicy};
use crate::transport::Listener;
use connection::ConnShared;

/// 服务端配置。默认值见 [`ServerConfig::new`]。
#[derive(Debug, Clone)]
pub struct ServerConfig {
    /// 默认 `iwork_proto::PROTOCOL_MAJOR`；可配只为在同一进程里扮演新旧两版后台（验收 3a）。
    pub protocol_major: u32,
    pub protocol_minor: u32,
    pub server_version: String,
    /// 每连接预算：发送队列 + 补发缓冲。默认 8MiB。
    pub budget_limit: usize,
    /// 只给补发用的保底，默认 1MiB。只是优化：正确性靠「关连接 → 客户端重连续传」。
    pub replay_reserve: usize,
    /// 续传最多补多少条，再多就给快照。默认 50 000。
    pub replay_limit: u64,
    /// 补发一页多少条。默认 1000。
    pub replay_page: usize,
    /// 补发等预算的上限，超时关连接。默认 2s。
    pub replay_reserve_timeout: Duration,
    /// 一次交接（取数 + 等发布）或补发一页的上限，超时关连接。默认 10s。
    pub handover_timeout: Duration,
    /// 一条连接同时存在的范围上限，超过按协议违规。默认 256。
    pub max_scopes: usize,
    /// 并行查询道的并发上限。默认 4。
    pub query_concurrency: usize,
    /// 等 `Hello` 的上限。默认 5s。
    pub handshake_timeout: Duration,
    /// 停机时每条连接把队列写完的上限。默认 1s。
    pub shutdown_flush: Duration,
    pub trust: TrustPolicy,
}

impl ServerConfig {
    pub fn new(server_version: impl Into<String>) -> Self {
        Self {
            protocol_major: iwork_proto::PROTOCOL_MAJOR,
            protocol_minor: 0,
            server_version: server_version.into(),
            budget_limit: 8 * 1024 * 1024,
            replay_reserve: 1024 * 1024,
            replay_limit: 50_000,
            replay_page: 1000,
            replay_reserve_timeout: Duration::from_secs(2),
            handover_timeout: Duration::from_secs(10),
            max_scopes: 256,
            query_concurrency: 4,
            handshake_timeout: Duration::from_secs(5),
            shutdown_flush: Duration::from_secs(1),
            trust: TrustPolicy::RequireSigner,
        }
    }
}

/// 连接登记表与发布水位：`Publisher` 与各连接共享。
pub(crate) struct Hub {
    conns: Mutex<HashMap<u64, Arc<ConnShared>>>,
    /// 发布水位：已发布的最高持久事件 seq 与启动基线（存储 head）的较大者。
    /// 交接要等它追上读事务看到的 S（方案 2.8「交接」第 4 步）。
    published_head: watch::Sender<u64>,
    /// 最后一次经 `publish` 发布的 seq，只用来查乱序。不能用水位查：基线可能已经包含「已提交、还没发布」的事件，
    /// 它们随后的正常发布会被误报。
    last_published: AtomicU64,
    next_conn_id: AtomicU64,
}

impl Hub {
    fn new() -> Self {
        Self {
            conns: Mutex::new(HashMap::new()),
            published_head: watch::Sender::new(0),
            last_published: AtomicU64::new(0),
            next_conn_id: AtomicU64::new(1),
        }
    }

    fn connections(&self) -> Vec<Arc<ConnShared>> {
        self.conns
            .lock()
            .expect("连接表锁中毒")
            .values()
            .cloned()
            .collect()
    }

    fn raise_head(&self, seq: u64) {
        self.published_head.send_if_modified(|h| {
            if seq > *h {
                *h = seq;
                true
            } else {
                false
            }
        });
    }
}

/// 存储写线程在提交后调用。非阻塞：对每条连接只拿一次锁、推一次队列或缓冲，永不等 socket、永不等预算。
#[derive(Clone)]
pub struct Publisher {
    hub: Arc<Hub>,
}

impl Publisher {
    /// 持久事件必须按 seq 递增调用（单写者，设计 5.2）。
    pub fn publish(&self, p: Published) {
        match p {
            Published::Persisted(e) => {
                let seq = e.seq();
                let last = self.hub.last_published.fetch_max(seq, Ordering::SeqCst);
                if seq <= last {
                    // 乱序发布会让游标跳过事件。修在调用方；这里照常投递（可能重复，不丢）。
                    tracing::error!(target: "iwork_ipc", seq, last_published = last, "持久事件乱序发布");
                    debug_assert!(
                        seq > last,
                        "持久事件乱序发布：seq {seq} ≤ 上一次发布的 {last}"
                    );
                }
                for conn in self.hub.connections() {
                    conn.on_persisted(&e);
                }
                // 先进各连接的队列 / 缓冲，再推进水位：等水位的交接醒来时，S 一定已在缓冲里。
                self.hub.raise_head(seq);
            }
            Published::Transient { event, routes } => {
                for conn in self.hub.connections() {
                    conn.on_transient(&event, &routes);
                }
            }
        }
    }
}

pub(crate) struct Shared<H, S> {
    pub(crate) cfg: ServerConfig,
    pub(crate) own: SelfIdentity,
    pub(crate) handler: Arc<H>,
    pub(crate) source: Arc<S>,
    pub(crate) hub: Arc<Hub>,
}

pub struct Server<H, S> {
    shared: Arc<Shared<H, S>>,
}

impl<H: RequestHandler, S: EventSource> Server<H, S> {
    pub fn new(cfg: ServerConfig, own: SelfIdentity, handler: H, source: S) -> (Self, Publisher) {
        let hub = Arc::new(Hub::new());
        let publisher = Publisher {
            hub: Arc::clone(&hub),
        };
        let shared = Arc::new(Shared {
            cfg,
            own,
            handler: Arc::new(handler),
            source: Arc::new(source),
            hub,
        });
        (Self { shared }, publisher)
    }

    /// 跑到 `shutdown` 触发；之后停止接受连接，给每条连接 `shutdown_flush` 把队列写完再关。
    ///
    /// 接受第一条连接之前先把存储里的 head 设为发布水位的基线：否则非空库重启、没有新提交时，
    /// 第一次交接会一直等一个不会再发布的 S（方案 2.10）。
    pub async fn serve<L: Listener>(self, mut listener: L, shutdown: CancellationToken) {
        match self.shared.source.head_seq().await {
            Ok(head) => self.shared.hub.raise_head(head),
            Err(e) => {
                tracing::error!(target: "iwork_ipc", error = %e, "读存储 head 失败，发布水位从 0 起");
            }
        }
        let mut tasks = tokio::task::JoinSet::new();
        loop {
            tokio::select! {
                biased;
                () = shutdown.cancelled() => break,
                accepted = listener.accept() => match accepted {
                    Ok(conn) => {
                        let id = self.shared.hub.next_conn_id.fetch_add(1, Ordering::Relaxed);
                        tasks.spawn(connection::run(Arc::clone(&self.shared), conn, id, shutdown.clone()));
                    }
                    Err(e) => {
                        tracing::warn!(target: "iwork_ipc", error = %e, "accept 失败");
                        // 避免 accept 持续出错时空转。
                        tokio::time::sleep(Duration::from_millis(50)).await;
                    }
                },
                Some(_) = tasks.join_next(), if !tasks.is_empty() => {}
            }
        }
        while tasks.join_next().await.is_some() {}
    }
}

#[cfg(test)]
mod tests;
