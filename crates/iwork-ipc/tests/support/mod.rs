//! 集成测试夹具：内存「世界」（事件日志 + 投影）、事件源、handler、审计捕获、身份。
//!
//! 世界里每条事件是对一个实体的写入或删除，路由键 = 实体所在的类别（`conversation:<id>` 或 `project:<id>`）。
//! 事件源与 handler 读同一个世界；客户端模型按 seq 查世界里的操作来应用事件，快照内容 = 世界在 S 时刻的投影。
#![allow(dead_code)]

use std::collections::{BTreeMap, BTreeSet};
use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use iwork_ipc::client::{Client, ClientConfig, ConnectError, Incoming, IncomingItem};
use iwork_ipc::peer::{ImageId, Manifest, PeerIdentity, SelfIdentity, SignerIdentity, UserId};
use iwork_ipc::server::{
    ChunkRefused, ConnectionContext, EventSource, Published, Publisher, ReplayPage, RequestHandler,
    RoutedEvent, Server, ServerConfig, SourceError,
};
use iwork_ipc::transport::Endpoint;
use iwork_ipc::transport::memory::{self, MemoryConnector};
use iwork_proto::pb;
use iwork_proto::routing::StreamKey;
use tokio_util::sync::CancellationToken;

pub const UID: u32 = 501;
pub const INSTALL_DIR: &str = "/opt/iwork";

pub fn image(b: u8) -> ImageId {
    ImageId::Sha256([b; 32])
}

pub const CLIENT_IMAGE: u8 = 0xC1;
pub const SERVER_IMAGE: u8 = 0x51;

pub fn manifest(images: &[u8]) -> Manifest {
    let entries: Vec<String> = images
        .iter()
        .map(|b| format!("\"{}\"", image(*b)))
        .collect();
    Manifest::parse(&format!(
        r#"{{"manifest_version":1,"app_version":"1.0.0","binaries":{{"iwork":[{}]}}}}"#,
        entries.join(",")
    ))
    .unwrap()
}

/// 无证书构建的一端：自己的映像 + 只登记 `trusted` 的清单。
pub fn own(self_image: u8, trusted: &[u8]) -> SelfIdentity {
    SelfIdentity {
        user: UserId::Uid(UID),
        signer: SignerIdentity::Image(image(self_image)),
        exe_dir: PathBuf::from(INSTALL_DIR),
        manifest: Some(manifest(trusted)),
    }
}

pub fn server_own() -> SelfIdentity {
    own(SERVER_IMAGE, &[SERVER_IMAGE, CLIENT_IMAGE])
}

pub fn client_own() -> SelfIdentity {
    own(CLIENT_IMAGE, &[SERVER_IMAGE, CLIENT_IMAGE])
}

pub fn peer(pid: u32, uid: u32, img: u8) -> PeerIdentity {
    PeerIdentity {
        pid,
        user: UserId::Uid(uid),
        exe: Some(PathBuf::from(INSTALL_DIR).join("iwork")),
        signer: SignerIdentity::Image(image(img)),
    }
}

pub fn client_peer() -> PeerIdentity {
    peer(2000, UID, CLIENT_IMAGE)
}

pub fn server_peer() -> PeerIdentity {
    peer(1000, UID, SERVER_IMAGE)
}

pub fn client_cfg() -> ClientConfig {
    ClientConfig::new(pb::ClientKind::Desktop, "test")
}

pub fn server_cfg() -> ServerConfig {
    ServerConfig::new("test-server")
}

// ---------------------------------------------------------------- 世界

/// 一条事件对实体做的事。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Op {
    Upsert { entity: String, value: u64 },
    Delete { entity: String },
}

impl Op {
    pub fn entity(&self) -> &str {
        match self {
            Op::Upsert { entity, .. } | Op::Delete { entity } => entity,
        }
    }
}

/// 实体名形如 `conversation:c1`、`project:p1`；路由键就是它。
pub fn route_of(entity: &str) -> StreamKey {
    let (kind, id) = entity.split_once(':').expect("实体名形如 kind:id");
    let kind: &'static str = match kind {
        "conversation" => "conversation",
        "project" => "project",
        other => panic!("测试世界只有两类实体：{other}"),
    };
    StreamKey::one(kind, id)
}

