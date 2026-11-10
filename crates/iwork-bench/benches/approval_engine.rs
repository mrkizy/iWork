//! 审批引擎的热路径与启动（T07 方案 6.10.12）：真实 `SqliteStore` + 真实引擎。
//!
//! 五组：① `on_permission_allow_e2e`：一条 `git status` 走完整条放行路径（追平 + 求值 + 审计提交）；
//! ② `catch_up_idle`：没有新事件时一次追平；③ `catch_up_burst`：积了 200 条回合事件后的一次追平；
//! ④ `engine_fold_startup`：1 万 / 10 万条混合事件从头折一遍（报回合表条数）；⑤ 16 个对话并发放行的吞吐。
//! macOS 上 ①⑤ 各跑 `fullfsync` 开、关两组。前后对比由报告里的交替多轮给出（旧引擎的 API 不同，另有一份对照副本）。
//!
//! 环境变量：`IWORK_BENCH_ENGINE_N`（① 的次数，默认 2000）、`IWORK_BENCH_FOLD_MAX`（④ 的最大事件数，默认 100000）。

use std::collections::BTreeSet;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use iwork_app::codec::ProtoEventCodec;
use iwork_core::approval::engine::{ApprovalEngine, EnginePorts, SessionKeys};
use iwork_core::approval::env::{EnvPaths, Platform, ScopeKeys};
use iwork_core::approval::session::{rule_paths_for, snapshot_for, tool_set_for};
use iwork_domain::action::{CanonicalPath, ToolRef};
use iwork_domain::approval_mode::{ApprovalMode, ProjectDefaultMode};
use iwork_domain::assistant::{Assistant, AssistantConfig, AssistantVersion, Avatar};
use iwork_domain::common::{Actor, ModelId, ModelSelection, Timestamp, ToolGroups};
use iwork_domain::conversation::{Conversation, ConversationKind, ConversationState};
use iwork_domain::event::*;
use iwork_domain::id::{
    AssistantId, CommandId, ConversationId, ProjectId, RawUlid, RuntimeId, Seq, TurnId,
};
use iwork_domain::project::{AiReviewSettings, PathStatus, Project, ProjectSettings, VcsKind};
use iwork_domain::turn::{Turn, TurnKind, TurnState};
use iwork_host::fs::OsPathResolver;
use iwork_ports::agent::{
    AgentError, AgentPort, AgentSession, ContentBlock, PermissionHandler, PermissionRequest,
    RawToolInput, RuntimeCapabilities, SessionSpec, UpdateStream,
};
use iwork_ports::clock::{Clock, IdGenerator, MonotonicInstant, Timer};
use iwork_ports::store::{CommitRequest, StoragePort};
use iwork_store::{DataDir, OpenOptions, SqliteProjector, SqliteStore, StoreConfig};

// ---------- 端口 ----------

#[derive(Clone, Copy)]
struct SysClock(Instant);
impl Clock for SysClock {
    fn now(&self) -> Timestamp {
        Timestamp(1_794_268_800_000 + self.0.elapsed().as_millis() as i64)
    }
    fn monotonic(&self) -> MonotonicInstant {
        MonotonicInstant(self.0.elapsed().as_nanos() as u64)
    }
}

/// 单调递增的 ID（高位放时间，满足存储的单调契约）。
struct Ids(AtomicU64);
impl IdGenerator for Ids {
    fn next(&self) -> RawUlid {
        let n = self.0.fetch_add(1, Ordering::SeqCst);
        RawUlid::from_u128((1u128 << 100) + u128::from(n))
    }
}

struct NoTimer;
impl Timer for NoTimer {
    async fn sleep(&self, d: Duration) {
        tokio::time::sleep(d).await;
    }
}

/// 基准里不跑审查：任何调用都报错。
struct NoReviewer;
struct NoSession;
struct NoUpdates;
impl AgentPort for NoReviewer {
    type Session = NoSession;
    async fn capabilities(&self) -> Result<RuntimeCapabilities, AgentError> {
        unimplemented!()
    }
    async fn open_session<H: PermissionHandler + 'static>(
        &self,
        _spec: SessionSpec,
        _h: H,
    ) -> Result<NoSession, AgentError> {
        Err(AgentError::Protocol {
            message: "基准不跑审查".into(),
        })
    }
}
impl AgentSession for NoSession {
    type Updates = NoUpdates;
    fn runtime_session_id(&self) -> Option<&str> {
        None
    }
    async fn prompt(&self, _b: Vec<ContentBlock>) -> Result<NoUpdates, AgentError> {
        unimplemented!()
    }
    async fn cancel(&self) -> Result<(), AgentError> {
        Ok(())
    }
    async fn close(self) -> Result<(), AgentError> {
        Ok(())
    }
}
impl UpdateStream for NoUpdates {
    async fn next(&mut self) -> Option<iwork_ports::agent::AgentUpdate> {
        None
    }
}

