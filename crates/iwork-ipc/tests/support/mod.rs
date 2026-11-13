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

/// 实体的路由键。`conversation:c1`、`project:p1` 只有自己一个键；`turn:t1@c2` 模拟带会话 ID 的回合事件，
/// 有两个键：`turn:t1` 与 `conversation:c2`（方案 2.8：回合、审批事件同时落在会话键上）。
pub fn routes_of(entity: &str) -> Vec<StreamKey> {
    let (kind, id) = entity.split_once(':').expect("实体名形如 kind:id");
    match kind {
        "conversation" => vec![StreamKey::one("conversation", id)],
        "project" => vec![StreamKey::one("project", id)],
        "turn" => {
            let (turn, conv) = id.split_once('@').expect("回合实体形如 turn:t1@c2");
            vec![
                StreamKey::one("turn", turn),
                StreamKey::one("conversation", conv),
            ]
        }
        other => panic!("测试世界没有 {other} 这类实体"),
    }
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
    let keys: Vec<StreamKey> = scopes.iter().flat_map(scope_keys).collect();
    keys_cover(&keys, entity)
}

pub fn keys_cover(keys: &[StreamKey], entity: &str) -> bool {
    let routes = routes_of(entity);
    keys.iter().any(|k| routes.iter().any(|r| k.covers(r)))
}

// ---------------------------------------------------------------- 线上载荷
//
// 夹具把真实数据放进帧里，客户端模型只从收到的帧取数据（测试的独立期望来自世界日志，不喂给模型）：
// - 持久事件：操作编码在 `PersistedEvent.actor` 里（`U|实体|值|填充`、`D|实体||填充`）；
// - 快照与会写视图的查询结果：视图里的实体放在 `ListConversationsResult` 里（`id` = 实体，`last_read_seq` = 值）。

pub fn encode_op(op: &Op, pad: usize) -> String {
    let pad = "u".repeat(pad);
    match op {
        Op::Upsert { entity, value } => format!("U|{entity}|{value}|{pad}"),
        Op::Delete { entity } => format!("D|{entity}||{pad}"),
    }
}

pub fn decode_op(actor: &str) -> Op {
    let mut parts = actor.splitn(4, '|');
    let (tag, entity, value) = (parts.next(), parts.next(), parts.next());
    match (tag, entity, value) {
        (Some("U"), Some(e), Some(v)) => Op::Upsert {
            entity: e.into(),
            value: v.parse().expect("值是数字"),
        },
        (Some("D"), Some(e), _) => Op::Delete { entity: e.into() },
        _ => panic!("事件载荷格式不对：{actor:?}"),
    }
}

pub fn view_result(seq: u64, entities: &BTreeMap<String, u64>) -> pb::QueryResult {
    pb::QueryResult {
        seq,
        kind: Some(pb::query_result::Kind::ListConversations(
            pb::ListConversationsResult {
                conversations: entities
                    .iter()
                    .map(|(e, v)| pb::Conversation {
                        id: e.clone(),
                        last_read_seq: *v,
                        ..Default::default()
                    })
                    .collect(),
                has_more: false,
            },
        )),
    }
}