/// 测试里的范围 → 键：Sidebar 覆盖两类全部；Conversation{id} 覆盖那一个对话。
pub fn scope_keys(scope: &pb::SubscriptionScope) -> Vec<StreamKey> {
    use pb::subscription_scope::Kind;
    match &scope.kind {
        Some(Kind::Sidebar(_)) => vec![StreamKey::all("conversation"), StreamKey::all("project")],
        Some(Kind::Conversation(c)) if !c.conversation_id.is_empty() => {
            vec![StreamKey::one("conversation", c.conversation_id.clone())]
        }
        _ => Vec::new(),
    }
}

pub fn sidebar() -> pb::SubscriptionScope {
    pb::SubscriptionScope {
        kind: Some(pb::subscription_scope::Kind::Sidebar(pb::SidebarScope {})),
    }
}

pub fn conversation(id: &str) -> pb::SubscriptionScope {
    pb::SubscriptionScope {
        kind: Some(pb::subscription_scope::Kind::Conversation(
            pb::ConversationScope {
                conversation_id: id.into(),
            },
        )),
    }
}

pub fn covers(scopes: &[pb::SubscriptionScope], entity: &str) -> bool {
    let r = route_of(entity);
    scopes
        .iter()
        .any(|s| scope_keys(s).iter().any(|k| k.covers(&r)))
}

#[derive(Default)]
pub struct WorldState {
    /// 下标 i 是 seq i+1。
    pub events: Vec<(Arc<RoutedEvent>, Op)>,
    /// 已提交但还没发布的事件数（模拟「提交后发布」的间隙或发布停滞）。
    pub unpublished_from: usize,
}

#[derive(Clone, Default)]
pub struct World {
    pub state: Arc<Mutex<WorldState>>,
    /// 快照：在两部分之间插一次提交（第 n 次取快照时），模拟「不在一个读事务里」。
    pub snapshot_tear_once: Arc<Mutex<bool>>,
    /// 快照 / 查询 / 补发人为延迟。
    pub read_delay: Arc<Mutex<Duration>>,
    /// 取数永不返回。
    pub read_hangs: Arc<Mutex<bool>>,
    /// 每条事件的填充字节（放在 actor 里），用来快速撑满预算。
    pub pad: Arc<Mutex<usize>>,
}

impl World {
    pub fn head(&self) -> u64 {
        self.state.lock().unwrap().events.len() as u64
    }

    /// 提交一条事件（不发布）。
    pub fn commit(&self, op: Op) -> Arc<RoutedEvent> {
        let pad = *self.pad.lock().unwrap();
        let mut st = self.state.lock().unwrap();
        let seq = st.events.len() as u64 + 1;
        let e = Arc::new(RoutedEvent::new(
            pb::PersistedEvent {
                seq,
                at: None,
                actor: "u".repeat(1 + pad),
                event: None,
            },
            vec![route_of(op.entity())],
        ));
        st.events.push((Arc::clone(&e), op));
        e
    }

    /// 提交并发布。
    pub fn commit_publish(&self, publisher: &Publisher, op: Op) -> u64 {
        let e = self.commit(op);
        let seq = e.seq();
        publisher.publish(Published::Persisted(e));
        seq
    }

    pub fn op_at(&self, seq: u64) -> Op {
        self.state.lock().unwrap().events[seq as usize - 1]
            .1
            .clone()
    }

    /// seq 时刻的投影：实体 → 值。
    pub fn projection_at(&self, seq: u64) -> BTreeMap<String, u64> {
        let st = self.state.lock().unwrap();
        let mut p = BTreeMap::new();
        for (_, op) in st.events.iter().take(seq as usize) {
            match op {
                Op::Upsert { entity, value } => {
                    p.insert(entity.clone(), *value);
                }
                Op::Delete { entity } => {
                    p.remove(entity);
                }
            }
        }
        p
    }

    async fn delay(&self) {
        for _ in 0..3 {
            tokio::task::yield_now().await;
        }
        let d = *self.read_delay.lock().unwrap();
        if !d.is_zero() {
            tokio::time::sleep(d).await;
        }
        if *self.read_hangs.lock().unwrap() {
            std::future::pending::<()>().await;
        }
    }
}

