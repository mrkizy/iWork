//! 流式消息的发布链路（审查 2026-11-13-m1-t10a-d7 1/F-4；T10a 方案 2.7 G6 前后对照）。
//!
//! 真实的 `PublishObserver`（写线程上的提交观察者）、`LiveDeltas`（流式增量的出口）、ipc 的 `Server`/`Publisher`
//! 与真客户端（进程内传输）。订阅范围用 `iwork_app::subscription::scope_streams`，和后台同一份：`SIDEBAR` 个侧栏订阅，
//! 一个打开着这个对话的会话订阅。每条消息：提交「开始流式」→ 发 `IWORK_BENCH_LIVE_DELTAS` 段增量 → 提交「结束」。
//!
//! 每组一行 JSON 到 stdout：
//! - `observe_appended`：观察者处理一批「开始流式」（单条事件）的耗时（写线程上，纳秒）；
//! - `delta_publish`：`LiveDeltas::message_delta` 一次调用的耗时（纳秒）；
//! - `delta_to_conversation`：调用到会话订阅的客户端解出这段增量的延迟（微秒）；
//! - `sidebar`：侧栏订阅一共收到的增量段数（G6 之后应为 0）。
//!
//! 同一份文件放进改动前的树里也能编（只用 main 上已有的接口），前后对照见 `scripts/bench-publish-live-ab.sh`。
//! 运行：`cargo bench -p iwork-daemon --bench publish_live`；`IWORK_BENCH_LIVE_MESSAGES`（默认 200）、
//! `IWORK_BENCH_LIVE_DELTAS`（默认 50）调规模。

#[path = "../../iwork-store/tests/support/fixtures.rs"]
#[allow(dead_code)]
mod fixtures;

use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use fixtures::*;
use iwork_daemon::publish::{LiveDeltas, PublishObserver, PublishSink, PublishState};
use iwork_domain::common::Actor;
use iwork_domain::event::{DomainEvent, MessageAppended, MessageCompleted};
use iwork_domain::id::Seq;
use iwork_domain::message::{MessageBody, MessageStatus};
use iwork_ipc::client::{Client, ClientConfig, Incoming, IncomingItem};
use iwork_ipc::peer::{ImageId, Manifest, PeerIdentity, SelfIdentity, SignerIdentity, UserId};
use iwork_ipc::server::{
    ChunkRefused, ConnectionContext, EventSource, Published, Publisher, ReplayPage, RequestHandler,
    RoutedEvent, Server, ServerConfig, SourceError,
};
use iwork_ipc::transport::Endpoint;
use iwork_ipc::transport::memory;
use iwork_ports::live::{LivePublisher, MessageDelta};
use iwork_ports::store::StoredEvent;
use iwork_proto::pb;
use iwork_proto::routing::StreamKey;
use iwork_store::{CommitObserver, CommittedBatch};

const SIDEBAR: usize = 4;
const CONV: u128 = 10;

type Log = Arc<Mutex<Vec<Arc<RoutedEvent>>>>;

/// 发布的去处：先记进日志（服务端的 head 与补发读它），再交给 ipc 的 `Publisher`，和后台写线程一样先有后发。
struct Tee {
    log: Log,
    publisher: Publisher,
}

impl PublishSink for Tee {
    fn publish(&self, p: Published) {
        if let Published::Persisted(e) = &p {
            self.log.lock().unwrap().push(e.clone());
        }
        self.publisher.publish(p);
    }
}

#[derive(Clone)]
struct Source {
    log: Log,
}