type Store = SqliteStore<ProtoEventCodec>;
type Engine = ApprovalEngine<SysClock, Ids, Shared, NoReviewer, NoTimer, OsPathResolver>;

/// 引擎与基准共用同一个存储。
#[derive(Clone)]
struct Shared(Arc<Store>);

impl StoragePort for Shared {
    async fn commit(
        &self,
        req: CommitRequest,
    ) -> Result<iwork_ports::store::CommitReceipt, iwork_ports::store::StorageError> {
        self.0.commit(req).await
    }
    async fn head_seq(&self) -> Result<Seq, iwork_ports::store::StorageError> {
        self.0.head_seq().await
    }
    async fn read_events(
        &self,
        filter: iwork_ports::store::EventFilter,
        from: Seq,
        limit: usize,
    ) -> Result<Vec<iwork_ports::store::StoredEvent>, iwork_ports::store::StorageError> {
        self.0.read_events(filter, from, limit).await
    }
    async fn checkpoint_message(
        &self,
        id: iwork_domain::id::MessageId,
        offset: u64,
        text: &str,
    ) -> Result<(), iwork_ports::store::StorageError> {
        self.0.checkpoint_message(id, offset, text).await
    }
    async fn outbox_pending(
        &self,
        limit: usize,
    ) -> Result<Vec<iwork_ports::store::OutboxItem>, iwork_ports::store::StorageError> {
        self.0.outbox_pending(limit).await
    }
    async fn outbox_mark(
        &self,
        id: iwork_ports::store::OutboxId,
        state: iwork_ports::store::DeliveryState,
    ) -> Result<(), iwork_ports::store::StorageError> {
        self.0.outbox_mark(id, state).await
    }
    async fn rebuild_projections(
        &self,
    ) -> Result<iwork_ports::store::RebuildReport, iwork_ports::store::StorageError> {
        self.0.rebuild_projections().await
    }
}

// ---------- 布景 ----------

fn pid() -> ProjectId {
    ProjectId::from_raw(RawUlid::from_u128(1))
}
fn aid() -> AssistantId {
    AssistantId::from_raw(RawUlid::from_u128(2))
}
fn cid(n: u32) -> ConversationId {
    ConversationId::from_raw(RawUlid::from_u128(0x1000 + u128::from(n)))
}
fn tid(n: u32) -> TurnId {
    TurnId::from_raw(RawUlid::from_u128(0x2000_0000 + u128::from(n)))
}

fn model() -> ModelSelection {
    ModelSelection {
        model_id: ModelId("m".into()),
        effort: None,
    }
}

fn world(conversations: u32) -> Vec<DomainEvent> {
    let mut events: Vec<DomainEvent> = vec![
        ProjectAdded {
            project: Project {
                id: pid(),
                root_path: PathBuf::from("/work/p"),
                vcs_kind: VcsKind("v".into()),
                name: "p".into(),
                pinned: false,
                last_active_at: None,
                path_status: PathStatus::Ok,
                settings: ProjectSettings {
                    default_approval_mode: ProjectDefaultMode::Auto,
                    ..ProjectSettings::default()
                },
            },
        }
        .into(),
        AssistantCreated {
            assistant: Assistant {
                id: aid(),
                builtin: false,
                current: AssistantVersion {
                    version: 1,
                    config: AssistantConfig {
                        name: "a".into(),
                        avatar: Avatar {
                            initial: "A".into(),
                            color: "#123456".into(),
                        },
                        prompt: "p".into(),
                        runtime_id: RuntimeId::from_raw(RawUlid::from_u128(3)),
                        model: model(),
                        disabled_groups: BTreeSet::new(),
                    },
                    created_at: Timestamp(0),
                },
            },
        }
        .into(),
    ];
    for c in 0..conversations {
        events.push(
            ConversationCreated {
                conversation: Conversation {
                    id: cid(c),
                    kind: ConversationKind::Project { project_id: pid() },
                    state: ConversationState::Active,
                    last_read_seq: Seq(0),
                    model_override: None,
                    approval_mode: Some(ApprovalMode::Auto),
                    updated_at: Timestamp(0),
                },
            }
            .into(),
        );
        events.extend(waiting_turn(c, c));
    }
    events
}

