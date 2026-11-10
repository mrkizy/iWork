//! ipc 热路径基准（T03 方案第 6 节）。`cargo bench -p iwork-bench --bench ipc`。
//!
//! 全部跑在进程内传输上，量的是 ipc 自身（编码、入队、写、解码、交接），不含存储读取（归 T02）与真实 socket。
//! `IWORK_BENCH_FANOUT_SECS` 调推送扇出的时长（默认 10 秒，方案的设定）。

use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use iwork_ipc::client::{Client, ClientConfig, Incoming, IncomingItem};
use iwork_ipc::peer::{ImageId, Manifest, PeerIdentity, SelfIdentity, SignerIdentity, UserId};
use iwork_ipc::server::{
    ChunkRefused, ConnectionContext, EventSource, Published, Publisher, ReplayPage, RequestHandler,
    RoutedEvent, Server, ServerConfig, SourceError,
};
use iwork_ipc::transport::Endpoint;
use iwork_ipc::transport::memory::{self, MemoryConnector};
use iwork_proto::pb;
use iwork_proto::routing::StreamKey;

type Log = Arc<Mutex<Vec<Arc<RoutedEvent>>>>;

#[derive(Clone)]
struct Source {
    log: Log,
}

impl EventSource for Source {
    async fn head_seq(&self) -> Result<u64, SourceError> {
        Ok(self.log.lock().unwrap().len() as u64)
    }
    fn scope_streams(&self, scope: &pb::SubscriptionScope) -> Vec<StreamKey> {
        if scope.kind.is_some() {
            vec![StreamKey::all("conversation")]
        } else {
            Vec::new()
        }
    }
    async fn replay(
        &self,
        _: &[StreamKey],
        from: u64,
        to: u64,
        limit: usize,
    ) -> Result<ReplayPage, SourceError> {
        let log = self.log.lock().unwrap();
        let events: Vec<_> = log[from as usize..to as usize]
            .iter()
            .take(limit)
            .cloned()
            .collect();
        let next_from = if events.len() == limit {
            events.last().unwrap().seq()
        } else {
            to
        };
        Ok(ReplayPage { events, next_from })
    }
    async fn snapshot(
        &self,
        scopes: &[pb::SubscriptionScope],
    ) -> Result<(pb::Snapshot, u64), SourceError> {
        let s = self.log.lock().unwrap().len() as u64;
        Ok((
            pb::Snapshot {
                results: scopes
                    .iter()
                    .map(|_| pb::QueryResult { seq: s, kind: None })
                    .collect(),
            },
            s,
        ))
    }
}

#[derive(Clone)]
struct Handler {
    log: Log,
    query_delay: Arc<Mutex<Duration>>,
}

impl RequestHandler for Handler {
    async fn handle(&self, _: &ConnectionContext, req: pb::Request) -> pb::Response {
        let s = self.log.lock().unwrap().len() as u64;
        let d = *self.query_delay.lock().unwrap();
        if !d.is_zero() {
            tokio::time::sleep(d).await;
        }
        pb::Response {
            request_id: req.request_id,
            body: Some(pb::response::Body::Result(pb::QueryResult {
                seq: s,
                kind: None,
            })),
        }
    }
    async fn chunk(&self, _: &ConnectionContext, _: pb::Chunk) -> Result<(), ChunkRefused> {
        Ok(())
    }
    async fn connection_closed(&self, _: &ConnectionContext) {}
}

fn identity(img: u8) -> PeerIdentity {
    PeerIdentity {
        pid: 1,
        user: UserId::Uid(1),
        exe: Some(PathBuf::from("/i/x")),
        signer: SignerIdentity::Image(ImageId::Sha256([img; 32])),
    }
}

fn own(img: u8) -> SelfIdentity {
    let entries = format!(
        "\"{}\",\"{}\"",
        ImageId::Sha256([1; 32]),
        ImageId::Sha256([2; 32])
    );
    SelfIdentity {
        user: UserId::Uid(1),
        signer: SignerIdentity::Image(ImageId::Sha256([img; 32])),
        exe_dir: PathBuf::from("/i"),
        manifest: Some(
            Manifest::parse(&format!(
                r#"{{"manifest_version":1,"app_version":"0","binaries":{{"x":[{entries}]}}}}"#
            ))
            .unwrap(),
        ),
    }
}

