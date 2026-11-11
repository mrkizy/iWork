//! 外壳：把日志与表接到 `iwork-ipc` 的真服务端上（Unix socket）。
//!
//! 信任策略：开 `dev` feature 时是 DevTrust（同用户照查、跳过签名，bin 接 Tauri 客户端要它）；不开时是发布同款的
//! RequireSigner，身份用 [`self_trusting_identity`]——只认与自己同一个可执行文件的对端，进程内的测试够用。

use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use iwork_ipc::LocalListener;
use iwork_ipc::peer::{ImageId, Manifest, SelfIdentity, SignerIdentity, TrustPolicy};
use iwork_ipc::server::{
    ChunkRefused, ConnectionContext, EventSource, Published, Publisher, ReplayPage, RequestHandler,
    RoutedEvent, Server, ServerConfig, SourceError,
};
use iwork_ipc::transport::Endpoint;
use iwork_proto::pb;
use iwork_proto::routing::StreamKey;
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;

use crate::scenario::{self, Step, UnknownScenario};
use crate::{State, Tables, commands, envelope_time, fixtures, routes};

/// 事件的动作主体（信封 actor）。
const ACTOR_USER: &str = "user";
const ACTOR_SYSTEM: &str = "system";

struct Shared {
    state: Mutex<State>,
    /// 当前这一代服务端的发布端；重启（断开全部连接）时换新的。
    publisher: Mutex<Option<Publisher>>,
    auto_reply: AtomicBool,
    /// 场景里等待的倍数（1 = 照原样）：把同样形状的流量摊长（真实 WebView 的 1 分钟用例）。
    pace: std::sync::atomic::AtomicU32,
    /// 只给测试用：命中的查询一律回错误（模拟查询受理出错）。
    failing_queries: Mutex<Option<QueryFilter>>,
    /// 重连时最多重放多少个 seq，再多给快照（ipc `ServerConfig::replay_limit`）；下一次重启监听时生效。
    replay_limit: std::sync::atomic::AtomicU64,
}

/// 哪些查询要回错误。
pub type QueryFilter = Box<dyn Fn(&pb::Query) -> bool + Send + Sync>;