fn turn(n: u32, c: u32) -> Turn {
    Turn {
        id: tid(n),
        conversation_id: cid(c),
        assistant_id: aid(),
        assistant_version: 1,
        runtime_id: RuntimeId::from_raw(RawUlid::from_u128(3)),
        model: model(),
        runtime_session_id: None,
        kind: TurnKind::Assistant,
        state: TurnState::Queued,
        started_at: Timestamp(0),
        ended_at: None,
    }
}

fn waiting_turn(n: u32, c: u32) -> Vec<DomainEvent> {
    vec![
        TurnQueued { turn: turn(n, c) }.into(),
        TurnStateChanged {
            turn_id: tid(n),
            from: TurnState::Queued,
            to: TurnState::WaitingApproval,
            reason: None,
        }
        .into(),
    ]
}

struct Env {
    root: PathBuf,
    ws: CanonicalPath,
    paths: EnvPaths,
    fs: OsPathResolver,
}

fn env() -> Env {
    let root = std::env::temp_dir().join(format!(
        "iwork-bench-engine-{}-{}",
        std::process::id(),
        Instant::now().elapsed().as_nanos()
    ));
    std::fs::create_dir_all(root.join("ws/src")).unwrap();
    let root = root.canonicalize().unwrap();
    let ws = CanonicalPath(root.join("ws"));
    let home = CanonicalPath(root.clone());
    let paths = EnvPaths {
        platform: Platform::MacOs,
        home: home.clone(),
        data_dir: CanonicalPath(root.join("data")),
        iwork_root: CanonicalPath(root.join("iWork")),
        workspace_root: ws.clone(),
        tmp_dirs: vec![CanonicalPath(PathBuf::from("/tmp"))],
        dm_private_dir: None,
        other_project_roots: Vec::new(),
        other_dm_dirs: Vec::new(),
    };
    let fs = OsPathResolver::new(
        home.0.clone(),
        OsPathResolver::detect_case_insensitive(&root).unwrap(),
    );
    Env {
        root,
        ws,
        paths,
        fs,
    }
}

fn keys(e: &Env, c: u32) -> SessionKeys {
    let scope = ScopeKeys {
        project: Some(pid()),
        assistant: aid(),
        conversation: cid(c),
    };
    let rule_paths = rule_paths_for(&[], &scope, &e.paths, &e.fs);
    SessionKeys {
        conversation: cid(c),
        assistant: aid(),
        runtime_id: RuntimeId::from_raw(RawUlid::from_u128(3)),
        assistant_version: 1,
        project: Some(pid()),
        turn_kind: TurnKind::Assistant,
        snapshot: Arc::new(snapshot_for(
            ApprovalMode::Auto,
            &[],
            &scope,
            &e.paths,
            Seq(0),
            &rule_paths,
        )),
        tool_set: tool_set_for(TurnKind::Assistant, &ToolGroups::new()),
        disabled_groups: ToolGroups::new(),
        cwd: e.ws.clone(),
        paths: e.paths.clone(),
        ai_review: AiReviewSettings {
            enabled: false,
            runtime_id: None,
            model: None,
            timeout_secs: 60,
        },
        review_model: model(),
    }
}

fn open(e: &Env, fullfsync: Option<bool>, clock: &Arc<SysClock>) -> Arc<Store> {
    let dir = DataDir::from_root(
        e.root
            .join(format!("data-{}", Instant::now().elapsed().as_nanos())),
    );
    dir.ensure_layout().unwrap();
    let codec = Arc::new(ProtoEventCodec);
    Arc::new(
        SqliteStore::open_with(
            &dir,
            codec.clone(),
            Arc::new(SqliteProjector::new(codec)),
            clock.clone(),
            StoreConfig {
                fullfsync,
                ..StoreConfig::default()
            },
            OpenOptions::default(),
        )
        .unwrap(),
    )
}

async fn commit(store: &Store, ids: &Ids, events: Vec<DomainEvent>) {
    store
        .commit(CommitRequest {
            command_id: CommandId::from_raw(ids.next()),
            actor: Actor("bench".into()),
            events,
            outbox: Vec::new(),
        })
        .await
        .unwrap();
}