impl EventSource for Source {
    async fn head_seq(&self) -> Result<u64, SourceError> {
        Ok(self.log.lock().unwrap().len() as u64)
    }
    fn scope_streams(&self, scope: &pb::SubscriptionScope) -> Vec<StreamKey> {
        iwork_app::subscription::scope_streams(scope)
    }
    async fn replay(
        &self,
        keys: &[StreamKey],
        from: u64,
        to: u64,
        limit: usize,
    ) -> Result<ReplayPage, SourceError> {
        let log = self.log.lock().unwrap();
        let events: Vec<_> = log[from as usize..to as usize]
            .iter()
            .filter(|e| e.routes.iter().any(|r| keys.iter().any(|k| k.covers(r))))
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
struct Handler;

impl RequestHandler for Handler {
    async fn handle(&self, _: &ConnectionContext, req: pb::Request) -> pb::Response {
        pb::Response {
            request_id: req.request_id,
            body: None,
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

fn env_or(key: &str, default: usize) -> usize {
    std::env::var(key)
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(default)
}

fn stored(seq: u64, event: DomainEvent) -> StoredEvent {
    StoredEvent {
        seq: Seq(seq),
        at: iwork_domain::common::Timestamp(T0),
        actor: Actor("bench".into()),
        event,
    }
}

async fn subscribed(inc: &mut Incoming) {
    loop {
        if let Some(IncomingItem::Subscribed { .. }) = inc.next().await {
            return;
        }
    }
}

/// 一段流式增量（`offset` 当作全局序号，用来查发出的时刻）。
fn delta_of(e: &pb::Event) -> Option<u64> {
    match &e.body {
        Some(pb::event::Body::Transient(t)) => match &t.event {
            Some(pb::transient_event::Event::MessageDelta(d)) => Some(d.offset),
            _ => None,
        },
        _ => None,
    }
}

fn pct(v: &mut [u128], p: usize) -> u128 {
    v.sort_unstable();
    v[(v.len() * p / 100).min(v.len() - 1)]
}

fn line(case: &str, unit: &str, v: &mut [u128]) {
    let (p50, p95) = (pct(v, 50), pct(v, 95));
    println!(
        "{{\"bench\":\"publish_live\",\"case\":\"{case}\",\"n\":{},\"p50_{unit}\":{p50},\"p95_{unit}\":{p95}}}",
        v.len()
    );
}

async fn run(messages: usize, deltas: usize) {
    let log: Log = Arc::default();
    let (server, publisher) = Server::new(
        ServerConfig::new("bench"),
        own(1),
        Handler,
        Source { log: log.clone() },
    );
    let (listener, connector) = memory::endpoint("bench", identity(2), Arc::new(|_| identity(1)));
    tokio::spawn(server.serve(listener, tokio_util::sync::CancellationToken::new()));
    let endpoint = Endpoint::Memory("bench".into());
    let connect = || async {
        Client::connect(
            &connector,
            &endpoint,
            &own(2),
            ClientConfig::new(pb::ClientKind::Desktop, "bench"),
        )
        .await
        .unwrap()
    };

    let total = (messages * deltas) as u64;
    let sent: Arc<Mutex<Vec<Instant>>> = Arc::new(Mutex::new(Vec::with_capacity(total as usize)));

    // 打开着这个对话的会话订阅：收齐全部增量为止，记延迟
    let (conv_client, mut conv_inc) = connect().await;
    let scope = pb::SubscriptionScope {
        kind: Some(pb::subscription_scope::Kind::Conversation(
            pb::ConversationScope {
                conversation_id: cid(CONV).to_string(),
            },
        )),
    };
    conv_client.subscribe(1, scope, 0).await.unwrap();
    subscribed(&mut conv_inc).await;
    let sent_c = sent.clone();
    let conv_reader = tokio::spawn(async move {
        let _keep = conv_client;
        let mut lat = Vec::with_capacity(total as usize);
        while (lat.len() as u64) < total {
            match conv_inc.next().await {
                Some(IncomingItem::Event(e)) => {
                    if let Some(i) = delta_of(&e) {
                        lat.push(sent_c.lock().unwrap()[i as usize].elapsed().as_micros());
                    }
                }
                Some(IncomingItem::Closed(r)) => panic!("会话订阅被关：{r:?}"),
                Some(_) => {}
                None => panic!("会话订阅断了"),
            }
        }
        lat
    });

    // 侧栏订阅：数收到的增量段数，会话订阅收齐后再等一会儿就停
    let (stop_tx, stop_rx) = tokio::sync::watch::channel(false);
    let mut sidebar_readers = Vec::new();
    for _ in 0..SIDEBAR {
        let (c, mut inc) = connect().await;
        let scope = pb::SubscriptionScope {
            kind: Some(pb::subscription_scope::Kind::Sidebar(pb::SidebarScope {})),
        };
        c.subscribe(1, scope, 0).await.unwrap();
        subscribed(&mut inc).await;
        let mut stop = stop_rx.clone();
        sidebar_readers.push(tokio::spawn(async move {
            let _keep = c;
            let mut n = 0u64;
            loop {
                tokio::select! {
                    item = inc.next() => match item {
                        Some(IncomingItem::Event(e)) => {
                            if delta_of(&e).is_some() {
                                n += 1;
                            }
                        }
                        Some(IncomingItem::Closed(r)) => panic!("侧栏订阅被关：{r:?}"),
                        Some(_) => {}
                        None => return n,
                    },
                    _ = stop.changed() => return n,
                }
            }
        }));
    }

    // 写线程：观察者在普通线程里同步回调，和存储的写线程一样
    let sink = Arc::new(Tee { log, publisher });
    let state = PublishState::new();
    let live = LiveDeltas::new(sink.clone(), state.routes.clone());
    let sent_w = sent.clone();
    let writer = std::thread::spawn(move || {
        let mut obs = PublishObserver::new(sink, state);
        let mut seq = 0u64;
        let mut next = |e: DomainEvent| {
            seq += 1;
            stored(seq, e)
        };
        let mut observe = Vec::with_capacity(messages);
        let mut publish = Vec::with_capacity(messages * deltas);
        for m in 0..messages {
            let id = 1000 + m as u128;
            let appended = [next(
                MessageAppended {
                    message: assistant_message(id, CONV, 20),
                }
                .into(),
            )];
            let t = Instant::now();
            obs.committed(CommittedBatch {
                events: &appended,
                outbox_added: 0,
            });
            observe.push(t.elapsed().as_nanos());
            for d in 0..deltas {
                let i = (m * deltas + d) as u64;
                let delta = MessageDelta {
                    message_id: mid(id),
                    offset: i,
                    text: "一段流式正文，长度和真实的增量差不多。".into(),
                };
                sent_w.lock().unwrap().push(Instant::now());
                let t = Instant::now();
                live.message_delta(delta);
                publish.push(t.elapsed().as_nanos());
                // 节拍：每段之间让出一下，不把一万段一次压进去（真实增量是一段一段来的）
                std::thread::sleep(Duration::from_micros(50));
            }
            let completed = [next(
                MessageCompleted {
                    conversation_id: cid(CONV),
                    message_id: mid(id),
                    body: MessageBody::Inline("完".into()),
                    status: MessageStatus::Completed,
                }
                .into(),
            )];
            obs.committed(CommittedBatch {
                events: &completed,
                outbox_added: 0,
            });
        }
        (observe, publish)
    });
    let (mut observe, mut publish) = tokio::task::spawn_blocking(move || writer.join().unwrap())
        .await
        .unwrap();
    let mut lat = conv_reader.await.unwrap();
    tokio::time::sleep(Duration::from_millis(500)).await;
    stop_tx.send(true).unwrap();
    let mut sidebar = 0;
    for r in sidebar_readers {
        sidebar += r.await.unwrap();
    }

    line("observe_appended", "ns", &mut observe);
    line("delta_publish", "ns", &mut publish);
    line("delta_to_conversation", "us", &mut lat);
    println!(
        "{{\"bench\":\"publish_live\",\"case\":\"sidebar\",\"subscribers\":{SIDEBAR},\"deltas_sent\":{total},\"deltas_received\":{sidebar}}}"
    );
}

fn main() {
    let messages = env_or("IWORK_BENCH_LIVE_MESSAGES", 200);
    let deltas = env_or("IWORK_BENCH_LIVE_DELTAS", 50);
    tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .unwrap()
        .block_on(run(messages, deltas));
}