/// 事件源：读世界。
#[derive(Clone)]
pub struct WorldSource {
    pub world: World,
    pub replay_calls: Arc<AtomicUsize>,
    pub snapshot_calls: Arc<AtomicUsize>,
}

impl WorldSource {
    pub fn new(world: World) -> Self {
        Self {
            world,
            replay_calls: Arc::default(),
            snapshot_calls: Arc::default(),
        }
    }
}

impl EventSource for WorldSource {
    async fn head_seq(&self) -> Result<u64, SourceError> {
        // 让出几次：真实存储读 head 要等读连接，测试里并发的提交要能落进「已暂停、还没读到 head」的窗口。
        for _ in 0..3 {
            tokio::task::yield_now().await;
        }
        Ok(self.world.head())
    }

    fn scope_streams(&self, scope: &pb::SubscriptionScope) -> Vec<StreamKey> {
        scope_keys(scope)
    }

    async fn replay(
        &self,
        keys: &[StreamKey],
        from: u64,
        to: u64,
        limit: usize,
    ) -> Result<ReplayPage, SourceError> {
        self.replay_calls.fetch_add(1, Ordering::SeqCst);
        self.world.delay().await;
        let st = self.world.state.lock().unwrap();
        let mut events = Vec::new();
        // next_from 是扫描到的位置：凑满一页就停在那条事件，否则扫到 to。
        let mut next_from = to;
        for (e, _) in &st.events[from as usize..to as usize] {
            if keys.iter().any(|k| e.routes.iter().any(|r| k.covers(r))) {
                events.push(Arc::clone(e));
                if events.len() == limit {
                    next_from = e.seq();
                    break;
                }
            }
        }
        Ok(ReplayPage { events, next_from })
    }

    async fn snapshot(
        &self,
        scopes: &[pb::SubscriptionScope],
    ) -> Result<(pb::Snapshot, u64), SourceError> {
        self.snapshot_calls.fetch_add(1, Ordering::SeqCst);
        self.world.delay().await;
        let s = self.world.head();
        let tear = std::mem::take(&mut *self.world.snapshot_tear_once.lock().unwrap());
        let mut results: Vec<pb::QueryResult> = scopes
            .iter()
            .map(|_| pb::QueryResult { seq: s, kind: None })
            .collect();
        if tear {
            // 两部分之间夹着一次提交：后一部分看到更新的 seq。
            results.push(pb::QueryResult {
                seq: s + 1,
                kind: None,
            });
        }
        Ok((pb::Snapshot { results }, s))
    }
}

// ---------------------------------------------------------------- handler

/// 查询读完 head 之后、返回之前调用的钩子。
pub type AfterRead = Box<dyn FnMut() + Send>;

#[derive(Clone, Default)]
pub struct TestHandler {
    pub world: World,
    /// 命令到达 handler 的顺序（command_id）。
    pub commands: Arc<Mutex<Vec<String>>>,
    pub handled: Arc<AtomicUsize>,
    pub chunks: Arc<AtomicUsize>,
    pub closed: Arc<AtomicUsize>,
    pub command_delay: Arc<Mutex<Duration>>,
    pub side_query_delay: Arc<Mutex<Duration>>,
    /// 会写视图的查询：读完 head 之后、返回之前的延迟（模拟读事务里慢的查询）。
    pub view_query_delay: Arc<Mutex<Duration>>,
    /// 返回之前调用的钩子（测试在「读事务之后、结果返回之前」提交事件）。
    pub after_read: Arc<Mutex<Option<AfterRead>>>,
}

impl TestHandler {
    pub fn new(world: World) -> Self {
        Self {
            world,
            ..Default::default()
        }
    }
}