impl Shared {
    fn lock(&self) -> std::sync::MutexGuard<'_, State> {
        self.state.lock().expect("假后台状态锁中毒")
    }

    /// 追加一条持久事件：分配 seq、算路由（在折叠之前：删除事件折叠后就查不到归属）、折叠、入日志、发布。
    /// 全程持有状态锁，所以发布顺序就是 seq 顺序（Publisher 要求递增）。
    fn append_locked(&self, st: &mut State, event: pb::DomainEvent, actor: &str) -> u64 {
        let seq = st.head() + 1;
        let at = envelope_time(seq);
        let inner = event
            .event
            .as_ref()
            .expect("假后台只追加带载荷的事件（DomainEvent.event 为空）");
        let routes = routes::routes(&st.tables, inner);
        st.tables.fold(seq, &at, inner);
        let routed = Arc::new(RoutedEvent::new(
            pb::PersistedEvent {
                seq,
                at: Some(at),
                actor: actor.to_owned(),
                event: Some(event),
            },
            routes,
        ));
        st.log.push(Arc::clone(&routed));
        if let Some(p) = self.publisher.lock().expect("发布端锁中毒").as_ref() {
            p.publish(Published::Persisted(routed));
        }
        seq
    }

    fn append(&self, event: pb::DomainEvent, actor: &str) -> u64 {
        let mut st = self.lock();
        self.append_locked(&mut st, event, actor)
    }

    fn transient(&self, event: pb::TransientEvent) {
        let st = self.lock();
        let routes = routes::transient_routes(&st.tables, &event);
        if let Some(p) = self.publisher.lock().expect("发布端锁中毒").as_ref() {
            p.publish(Published::Transient { event, routes });
        }
    }

    fn command(self: &Arc<Self>, command: pb::Command) -> Result<pb::CommandAccepted, pb::Error> {
        let kind = command.kind.as_ref().ok_or_else(|| pb::Error {
            code: pb::ErrorCode::InvalidArgument.into(),
            message: "命令为空".into(),
            retryable: false,
            details: Default::default(),
        })?;
        let mut st = self.lock();
        if let Some(prev) = st.accepted.get(&command.command_id) {
            return Ok(pb::CommandAccepted {
                replayed: true,
                ..*prev
            });
        }
        let head = st.head();
        let decision = {
            let State {
                tables, next_id, ..
            } = &mut *st;
            let mut fresh = |prefix: &str| {
                *next_id += 1;
                format!("{prefix}-gen-{:06}", *next_id)
            };
            commands::decide(tables, kind, head, &mut fresh)?
        };
        let mut seqs = decision
            .events
            .into_iter()
            .map(|e| self.append_locked(&mut st, e, ACTOR_USER))
            .collect::<Vec<_>>()
            .into_iter();
        let first = seqs.next();
        let last = seqs.last().or(first);
        let accepted = pb::CommandAccepted {
            first_seq: first.unwrap_or(head),
            last_seq: last.unwrap_or(head),
            replayed: false,
        };
        st.accepted.insert(command.command_id.clone(), accepted);
        drop(st);
        if let Some(conv) = decision.reply_in
            && self.auto_reply.load(Ordering::Relaxed)
        {
            let me = Arc::clone(self);
            tokio::spawn(async move { me.scripted_reply(conv).await });
        }
        Ok(accepted)
    }

    /// 脚本化回合：排队 → 运行 → 一条流式助手消息 → 完成。只为开着看（dev:fake）时能对话。
    async fn scripted_reply(self: Arc<Self>, conversation_id: String) {
        let (turn, msg, assistant, name) = {
            let mut st = self.lock();
            let assistant = reply_assistant(&st.tables, &conversation_id);
            let name = st
                .tables
                .assistants
                .get(&assistant)
                .and_then(|a| a.config.as_ref())
                .map(|c| c.name.clone())
                .unwrap_or_else(|| "iWork".into());
            (st.fresh_id("turn"), st.fresh_id("msg"), assistant, name)
        };
        use pb::TurnState as T;
        self.append(
            fixtures::turn_queued(&turn, &conversation_id, &assistant),
            ACTOR_SYSTEM,
        );
        self.append(
            fixtures::turn_state(&turn, T::Queued, T::Running, None),
            ACTOR_SYSTEM,
        );
        self.append(
            fixtures::assistant_message(&msg, &conversation_id, &turn, &assistant, &name),
            &assistant,
        );
        let reply = "收到。这是假后台的脚本化回复，用来看界面。";
        let mut offset = 0u64;
        for piece in reply.split_inclusive('。') {
            tokio::time::sleep(Duration::from_millis(30)).await;
            self.transient(fixtures::delta(&msg, offset, piece));
            offset += piece.len() as u64;
        }
        self.append(
            fixtures::message_completed(&conversation_id, &msg, reply),
            &assistant,
        );
        self.append(
            fixtures::turn_finished(&turn, T::Completed, false, None),
            ACTOR_SYSTEM,
        );
    }
}

/// 自动回复用哪个助手：私聊用它自己；项目对话用内置 iWork（没有就用任意一个）。
fn reply_assistant(tables: &Tables, conversation_id: &str) -> String {
    let dm = tables
        .conversations
        .get(conversation_id)
        .and_then(|r| match &r.conversation.kind {
            Some(pb::conversation::Kind::AssistantDm(d)) => Some(d.assistant_id.clone()),
            _ => None,
        });
    dm.or_else(|| {
        tables
            .assistants
            .values()
            .find(|a| a.builtin)
            .map(|a| a.id.clone())
    })
    .or_else(|| tables.assistants.keys().next().cloned())
    .unwrap_or_else(|| fixtures::IWORK.into())
}

struct Handler(Arc<Shared>);