struct Bench {
    log: Log,
    handler: Handler,
    publisher: Publisher,
    connector: MemoryConnector,
    endpoint: Endpoint,
}

impl Bench {
    fn start() -> Bench {
        let log: Log = Arc::default();
        let handler = Handler {
            log: log.clone(),
            query_delay: Arc::default(),
        };
        let (server, publisher) = Server::new(
            ServerConfig::new("bench"),
            own(1),
            handler.clone(),
            Source { log: log.clone() },
        );
        let (listener, connector) =
            memory::endpoint("bench", identity(2), Arc::new(|_| identity(1)));
        tokio::spawn(server.serve(listener, tokio_util::sync::CancellationToken::new()));
        Bench {
            log,
            handler,
            publisher,
            connector,
            endpoint: Endpoint::Memory("bench".into()),
        }
    }

    async fn connect(&self) -> (Client, Incoming) {
        Client::connect(
            &self.connector,
            &self.endpoint,
            &own(2),
            ClientConfig::new(pb::ClientKind::Desktop, "bench"),
        )
        .await
        .unwrap()
    }

    /// 提交一条约 `size` 字节的事件（不发布）。
    fn commit(&self, size: usize) -> Arc<RoutedEvent> {
        let mut log = self.log.lock().unwrap();
        let e = Arc::new(RoutedEvent::new(
            pb::PersistedEvent {
                seq: log.len() as u64 + 1,
                actor: "x".repeat(size),
                ..Default::default()
            },
            vec![StreamKey::one("conversation", "c1")],
        ));
        log.push(e.clone());
        e
    }
}

fn scope() -> pb::SubscriptionScope {
    pb::SubscriptionScope {
        kind: Some(pb::subscription_scope::Kind::Conversation(
            pb::ConversationScope {
                conversation_id: "c1".into(),
            },
        )),
    }
}

fn pct(v: &mut [Duration], p: usize) -> Duration {
    v.sort();
    v[(v.len() * p / 100).min(v.len() - 1)]
}

fn report(name: &str, v: &mut [Duration]) {
    let (p50, p95, p99) = (pct(v, 50), pct(v, 95), pct(v, 99));
    println!(
        "{name:<44} n={:<6} P50={p50:>10.1?} P95={p95:>10.1?} P99={p99:>10.1?}",
        v.len()
    );
}

async fn subscribed(inc: &mut Incoming) {
    loop {
        if let Some(IncomingItem::Subscribed { .. }) = inc.next().await {
            return;
        }
    }
}

async fn connect_handshake_latency() {
    let b = Bench::start();
    let mut v = Vec::new();
    for _ in 0..500 {
        let t = Instant::now();
        let (c, _i) = b.connect().await;
        v.push(t.elapsed());
        drop(c);
    }
    report("connect_handshake_latency（内存传输）", &mut v);
}

/// 1600 事件 / 秒（16 成员 × 100 片），publish 到客户端解出的延迟。
async fn ipc_publish_fanout(conns: usize, secs: u64) {
    let b = Bench::start();
    let sent: Arc<Mutex<Vec<Instant>>> = Arc::default();
    let total = 1600 * secs;
    let mut readers = Vec::new();
    for _ in 0..conns {
        let (c, mut inc) = b.connect().await;
        c.subscribe(1, scope(), 0).await.unwrap();
        subscribed(&mut inc).await;
        let sent = sent.clone();
        readers.push(tokio::spawn(async move {
            let _keep = c;
            let mut lat = Vec::with_capacity(total as usize);
            while (lat.len() as u64) < total {
                if let Some(IncomingItem::Event(e)) = inc.next().await
                    && let Some(pb::event::Body::Persisted(p)) = &e.body
                {
                    let t0 = sent.lock().unwrap()[p.seq as usize - 1];
                    lat.push(t0.elapsed());
                }
            }
            lat
        }));
    }
    // 存储写线程：普通线程里按节拍调 publish（publish 是同步、非阻塞的）。
    let (log, publisher, sent2) = (b.log.clone(), b.publisher.clone(), sent.clone());
    let writer = std::thread::spawn(move || {
        let start = Instant::now();
        for i in 0..total {
            let e = {
                let mut l = log.lock().unwrap();
                let e = Arc::new(RoutedEvent::new(
                    pb::PersistedEvent {
                        seq: l.len() as u64 + 1,
                        actor: "x".repeat(200),
                        ..Default::default()
                    },
                    vec![StreamKey::one("conversation", "c1")],
                ));
                l.push(e.clone());
                e
            };
            sent2.lock().unwrap().push(Instant::now());
            publisher.publish(Published::Persisted(e));
            let due = start + Duration::from_micros(625 * (i + 1));
            if let Some(w) = due.checked_duration_since(Instant::now()) {
                std::thread::sleep(w);
            }
        }
    });
    let mut all = Vec::new();
    for r in readers {
        all.extend(r.await.unwrap());
    }
    writer.join().unwrap();
    report(
        &format!("ipc_publish_fanout（{conns} 条连接，1600/s × {secs}s）"),
        &mut all,
    );
}