async fn engine(
    e: &Env,
    store: &Arc<Store>,
    clock: &Arc<SysClock>,
    _ids: &Arc<Ids>,
) -> Arc<Engine> {
    // 引擎自己的 ID 段，和基准写布景用的分开
    let (engine, plan) = Engine::start(
        EnginePorts {
            clock: **clock,
            ids: Ids(AtomicU64::new(1 << 40)),
            store: Shared(store.clone()),
            reviewer: NoReviewer,
            timer: NoTimer,
            fs: e.fs.clone(),
        },
        Default::default(),
        |_, _| None,
    )
    .await
    .unwrap();
    assert!(plan.is_empty());
    engine
}

/// 一次工具调用：开始 + 结束两条回合事件。
fn tool_call(turn: TurnId, id: String) -> [DomainEvent; 2] {
    use iwork_domain::action::{Action, ActionFlags, ActionKind, ActionTarget};
    [
        ToolCallStarted {
            turn_id: turn,
            tool_call_id: ToolCallId(id.clone()),
            tool_name: "Bash".into(),
            action: Action {
                kind: ActionKind::Unclassified,
                target: ActionTarget::ToolName("Bash".into()),
                tool: ToolRef("Bash".into()),
                raw_summary: String::new(),
                flags: ActionFlags::default(),
            },
        }
        .into(),
        ToolCallFinished {
            turn_id: turn,
            tool_call_id: ToolCallId(id),
            ok: true,
            output_artifact: None,
        }
        .into(),
    ]
}

fn req(cmd: &str) -> PermissionRequest {
    PermissionRequest {
        tool: ToolRef("Bash".into()),
        raw_input: RawToolInput(serde_json::json!({ "command": cmd }).to_string()),
        bypass_sandbox: false,
        updates_before: 0,
    }
}

fn pct(samples: &mut [u64], q: f64) -> u64 {
    samples.sort_unstable();
    samples[((samples.len() as f64 - 1.0) * q) as usize]
}

fn n_env(name: &str, default: usize) -> usize {
    std::env::var(name)
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(default)
}

// ---------- 五组 ----------

async fn allow_e2e(e: &Env, fullfsync: Option<bool>, label: &str) {
    let clock = Arc::new(SysClock(Instant::now()));
    let ids = Arc::new(Ids(AtomicU64::new(0)));
    let store = open(e, fullfsync, &clock);
    commit(&store, &ids, world(1)).await;
    let engine = engine(e, &store, &clock, &ids).await;
    let k = Arc::new(keys(e, 0));
    let r = req("git status");
    let n = n_env("IWORK_BENCH_ENGINE_N", 2000);
    let mut samples = Vec::with_capacity(n);
    let t0 = Instant::now();
    for _ in 0..n {
        let t = Instant::now();
        let d = engine.on_permission(&k, tid(0), &r).await;
        assert_eq!(d.decision, iwork_ports::agent::PermissionDecision::Allow);
        samples.push(t.elapsed().as_micros() as u64);
    }
    let per_sec = n as f64 / t0.elapsed().as_secs_f64();
    println!(
        "on_permission_allow_e2e fullfsync={label:<4} 次数 {n}  P50 {:>6}µs  P95 {:>6}µs  P99 {:>6}µs  {per_sec:>8.1} 次/秒",
        pct(&mut samples, 0.5),
        pct(&mut samples, 0.95),
        pct(&mut samples, 0.99)
    );
}

async fn catch_up(e: &Env) {
    let clock = Arc::new(SysClock(Instant::now()));
    let ids = Arc::new(Ids(AtomicU64::new(0)));
    let store = open(e, Some(false), &clock);
    commit(&store, &ids, world(1)).await;
    let engine = engine(e, &store, &clock, &ids).await;
    let mut idle = Vec::new();
    for _ in 0..2000 {
        let t = Instant::now();
        engine.catch_up().await.unwrap();
        idle.push(t.elapsed().as_micros() as u64);
    }
    println!(
        "catch_up_idle  P50 {:>6}µs  P95 {:>6}µs",
        pct(&mut idle, 0.5),
        pct(&mut idle, 0.95)
    );
    let mut burst = Vec::new();
    for round in 0..50u32 {
        let events: Vec<DomainEvent> = (0..100u32)
            .flat_map(|i| tool_call(tid(0), format!("t{round}-{i}")))
            .collect();
        commit(&store, &ids, events).await;
        let t = Instant::now();
        engine.catch_up().await.unwrap();
        burst.push(t.elapsed().as_micros() as u64);
    }
    println!(
        "catch_up_burst（200 条回合事件）  P50 {:>6}µs  P95 {:>6}µs",
        pct(&mut burst, 0.5),
        pct(&mut burst, 0.95)
    );
}