impl RequestHandler for TestHandler {
    async fn handle(&self, _ctx: &ConnectionContext, req: pb::Request) -> pb::Response {
        self.handled.fetch_add(1, Ordering::SeqCst);
        let body = match req.body {
            Some(pb::request::Body::Command(c)) => {
                let d = *self.command_delay.lock().unwrap();
                if !d.is_zero() {
                    tokio::time::sleep(d).await;
                }
                self.commands.lock().unwrap().push(c.command_id.clone());
                pb::response::Body::Accepted(pb::CommandAccepted::default())
            }
            Some(pb::request::Body::Query(q)) => {
                let kind = q.kind.expect("ipc 已校验非空");
                if iwork_proto::routing::query_lane(&kind)
                    == iwork_proto::routing::QueryLane::SideQuery
                {
                    let d = *self.side_query_delay.lock().unwrap();
                    tokio::time::sleep(d).await;
                    pb::response::Body::Result(pb::QueryResult {
                        seq: self.world.head(),
                        kind: None,
                    })
                } else if let pb::query::Kind::GetConversation(g) = &kind
                    && g.conversation_id == "missing"
                {
                    pb::response::Body::Error(pb::Error {
                        code: pb::ErrorCode::NotFound.into(),
                        message: "没有这个对话".into(),
                        ..Default::default()
                    })
                } else {
                    // 「读事务」：读到的 head 就是 S。
                    let s = self.world.head();
                    self.world.delay().await;
                    if let Some(f) = self.after_read.lock().unwrap().as_mut() {
                        f();
                    }
                    let d = *self.view_query_delay.lock().unwrap();
                    if !d.is_zero() {
                        tokio::time::sleep(d).await;
                    }
                    pb::response::Body::Result(pb::QueryResult { seq: s, kind: None })
                }
            }
            _ => unreachable!("订阅不会到 handler"),
        };
        pb::Response {
            request_id: req.request_id,
            body: Some(body),
        }
    }

    async fn chunk(&self, _ctx: &ConnectionContext, chunk: pb::Chunk) -> Result<(), ChunkRefused> {
        self.chunks.fetch_add(1, Ordering::SeqCst);
        if chunk.upload_id == "refuse" {
            return Err(ChunkRefused(pb::Error {
                code: pb::ErrorCode::PayloadTooLarge.into(),
                message: "上传超过 20MB".into(),
                ..Default::default()
            }));
        }
        Ok(())
    }

    async fn connection_closed(&self, _ctx: &ConnectionContext) {
        self.closed.fetch_add(1, Ordering::SeqCst);
    }
}

// ---------------------------------------------------------------- 启动

pub struct Harness {
    pub world: World,
    pub handler: TestHandler,
    pub source: WorldSource,
    pub publisher: Publisher,
    pub connector: MemoryConnector,
    pub endpoint: Endpoint,
    pub shutdown: CancellationToken,
    pub server: tokio::task::JoinHandle<()>,
}

pub fn start(cfg: ServerConfig) -> Harness {
    start_with(
        cfg,
        World::default(),
        server_own(),
        client_peer(),
        Arc::new(|_| server_peer()),
    )
}

pub fn start_with(
    cfg: ServerConfig,
    world: World,
    server_identity: SelfIdentity,
    client_seen_by_server: PeerIdentity,
    server_seen_by_client: Arc<memory::ServerIdentityFn>,
) -> Harness {
    let handler = TestHandler::new(world.clone());
    let source = WorldSource::new(world.clone());
    let (server, publisher) = Server::new(cfg, server_identity, handler.clone(), source.clone());
    let (listener, connector) =
        memory::endpoint("test", client_seen_by_server, server_seen_by_client);
    let endpoint = Endpoint::Memory("test".into());
    let shutdown = CancellationToken::new();
    let server = tokio::spawn(server.serve(listener, shutdown.clone()));
    Harness {
        world,
        handler,
        source,
        publisher,
        connector,
        endpoint,
        shutdown,
        server,
    }
}

impl Harness {
    pub async fn connect(&self) -> Result<(Client, Incoming), ConnectError> {
        Client::connect(&self.connector, &self.endpoint, &client_own(), client_cfg()).await
    }

    pub async fn connect_ok(&self) -> (Client, Incoming) {
        self.connect().await.expect("应能连上")
    }

    pub fn publish(&self, op: Op) -> u64 {
        self.world.commit_publish(&self.publisher, op)
    }

    pub fn upsert(&self, entity: &str, value: u64) -> u64 {
        self.publish(Op::Upsert {
            entity: entity.into(),
            value,
        })
    }
}

/// 从 `Incoming` 取下一项，带超时（测试不许无限等）。
pub async fn next_item(incoming: &mut Incoming) -> IncomingItem {
    tokio::time::timeout(Duration::from_secs(20), incoming.next())
        .await
        .expect("等 Incoming 超时")
        .expect("Incoming 不该在 Closed 之前结束")
}