impl RequestHandler for Handler {
    async fn handle(&self, _: &ConnectionContext, req: pb::Request) -> pb::Response {
        use pb::request::Body;
        use pb::response::Body as Out;
        let body = match req.body {
            Some(Body::Command(c)) => match self.0.command(c) {
                Ok(a) => Out::Accepted(a),
                Err(e) => Out::Error(e),
            },
            Some(Body::Query(q))
                if self
                    .0
                    .failing_queries
                    .lock()
                    .expect("查询故障表锁中毒")
                    .as_ref()
                    .is_some_and(|f| f(&q)) =>
            {
                Out::Error(pb::Error {
                    code: pb::ErrorCode::Internal.into(),
                    message: "假后台：按测试设定让这条查询出错".into(),
                    retryable: true,
                    details: Default::default(),
                })
            }
            Some(Body::Query(q)) => {
                let st = self.0.lock();
                match st.tables.query(st.head(), &q) {
                    Ok(r) => Out::Result(r),
                    Err(e) => Out::Error(e),
                }
            }
            Some(Body::Subscribe(_) | Body::Unsubscribe(_)) | None => Out::Error(pb::Error {
                code: pb::ErrorCode::InvalidArgument.into(),
                message: "这类请求不该到 handler".into(),
                retryable: false,
                details: Default::default(),
            }),
        };
        pb::Response {
            request_id: req.request_id,
            body: Some(body),
        }
    }

    async fn chunk(&self, _: &ConnectionContext, _: pb::Chunk) -> Result<(), ChunkRefused> {
        Ok(())
    }

    async fn connection_closed(&self, _: &ConnectionContext) {}
}

struct Source(Arc<Shared>);

impl EventSource for Source {
    async fn head_seq(&self) -> Result<u64, SourceError> {
        Ok(self.0.lock().head())
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
        let st = self.0.lock();
        let end = (to as usize).min(st.log.len());
        let mut events = Vec::new();
        for e in st.log.get(from as usize..end).unwrap_or(&[]) {
            if e.routes.iter().any(|r| keys.iter().any(|k| k.covers(r))) {
                events.push(Arc::clone(e));
                if events.len() == limit {
                    return Ok(ReplayPage {
                        events,
                        next_from: e.seq(),
                    });
                }
            }
        }
        Ok(ReplayPage {
            events,
            next_from: to,
        })
    }

    async fn snapshot(
        &self,
        scopes: &[pb::SubscriptionScope],
    ) -> Result<(pb::Snapshot, u64), SourceError> {
        let st = self.0.lock();
        let head = st.head();
        let mut results = Vec::new();
        for q in scopes
            .iter()
            .flat_map(iwork_app::subscription::snapshot_queries)
        {
            match st.tables.query(head, &q) {
                Ok(r) => results.push(r),
                // 订阅了已删除的对话：快照里没有这一项（T10 怎么处理以真后台为准，见报告）。
                Err(e) if e.code == i32::from(pb::ErrorCode::NotFound) => {}
                Err(e) => return Err(SourceError(e.message)),
            }
        }
        Ok((pb::Snapshot { results }, head))
    }
}

struct Running {
    token: CancellationToken,
    task: JoinHandle<()>,
}

/// 假后台。见 crate 文档。
pub struct FakeDaemon {
    shared: Arc<Shared>,
    path: PathBuf,
    own: SelfIdentity,
    running: tokio::sync::Mutex<Option<Running>>,
}

impl FakeDaemon {
    /// 在 `socket_path` 上起服务。必须在 tokio 运行时里调用。
    pub async fn start(socket_path: PathBuf) -> std::io::Result<FakeDaemon> {
        let own = self_trusting_identity()?;
        let shared = Arc::new(Shared {
            state: Mutex::new(State::default()),
            publisher: Mutex::new(None),
            auto_reply: AtomicBool::new(false),
            pace: std::sync::atomic::AtomicU32::new(1),
            failing_queries: Mutex::new(None),
            replay_limit: std::sync::atomic::AtomicU64::new(
                ServerConfig::new("iwork-fake-daemon").replay_limit,
            ),
        });
        let running = spawn_server(&shared, &socket_path, own.clone(), NORMAL_TRUST)?;
        Ok(FakeDaemon {
            shared,
            path: socket_path,
            own,
            running: tokio::sync::Mutex::new(Some(running)),
        })
    }