async fn ipc_replay_10k() {
    let b = Bench::start();
    for _ in 0..10_000 {
        b.commit(200);
    }
    let mut v = Vec::new();
    for _ in 0..20 {
        let (c, mut inc) = b.connect().await;
        let t = Instant::now();
        c.subscribe(1, scope(), 1).await.unwrap();
        let mut n = 0;
        while n < 9_999 {
            if let Some(IncomingItem::Event(_)) = inc.next().await {
                n += 1;
            }
        }
        v.push(t.elapsed());
    }
    report("ipc_replay_10k（续传 9 999 条，每条约 200B）", &mut v);
}

async fn ipc_scope_change_snapshot() {
    let b = Bench::start();
    for _ in 0..1000 {
        b.commit(200);
    }
    let (c, mut inc) = b.connect().await;
    let mut v = Vec::new();
    for i in 0..500u32 {
        let t = Instant::now();
        c.subscribe(i % 6 + 1, scope(), 0).await.unwrap();
        subscribed(&mut inc).await;
        v.push(t.elapsed());
    }
    report("ipc_scope_change_snapshot（Subscribe→Subscribed）", &mut v);
}

/// 交接期间事件只进缓冲：量查询在飞时事件的额外延迟（预期 ≈ 查询时长）。
async fn ipc_view_query_handover(query_ms: u64) {
    let b = Bench::start();
    *b.handler.query_delay.lock().unwrap() = Duration::from_millis(query_ms);
    let (c, mut inc) = b.connect().await;
    c.subscribe(1, scope(), 0).await.unwrap();
    subscribed(&mut inc).await;
    let c = Arc::new(c);
    let sent: Arc<Mutex<Vec<Instant>>> = Arc::default();
    let rounds = 20u64;
    let per_round = 40u64;
    let total = rounds * per_round;
    let counter = Arc::new(AtomicU64::new(0));
    let reader = {
        let (sent, counter) = (sent.clone(), counter.clone());
        tokio::spawn(async move {
            let mut lat = Vec::new();
            while counter.load(Ordering::SeqCst) < total {
                match inc.next().await {
                    Some(IncomingItem::Event(e)) => {
                        let Some(pb::event::Body::Persisted(p)) = &e.body else {
                            continue;
                        };
                        lat.push(sent.lock().unwrap()[p.seq as usize - 1].elapsed());
                        counter.fetch_add(1, Ordering::SeqCst);
                    }
                    Some(_) => {}
                    None => break,
                }
            }
            lat
        })
    };
    for _ in 0..rounds {
        let q = {
            let c = c.clone();
            tokio::spawn(async move {
                c.view_query(pb::Query {
                    kind: Some(pb::query::Kind::GetConversation(
                        pb::GetConversation::default(),
                    )),
                })
                .await
            })
        };
        for _ in 0..per_round {
            let e = b.commit(200);
            sent.lock().unwrap().push(Instant::now());
            b.publisher.publish(Published::Persisted(e));
            tokio::time::sleep(Duration::from_micros(625)).await;
        }
        q.await.unwrap().unwrap();
    }
    let mut lat = reader.await.unwrap();
    report(
        &format!("ipc_view_query_handover（查询 {query_ms}ms 在飞）"),
        &mut lat,
    );
}

fn main() {
    let secs = std::env::var("IWORK_BENCH_FANOUT_SECS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(10);
    let rt = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .unwrap();
    rt.block_on(async {
        connect_handshake_latency().await;
        ipc_publish_fanout(1, secs).await;
        ipc_publish_fanout(4, secs).await;
        ipc_replay_10k().await;
        ipc_scope_change_snapshot().await;
        ipc_view_query_handover(5).await;
        ipc_view_query_handover(50).await;
    });
}