/// 收下一条持久事件的 seq（跳过瞬时事件）；碰到别的项报错。
pub async fn next_persisted(incoming: &mut Incoming) -> u64 {
    loop {
        match next_item(incoming).await {
            IncomingItem::Event(pb::Event {
                body: Some(pb::event::Body::Persisted(p)),
                ..
            }) => return p.seq,
            IncomingItem::Event(_) => continue,
            other => panic!("期望持久事件，得到 {other:?}"),
        }
    }
}

pub fn persisted_seq(item: &IncomingItem) -> Option<u64> {
    match item {
        IncomingItem::Event(pb::Event {
            body: Some(pb::event::Body::Persisted(p)),
            ..
        }) => Some(p.seq),
        _ => None,
    }
}

pub fn get_conversation(id: &str) -> pb::Query {
    pb::Query {
        kind: Some(pb::query::Kind::GetConversation(pb::GetConversation {
            conversation_id: id.into(),
            ..Default::default()
        })),
    }
}

pub fn command(id: &str) -> pb::request::Body {
    pb::request::Body::Command(pb::Command {
        command_id: id.into(),
        kind: None,
    })
}

// ---------------------------------------------------------------- 审计捕获

/// 捕获 tracing 事件的字段（只在当前线程生效：测试用 current_thread 运行时）。
#[derive(Clone, Default)]
pub struct Captured(pub Arc<Mutex<Vec<BTreeMap<String, String>>>>);

impl Captured {
    pub fn install() -> (Captured, tracing::subscriber::DefaultGuard) {
        let c = Captured::default();
        let guard = tracing::subscriber::set_default(CaptureSubscriber(c.clone()));
        (c, guard)
    }

    pub fn events(&self, name: &str) -> Vec<BTreeMap<String, String>> {
        self.0
            .lock()
            .unwrap()
            .iter()
            .filter(|e| e.get("event").map(String::as_str) == Some(name))
            .cloned()
            .collect()
    }

    pub fn all(&self) -> Vec<BTreeMap<String, String>> {
        self.0.lock().unwrap().clone()
    }
}

struct CaptureSubscriber(Captured);

struct Visitor<'a>(&'a mut BTreeMap<String, String>);

impl tracing::field::Visit for Visitor<'_> {
    fn record_debug(&mut self, field: &tracing::field::Field, value: &dyn std::fmt::Debug) {
        self.0.insert(
            field.name().to_owned(),
            format!("{value:?}").trim_matches('"').to_owned(),
        );
    }
    fn record_str(&mut self, field: &tracing::field::Field, value: &str) {
        self.0.insert(field.name().to_owned(), value.to_owned());
    }
}

impl tracing::Subscriber for CaptureSubscriber {
    fn enabled(&self, _: &tracing::Metadata<'_>) -> bool {
        true
    }
    fn new_span(&self, _: &tracing::span::Attributes<'_>) -> tracing::span::Id {
        tracing::span::Id::from_u64(1)
    }
    fn record(&self, _: &tracing::span::Id, _: &tracing::span::Record<'_>) {}
    fn record_follows_from(&self, _: &tracing::span::Id, _: &tracing::span::Id) {}
    fn event(&self, event: &tracing::Event<'_>) {
        let mut fields = BTreeMap::new();
        fields.insert("target".to_owned(), event.metadata().target().to_owned());
        fields.insert("level".to_owned(), event.metadata().level().to_string());
        event.record(&mut Visitor(&mut fields));
        self.0.0.lock().unwrap().push(fields);
    }
    fn enter(&self, _: &tracing::span::Id) {}
    fn exit(&self, _: &tracing::span::Id) {}
}

/// 世界里实体集合 `members` 是否完全一致（视图对拍用）。
pub fn visible(
    p: &BTreeMap<String, u64>,
    scopes: &[pb::SubscriptionScope],
) -> BTreeMap<String, u64> {
    p.iter()
        .filter(|(e, _)| covers(scopes, e))
        .map(|(e, v)| (e.clone(), *v))
        .collect()
}

pub fn set<T: Ord + Clone>(v: &[T]) -> BTreeSet<T> {
    v.iter().cloned().collect()
}