    pub fn endpoint(&self) -> Endpoint {
        Endpoint::Unix(self.path.clone())
    }

    /// 追加一条持久事件，返回它的 seq。
    pub fn append(&self, event: pb::DomainEvent) -> u64 {
        self.shared.append(event, ACTOR_SYSTEM)
    }

    /// 发一条流式增量（不落日志、不带 seq）。
    pub fn transient(&self, event: pb::TransientEvent) {
        self.shared.transient(event);
    }

    /// 回答一个查询，`seq` = 当前 head。查询本身错了（对话不存在等）就 panic：只给测试与场景用，
    /// 要错误值的用 [`FakeDaemon::try_query`]。
    pub fn query(&self, q: &pb::Query) -> pb::QueryResult {
        self.try_query(q)
            .unwrap_or_else(|e| panic!("假后台查询失败：{}（{q:?}）", e.message))
    }

    pub fn try_query(&self, q: &pb::Query) -> Result<pb::QueryResult, pb::Error> {
        let st = self.shared.lock();
        st.tables.query(st.head(), q)
    }

    pub fn head(&self) -> u64 {
        self.shared.lock().head()
    }

    /// 只给测试场景用：直接改折叠表（不产生事件）。
    pub fn mutate_tables(&self, f: impl FnOnce(&mut Tables)) {
        f(&mut self.shared.lock().tables);
    }

    /// 只给测试用：之后命中 `f` 的查询一律回 INTERNAL 错误；`None` 恢复正常。
    pub fn fail_queries(&self, f: Option<QueryFilter>) {
        *self
            .shared
            .failing_queries
            .lock()
            .expect("查询故障表锁中毒") = f;
    }

    /// SendMessage 之后要不要自动回一个脚本化回合（默认关）。
    /// 场景里的每一次等待乘上这个倍数（至少 1）。
    pub fn set_pace(&self, factor: u32) {
        self.shared.pace.store(factor.max(1), Ordering::Relaxed);
    }

    pub fn set_auto_reply(&self, on: bool) {
        self.shared.auto_reply.store(on, Ordering::Relaxed);
    }

    /// 关掉全部连接（模拟后台重启）：停掉这一代服务端，日志与表保留，在同一个 socket 上起新的一代。
    pub async fn disconnect_all(&self) {
        self.restart(NORMAL_TRUST, self.own.clone()).await;
    }

    /// 换身份模拟（方案 S14 第 2 条）：`on` 时服务端换成一个无证书、清单里没有任何真实映像的身份，并按
    /// `RequireSigner` 判定——真的 `judge_peer` 拒绝客户端，握手前关连接，客户端看到 `ClosedBeforeWelcome`。
    /// `off` 恢复正常身份。两次切换都断开现有连接（就像后台换了版本重启）。
    pub async fn set_rejecting(&self, on: bool) {
        if on {
            let own = rejecting_identity(&self.own);
            self.restart(TrustPolicy::RequireSigner, own).await;
        } else {
            self.restart(NORMAL_TRUST, self.own.clone()).await;
        }
    }

    /// 停止监听（客户端连不上）；`disconnect_all` 恢复。
    pub async fn stop_listening(&self) {
        if let Some(r) = self.running.lock().await.take() {
            r.token.cancel();
            let _ = r.task.await;
        }
    }

    async fn restart(&self, trust: TrustPolicy, own: SelfIdentity) {
        let mut running = self.running.lock().await;
        if let Some(r) = running.take() {
            r.token.cancel();
            let _ = r.task.await;
        }
        let next = spawn_server(&self.shared, &self.path, own, trust)
            .unwrap_or_else(|e| panic!("假后台在 {} 上重新监听失败：{e}", self.path.display()));
        *running = Some(next);
    }

    pub async fn run_scenario(&self, name: &str) -> Result<(), UnknownScenario> {
        let sc = scenario::get(name).ok_or_else(|| UnknownScenario(name.to_owned()))?;
        self.run_steps(sc.steps()).await;
        Ok(())
    }