/// 混合事件：按真实比例，每个回合 1 条入队 + 1 条状态 + 4 条工具调用事件 + 1 条消息 + 1 条结束。
async fn fold_startup(e: &Env, total: usize) {
    let clock = Arc::new(SysClock(Instant::now()));
    let ids = Arc::new(Ids(AtomicU64::new(0)));
    let store = open(e, Some(false), &clock);
    commit(&store, &ids, world(1)).await;
    let mut written = world(1).len();
    let mut turn_no = 1u32;
    while written < total {
        let mut batch: Vec<DomainEvent> = Vec::new();
        for _ in 0..50 {
            let t = turn_no;
            turn_no += 1;
            batch.extend(waiting_turn(t, 0));
            for i in 0..2 {
                batch.extend(tool_call(tid(t), format!("{t}-{i}")));
            }
            let mut m = iwork_domain::message::Message {
                id: iwork_domain::id::MessageId::from_raw(ids.next()),
                conversation_id: cid(0),
                author: iwork_domain::message::Author::User,
                context_project_id: Some(pid()),
                status: iwork_domain::message::MessageStatus::Sent,
                body: iwork_domain::message::MessageBody::Inline("继续".into()),
                attachments: vec![],
                created_at: Timestamp(0),
                turn_id: None,
            };
            m.turn_id = None;
            batch.push(MessageAppended { message: m }.into());
            batch.push(
                TurnFinished {
                    turn_id: tid(t),
                    state: TurnState::Completed,
                    usage: Default::default(),
                    celebrate: false,
                    change_summary: None,
                }
                .into(),
            );
        }
        written += batch.len();
        commit(&store, &ids, batch).await;
    }
    let t = Instant::now();
    let engine = engine(e, &store, &clock, &ids).await;
    let elapsed = t.elapsed();
    let turns = engine.folded().turn_count();
    println!(
        "engine_fold_startup 事件 {written:>6}  {:>7.1}ms  回合表 {turns} 条（约 {} KB）",
        elapsed.as_secs_f64() * 1000.0,
        turns * 60 / 1024
    );
}

async fn concurrent(e: &Env, fullfsync: Option<bool>, label: &str) {
    let clock = Arc::new(SysClock(Instant::now()));
    let ids = Arc::new(Ids(AtomicU64::new(0)));
    let store = open(e, fullfsync, &clock);
    commit(&store, &ids, world(16)).await;
    let engine = engine(e, &store, &clock, &ids).await;
    let per = 200u32;
    let t0 = Instant::now();
    let mut tasks = Vec::new();
    for c in 0..16u32 {
        let engine = engine.clone();
        let k = Arc::new(keys(e, c));
        tasks.push(tokio::spawn(async move {
            let r = req("git status");
            for _ in 0..per {
                engine.on_permission(&k, tid(c), &r).await;
            }
        }));
    }
    for t in tasks {
        t.await.unwrap();
    }
    let per_sec = f64::from(16 * per) / t0.elapsed().as_secs_f64();
    println!(
        "concurrent_16_allow fullfsync={label:<4} 次数 {}  {per_sec:>8.1} 次/秒",
        16 * per
    );
}

fn main() {
    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .unwrap();
    let e = env();
    let settings: &[(Option<bool>, &str)] = if cfg!(target_os = "macos") {
        &[(Some(true), "on"), (Some(false), "off")]
    } else {
        &[(None, "n/a")]
    };
    rt.block_on(async {
        for (f, label) in settings {
            allow_e2e(&e, *f, label).await;
        }
        catch_up(&e).await;
        let max = n_env("IWORK_BENCH_FOLD_MAX", 100_000);
        for total in [10_000, 100_000].into_iter().filter(|t| *t <= max) {
            fold_startup(&e, total).await;
        }
        for (f, label) in settings {
            concurrent(&e, *f, label).await;
        }
    });
    let _ = std::fs::remove_dir_all(&e.root);
}