pub fn decode_view(r: &pb::QueryResult) -> BTreeMap<String, u64> {
    match &r.kind {
        Some(pb::query_result::Kind::ListConversations(l)) => l
            .conversations
            .iter()
            .map(|c| (c.id.clone(), c.last_read_seq))
            .collect(),
        _ => BTreeMap::new(),
    }
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
                actor: encode_op(&op, pad),
                event: None,
            },
            routes_of(op.entity()),
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

    /// `keys` 覆盖的实体在 seq 时刻的投影。
    pub fn view_at(&self, seq: u64, keys: &[StreamKey]) -> BTreeMap<String, u64> {
        self.projection_at(seq)
            .into_iter()
            .filter(|(e, _)| keys_cover(keys, e))
            .collect()
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
        // 每个范围一部分，内容是这个范围的视图在 s 时刻的实体（一个读事务：都按同一个 s 取）。
        let mut results: Vec<pb::QueryResult> = scopes
            .iter()
            .map(|scope| view_result(s, &self.world.view_at(s, &scope_keys(scope))))
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
    /// 正在执行的 handle 调用数。handler 任务被中止时，守卫随 future 一起丢弃，计数会回落。
    pub in_flight: Arc<AtomicUsize>,
    /// 会写视图的查询读完 head 之后执行到的次数（迟到返回的取消断言用）。
    pub reads_finished: Arc<AtomicUsize>,
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
        let _in_flight = InFlight::enter(&self.in_flight);
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
                    self.reads_finished.fetch_add(1, Ordering::SeqCst);
                    if let Some(f) = self.after_read.lock().unwrap().as_mut() {
                        f();
                    }
                    let d = *self.view_query_delay.lock().unwrap();
                    if !d.is_zero() {
                        tokio::time::sleep(d).await;
                    }
                    // GetConversation 的结果是这个对话视图在 S 时刻的实体（含带这个会话键的回合实体）；
                    // 其余列表查询这个世界里没有内容。
                    let view = match &kind {
                        pb::query::Kind::GetConversation(g) => self.world.view_at(
                            s,
                            &[StreamKey::one("conversation", g.conversation_id.clone())],
                        ),
                        _ => BTreeMap::new(),
                    };
                    pb::response::Body::Result(view_result(s, &view))
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

/// 进入时加一、离开（含 future 被丢弃）时减一。
struct InFlight(Arc<AtomicUsize>);

impl InFlight {
    fn enter(n: &Arc<AtomicUsize>) -> Self {
        n.fetch_add(1, Ordering::SeqCst);
        Self(Arc::clone(n))
    }
}

impl Drop for InFlight {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::SeqCst);
    }
}

/// 在期限内等计数变成 `want`。
pub async fn wait_count(n: &AtomicUsize, want: usize, within: Duration) -> bool {
    let deadline = tokio::time::Instant::now() + within;
    while n.load(Ordering::SeqCst) != want {
        if tokio::time::Instant::now() >= deadline {
            return false;
        }
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    true
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
        let item = next_item(incoming).await;
        if let Some(seq) = persisted_seq(&item) {
            return seq;
        }
        assert!(
            matches!(item, IncomingItem::Event(_)),
            "期望持久事件，得到 {item:?}"
        );
    }
}

pub fn persisted_seq(item: &IncomingItem) -> Option<u64> {
    if let IncomingItem::Event(e) = item
        && let Some(pb::event::Body::Persisted(p)) = &e.body
    {
        Some(p.seq)
    } else {
        None
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
        keep_callsites_interested();
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

/// 装一个进程级的全局订阅者 [`KeepInterest`]，整个测试程序只装一次。
///
/// tracing 按「第一次命中某个 callsite 时有哪些订阅者」算这个 callsite 的 interest 并缓存。
/// 只有一个分发器时，它用命中线程自己的默认订阅者来算：同一测试程序里别的测试线程没装订阅者，
/// 先命中审计 callsite 就会把它缓存成 never，之后本线程的 `Captured` 再也收不到这条审计
/// （必红的复现见 `tests/audit_capture.rs`）。
/// 有了全局的 `KeepInterest`，没装订阅者的线程算出来的也是 sometimes，事件照常问到各线程自己的订阅者。
fn keep_callsites_interested() {
    static ONCE: std::sync::Once = std::sync::Once::new();
    ONCE.call_once(|| {
        tracing::subscriber::set_global_default(KeepInterest)
            .expect("iwork-ipc 的测试程序里只有这里装全局订阅者");
    });
}

/// 对每个 callsite 都答 sometimes、自己一条事件都不收的订阅者。
struct KeepInterest;

impl tracing::Subscriber for KeepInterest {
    fn register_callsite(
        &self,
        _: &'static tracing::Metadata<'static>,
    ) -> tracing::subscriber::Interest {
        tracing::subscriber::Interest::sometimes()
    }
    fn enabled(&self, _: &tracing::Metadata<'_>) -> bool {
        false
    }
    fn new_span(&self, _: &tracing::span::Attributes<'_>) -> tracing::span::Id {
        tracing::span::Id::from_u64(1)
    }
    fn record(&self, _: &tracing::span::Id, _: &tracing::span::Record<'_>) {}
    fn record_follows_from(&self, _: &tracing::span::Id, _: &tracing::span::Id) {}
    fn event(&self, _: &tracing::Event<'_>) {}
    fn enter(&self, _: &tracing::span::Id) {}
    fn exit(&self, _: &tracing::span::Id) {}
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