    /// 按顺序跑一串步骤（场景的一段，见 `Scenario::split_at_focus`）。
    pub async fn run_steps(&self, steps: Vec<Step>) {
        for step in steps {
            match step {
                Step::Append(e) => {
                    self.append(*e);
                }
                Step::Transient(t) => self.transient(t),
                Step::Sleep(d) => {
                    tokio::time::sleep(d * self.shared.pace.load(Ordering::Relaxed)).await;
                }
                Step::DisconnectAll => self.disconnect_all().await,
                Step::DisconnectForSnapshot => {
                    self.shared.replay_limit.store(0, Ordering::Relaxed);
                    self.disconnect_all().await;
                }
                Step::StopListening => self.stop_listening().await,
                Step::OpenHere => {}
                Step::Mutate(f) => self.mutate_tables(f),
            }
        }
    }

    pub async fn shutdown(self) {
        if let Some(r) = self.running.lock().await.take() {
            r.token.cancel();
            let _ = r.task.await;
        }
        let _ = std::fs::remove_file(&self.path);
    }
}

fn spawn_server(
    shared: &Arc<Shared>,
    path: &std::path::Path,
    own: SelfIdentity,
    trust: TrustPolicy,
) -> std::io::Result<Running> {
    let listener = LocalListener::bind(path)?;
    let mut cfg = ServerConfig::new("iwork-fake-daemon");
    cfg.trust = trust;
    cfg.replay_limit = shared.replay_limit.load(Ordering::Relaxed);
    cfg.shutdown_flush = Duration::from_millis(100);
    let (server, publisher) = Server::new(
        cfg,
        own,
        Handler(Arc::clone(shared)),
        Source(Arc::clone(shared)),
    );
    // 先换上新的发布端再开始服务：之后追加的事件都进新的一代（serve 开始时按日志 head 设水位基线）。
    *shared.publisher.lock().expect("发布端锁中毒") = Some(publisher);
    let token = CancellationToken::new();
    let task = tokio::spawn(server.serve(listener, token.clone()));
    Ok(Running { token, task })
}

/// 正常时的信任策略（见文件头）。
#[cfg(feature = "dev")]
const NORMAL_TRUST: TrustPolicy = TrustPolicy::DevTrust;
#[cfg(not(feature = "dev"))]
const NORMAL_TRUST: TrustPolicy = TrustPolicy::RequireSigner;

/// 本进程的真实身份，无证书时补一份只登记自己映像的清单：按发布同款的 `judge_peer` 判定，只有同一个可执行文件
/// 的对端能通过。进程内测试里客户端与假后台就是同一个可执行文件，两端都用它就能连上，不需要 DevTrust。
pub fn self_trusting_identity() -> std::io::Result<SelfIdentity> {
    let mut own = SelfIdentity::detect().map_err(|e| std::io::Error::other(e.to_string()))?;
    if let SignerIdentity::Image(id) = &own.signer {
        let manifest = Manifest::parse(&format!(
            r#"{{"manifest_version":1,"app_version":"0.0.0-fake","binaries":{{"self":["{id}"]}}}}"#
        ))
        .map_err(|e| std::io::Error::other(e.to_string()))?;
        own.manifest = Some(manifest);
    }
    Ok(own)
}

/// 一个「别的安装」的身份：无证书、安装目录不存在、清单只登记一个不存在的映像。任何真实客户端都不在清单里。
fn rejecting_identity(real: &SelfIdentity) -> SelfIdentity {
    let foreign = ImageId::Sha256([0xEE; 32]);
    let manifest = Manifest::parse(&format!(
        r#"{{"manifest_version":1,"app_version":"0.0.0-fake","binaries":{{"iwork":["{foreign}"]}}}}"#
    ))
    .expect("内置的拒绝用清单能解析");
    SelfIdentity {
        user: real.user.clone(),
        signer: SignerIdentity::Image(foreign),
        exe_dir: PathBuf::from("/nonexistent/iwork-fake-daemon-rejecting"),
        manifest: Some(manifest),
    }
}
