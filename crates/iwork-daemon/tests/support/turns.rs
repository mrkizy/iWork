//! 回合宿主的测试夹具：真存储（带封口闸）+ 真引擎 + 真观察者 + 进程内的假运行时 + 分发入口。
//! 先后一律用屏障与通知定（7.2a 表下）；等「应该发生」的结果都套时限。

#![allow(dead_code)]

use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use iwork_app::codec::ProtoEventCodec;
use iwork_core::approval::engine::{ApprovalEngine, EnginePorts};
use iwork_daemon::assembly;
use iwork_daemon::gate::{GatedStore, SealGate};
use iwork_daemon::handler::dispatch::{CommandPorts, Dispatcher};
use iwork_daemon::host::changes::{ReadBudget, VcsChanges};
use iwork_daemon::host::ports::{HostEnv, Wired};
use iwork_daemon::host::{HostError, TurnHost};
use iwork_daemon::publish::{PublishObserver, PublishSink, PublishState};
use iwork_daemon::time::{MonotonicIds, SystemClock, TokioTimer};
use iwork_domain::event::{AssistantCreated, ConversationCreated, DomainEvent, ProjectAdded};
use iwork_domain::id::{AssistantId, CommandId, ConversationId, RawUlid, TurnId};
use iwork_domain::turn::InterruptReason;
use iwork_domain::turn::TurnState;
use iwork_host::fs::OsPathResolver;
use iwork_ipc::peer::UserId;
use iwork_ipc::server::{ConnectionContext, PeerSummary, Published, RequestHandler};
use iwork_ports::agent::{
    AgentError, AgentPort, AgentSession, AgentUpdate, ContentBlock, PermissionHandler,
    RuntimeCapabilities, SandboxStatus, SessionSpec, TurnOutcome, UpdateStream,
};
use iwork_ports::live::{LivePublisher, MessageDelta};
use iwork_ports::store::{EventFilter, StoragePort};
use iwork_proto::pb;
use iwork_store::SqliteReadModel;
use tokio::sync::{Notify, mpsc};

use super::fixtures::*;
pub use super::hooked::{HookedRead, HookedStore, Knobs};

// ---------- 假运行时 ----------

/// 假运行时的控制面与记录。
#[derive(Default)]
pub struct Ctl {
    /// 每次开会话收到的续接编号
    pub opened_with: Mutex<Vec<Option<String>>>,
    /// 每次真正收下的 prompt：（会话编号，内容）
    pub prompts: Mutex<Vec<(String, Vec<ContentBlock>)>>,
    /// 为真时，回合停在「跑着」，直到 `finish_one` 放行或被取消
    pub hold: std::sync::atomic::AtomicBool,
    pub release: Notify,
    pub released: AtomicUsize,
    /// 收到过的取消
    pub cancels: AtomicUsize,
    /// 下一次开会话时给的会话编号；没设就续接沿用、新开按次数起名
    pub next_id: Mutex<Option<String>>,
    opened: AtomicUsize,
    /// 每收下一条 prompt 叫一声
    pub prompted: Notify,
    /// 为真时开会话报「找不到运行时程序」（运行时缺失）
    pub missing: std::sync::atomic::AtomicBool,
    /// 为真时带着旧编号续接报「会话记录不存在」
    pub lose_on_resume: std::sync::atomic::AtomicBool,
    /// 为真时回合里发一个要问用户的权限请求（`git push`），等判定方的答复
    pub ask: std::sync::atomic::AtomicBool,
    /// 会话声明的等审批上限；`None` = 不限
    pub wait_limit: Mutex<Option<Duration>>,
    /// 收到过的权限答复
    pub decisions: Mutex<Vec<iwork_ports::agent::PermissionDecision>>,
    /// 为真时开会话停在里面（会话已在运行时建好、还没交回编排），直到 `open_release`
    pub open_hold: std::sync::atomic::AtomicBool,
    pub open_release: Notify,
    pub opening: Notify,
    /// 为真时清树（`terminate`）永不返回：进程树清不掉
    pub hang_terminate: std::sync::atomic::AtomicBool,
    /// 为真时收下取消但不理：回合一直跑着，直到 `finish_one`
    pub ignore_cancel: std::sync::atomic::AtomicBool,
}

type Asker = Arc<
    dyn Fn(
            iwork_ports::agent::PermissionRequest,
        ) -> std::pin::Pin<
            Box<dyn std::future::Future<Output = iwork_ports::agent::PermissionDecision> + Send>,
        > + Send
        + Sync,
>;

impl Ctl {
    pub fn prompt_count(&self) -> usize {
        self.prompts.lock().unwrap().len()
    }

    /// 等运行时收下第 `n` 条 prompt（回合先落「开始执行」再发 prompt，D2：状态是 Running 不等于 prompt 已到）。
    pub async fn wait_prompts(&self, n: usize) {
        within("运行时收到 prompt", async {
            loop {
                let got = self.prompted.notified();
                if self.prompt_count() >= n {
                    return;
                }
                got.await;
            }
        })
        .await
    }

    /// 放行一个回合（照「跑完」收尾）：按 prompt 的先后，第 n 次放行放的是第 n 条 prompt，先放后到也算数。
    pub fn finish_one(&self) {
        self.released.fetch_add(1, Ordering::SeqCst);
        self.release.notify_waiters();
    }
}

#[derive(Clone)]
pub struct FakeAgent(pub Arc<Ctl>);

pub struct FakeSession {
    ctl: Arc<Ctl>,
    id: String,
    /// 当前这一回合的取消标记：每条 prompt 一个新的，取消只落在正在跑的那一回合上
    cancelled: Mutex<Arc<(std::sync::atomic::AtomicBool, Notify)>>,
    asker: Asker,
}

pub struct FakeUpdates {
    ctl: Arc<Ctl>,
    asker: Option<Asker>,
    cancelled: Arc<(std::sync::atomic::AtomicBool, Notify)>,
    /// 这是第几条 prompt（从 0 数）：放行数比它大就轮到它结束
    ticket: usize,
    step: u8,
}

impl AgentPort for FakeAgent {
    type Session = FakeSession;

    async fn capabilities(&self) -> Result<RuntimeCapabilities, AgentError> {
        Err(AgentError::RuntimeNotFound)
    }

    async fn open_session<H: PermissionHandler + 'static>(
        &self,
        spec: SessionSpec,
        on_permission: H,
    ) -> Result<FakeSession, AgentError> {
        let handler = Arc::new(on_permission);
        let asker: Asker = Arc::new(move |req| {
            let h = handler.clone();
            Box::pin(async move { h.on_permission(req).await })
        });
        if self.0.missing.load(Ordering::SeqCst) {
            return Err(AgentError::RuntimeNotFound);
        }
        if self.0.open_hold.load(Ordering::SeqCst) {
            let go = self.0.open_release.notified();
            self.0.opening.notify_waiters();
            go.await;
        }
        self.0.opened_with.lock().unwrap().push(spec.resume.clone());
        if spec.resume.is_some() && self.0.lose_on_resume.load(Ordering::SeqCst) {
            return Err(AgentError::SessionLost {
                reason: iwork_domain::session::SessionLostReason::NotFound,
            });
        }
        let n = self.0.opened.fetch_add(1, Ordering::SeqCst);
        let id = self.0.next_id.lock().unwrap().take();
        Ok(FakeSession {
            ctl: self.0.clone(),
            id: id.or(spec.resume).unwrap_or_else(|| format!("rt-{n}")),
            cancelled: Mutex::default(),
            asker,
        })
    }
}

impl AgentSession for FakeSession {
    type Updates = FakeUpdates;

    fn runtime_session_id(&self) -> Option<&str> {
        Some(&self.id)
    }
    fn permission_wait_limit(&self) -> Option<Duration> {
        *self.ctl.wait_limit.lock().unwrap()
    }
    fn sandbox(&self) -> SandboxStatus {
        SandboxStatus::Enforced
    }
    async fn terminate(&self) -> Result<(), AgentError> {
        if self.ctl.hang_terminate.load(Ordering::SeqCst) {
            std::future::pending::<()>().await;
        }
        Ok(())
    }
    async fn prompt(&self, blocks: Vec<ContentBlock>) -> Result<FakeUpdates, AgentError> {
        let ticket = {
            let mut prompts = self.ctl.prompts.lock().unwrap();
            prompts.push((self.id.clone(), blocks));
            prompts.len() - 1
        };
        self.ctl.prompted.notify_waiters();
        let cancelled: Arc<(std::sync::atomic::AtomicBool, Notify)> = Arc::default();
        *self.cancelled.lock().unwrap() = cancelled.clone();
        Ok(FakeUpdates {
            ctl: self.ctl.clone(),
            asker: self
                .ctl
                .ask
                .load(Ordering::SeqCst)
                .then(|| self.asker.clone()),
            cancelled,
            ticket,
            step: 0,
        })
    }
    async fn cancel(&self) -> Result<(), AgentError> {
        self.ctl.cancels.fetch_add(1, Ordering::SeqCst);
        if self.ctl.ignore_cancel.load(Ordering::SeqCst) {
            return Ok(());
        }
        let c = self.cancelled.lock().unwrap().clone();
        c.0.store(true, Ordering::SeqCst);
        c.1.notify_waiters();
        Ok(())
    }
    async fn close(self) -> Result<(), AgentError> {
        Ok(())
    }
}

impl UpdateStream for FakeUpdates {
    async fn next(&mut self) -> Option<AgentUpdate> {
        self.step += 1;
        match self.step {
            1 => Some(AgentUpdate::Text {
                offset: 0,
                text: "好的".into(),
            }),
            2 => {
                if let Some(ask) = self.asker.take() {
                    let req = iwork_ports::agent::PermissionRequest {
                        tool: iwork_domain::action::ToolRef("Bash".into()),
                        raw_input: iwork_ports::agent::RawToolInput(
                            r#"{"command":"git push"}"#.into(),
                        ),
                        bypass_sandbox: false,
                        updates_before: 1,
                        cwd: None,
                    };
                    let cancelled = self.cancelled.1.notified();
                    if self.cancelled.0.load(Ordering::SeqCst) {
                        return Some(AgentUpdate::Finished(TurnOutcome::Cancelled));
                    }
                    tokio::select! {
                        d = ask(req) => self.ctl.decisions.lock().unwrap().push(d),
                        _ = cancelled => {
                            return Some(AgentUpdate::Finished(TurnOutcome::Cancelled));
                        }
                    }
                }
                if self.ctl.hold.load(Ordering::SeqCst) {
                    loop {
                        let released = self.ctl.release.notified();
                        let cancelled = self.cancelled.1.notified();
                        if self.cancelled.0.load(Ordering::SeqCst) {
                            return Some(AgentUpdate::Finished(TurnOutcome::Cancelled));
                        }
                        if self.ctl.released.load(Ordering::SeqCst) > self.ticket {
                            break;
                        }
                        tokio::select! {
                            _ = released => {}
                            _ = cancelled => {}
                        }
                    }
                }
                if self.cancelled.0.load(Ordering::SeqCst) {
                    return Some(AgentUpdate::Finished(TurnOutcome::Cancelled));
                }
                Some(AgentUpdate::Finished(TurnOutcome::Completed))
            }
            _ => None,
        }
    }
}

pub struct NoLive;
impl LivePublisher for NoLive {
    fn message_delta(&self, _delta: MessageDelta) {}
}

/// 发布到哪都不去（观察者照真的装，只是不接 ipc）。
pub struct Nowhere;
impl PublishSink for Nowhere {
    fn publish(&self, _p: Published) {}
}

/// 版本控制的假实现：数被调用的次数（D10 的 `probe_disabled_makes_no_vcs_calls`）。
#[derive(Default)]
pub struct CountingVcs {
    pub calls: AtomicUsize,
    /// 为真时 `baseline` 一直不返回，直到 `unhang`
    pub hang: std::sync::atomic::AtomicBool,
    pub unhang: Notify,
    /// 正卡在 `baseline` 里的读
    pub hanging: AtomicUsize,
    pub baselines: AtomicUsize,
}

impl iwork_ports::vcs::VcsProvider for CountingVcs {
    fn capabilities(&self) -> iwork_ports::vcs::VcsCapabilities {
        iwork_ports::vcs::VcsCapabilities {
            cheap_isolated_workspace: false,
        }
    }
    async fn detect(
        &self,
        _: &std::path::Path,
    ) -> Result<iwork_ports::vcs::Detection, iwork_ports::vcs::VcsError> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        Ok(iwork_ports::vcs::Detection::NotVersionControlled)
    }
    async fn init(&self, _: &std::path::Path) -> Result<(), iwork_ports::vcs::VcsError> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        Ok(())
    }
    async fn clone_repository<S: iwork_ports::vcs::ProgressSink + 'static>(
        &self,
        _: &str,
        _: &std::path::Path,
        _: S,
    ) -> Result<(), iwork_ports::vcs::VcsError> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        Ok(())
    }
    async fn baseline(
        &self,
        _: &iwork_ports::vcs::RepoRoot,
    ) -> Result<iwork_ports::vcs::BaselineRef, iwork_ports::vcs::VcsError> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        self.baselines.fetch_add(1, Ordering::SeqCst);
        if self.hang.load(Ordering::SeqCst) {
            let go = self.unhang.notified();
            self.hanging.fetch_add(1, Ordering::SeqCst);
            go.await;
            self.hanging.fetch_sub(1, Ordering::SeqCst);
        }
        Ok(iwork_ports::vcs::BaselineRef("b".into()))
    }
    async fn status(
        &self,
        _: &iwork_ports::vcs::RepoRoot,
    ) -> Result<iwork_ports::vcs::WorkspaceStatus, iwork_ports::vcs::VcsError> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        Ok(Default::default())
    }
    async fn diff(
        &self,
        _: &iwork_ports::vcs::RepoRoot,
        _: iwork_ports::vcs::DiffRange,
    ) -> Result<iwork_ports::vcs::Diff, iwork_ports::vcs::VcsError> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        Ok(Default::default())
    }
    async fn worktree_versions(
        &self,
        _: &iwork_ports::vcs::RepoRoot,
        paths: &[PathBuf],
    ) -> Result<Vec<iwork_ports::vcs::PathVersion>, iwork_ports::vcs::VcsError> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        Ok(vec![iwork_ports::vcs::PathVersion::Absent; paths.len()])
    }
}

// ---------- 装配 ----------

pub type Gated = GatedStore<Store>;
pub type Read = SqliteReadModel<ProtoEventCodec>;
/// 宿主与引擎用的存储、宿主用的读模型：外面套一层能停住、能计数的外壳（`H::knobs`）
pub type HStore = HookedStore<Gated>;
pub type HRead = HookedRead<Read>;
pub type Engine = ApprovalEngine<
    SystemClock,
    MonotonicIds<SystemClock>,
    HStore,
    FakeAgent,
    TokioTimer,
    OsPathResolver,
>;
pub type Ports = Wired<
    HStore,
    HRead,
    FakeAgent,
    Arc<Engine>,
    NoLive,
    SystemClock,
    MonotonicIds<SystemClock>,
    TokioTimer,
    OsPathResolver,
    VcsChanges<CountingVcs>,
>;
pub type Disp = Dispatcher<Gated, Read, Engine>;

pub const CONV: u128 = 10;
pub const PROJECT: u128 = 7;

pub struct H {
    pub env: Env,
    pub raw: Arc<Store>,
    pub gate: Arc<SealGate>,
    pub engine: Arc<Engine>,
    pub host: TurnHost<Ports>,
    pub d: Arc<Disp>,
    pub agent: Arc<Ctl>,
    pub vcs: Arc<CountingVcs>,
    pub budget: Arc<ReadBudget>,
    pub knobs: Arc<Knobs>,
    pub publishing: PublishState,
    pub host_task: Option<tokio::task::JoinHandle<HostError>>,
    pub sync_task: Option<tokio::task::JoinHandle<()>>,
}

/// 选项。
#[derive(Default, Clone)]
pub struct Opts {
    /// 改动探针开不开（组装处在 7/F-2 修好之前是关的）
    pub probe_enabled: bool,
    /// 起宿主之前额外提交的事件
    pub extra: Vec<DomainEvent>,
    /// 宿主拿一个没人叫的唤醒（提交观察者叫不到它）：D1–D4 的保底一格
    pub detached_wake: bool,
}

/// 夹具里的对话：项目 PROJECT 的项目对话 CONV。内置助手由组装处的第 1a 步登记。
pub fn seed() -> Vec<DomainEvent> {
    vec![
        ProjectAdded {
            project: project(PROJECT),
        }
        .into(),
        ConversationCreated {
            conversation: project_conversation(CONV, PROJECT),
        }
        .into(),
    ]
}

/// 在已有的数据目录上装一套（重启用：同一个 `env` 再调一次）。
pub async fn assemble(env: Env, opts: Opts, agent: Arc<Ctl>) -> H {
    let raw = Arc::new(env.open());
    let publishing = PublishState::new();
    raw.set_commit_observer(Box::new(PublishObserver::new(
        Arc::new(Nowhere),
        publishing.clone(),
    )))
    .await
    .unwrap();
    let gate = Arc::new(SealGate::default());
    let gated = GatedStore::new(raw.clone(), gate.clone());
    let knobs = Arc::new(Knobs::default());
    let hooked = HookedStore {
        inner: gated.clone(),
        knobs: knobs.clone(),
    };
    let read = read_model(&raw);
    let ids = MonotonicIds::new(SystemClock::new());
    assembly::register_builtin_assistant(&gated, &read, &SystemClock::new())
        .await
        .unwrap();
    if !opts.extra.is_empty() {
        gated
            .commit(iwork_ports::store::CommitRequest {
                command_id: CommandId::from_raw(ids.next_raw()),
                actor: iwork_domain::common::Actor("test".into()),
                events: opts.extra.clone(),
                outbox: vec![],
            })
            .await
            .unwrap();
    }
    let fs = OsPathResolver::new(env.tmp.path().join("home"), false);
    let host_env = HostEnv {
        platform: HostEnv::platform(),
        home: env.tmp.path().join("home"),
        data_dir: env.dir.root().to_path_buf(),
        tmp_dirs: vec![std::env::temp_dir()],
    };
    let keys = assembly::keys_for_pending(&read, &host_env, &fs)
        .await
        .unwrap();
    let engine = assembly::start_engine(
        EnginePorts {
            clock: SystemClock::new(),
            ids: MonotonicIds::new(SystemClock::new()),
            store: hooked.clone(),
            reviewer: FakeAgent(agent.clone()),
            timer: TokioTimer,
            fs: fs.clone(),
        },
        keys,
        &gated,
        &ids,
    )
    .await
    .unwrap();
    let vcs = Arc::new(CountingVcs::default());
    let budget = Arc::new(ReadBudget::default());
    let host = TurnHost::start(
        Arc::new(Wired {
            store: hooked.clone(),
            read: HookedRead {
                inner: read_model(&raw),
                knobs: knobs.clone(),
            },
            agent: FakeAgent(agent.clone()),
            engine: engine.clone(),
            live: NoLive,
            clock: SystemClock::new(),
            ids: MonotonicIds::new(SystemClock::new()),
            timer: TokioTimer,
            fs,
            env: host_env,
            changes: VcsChanges::new(vcs.clone(), opts.probe_enabled, budget.clone())
                .with_capture_limit(Duration::from_millis(100)),
            gate: gate.clone(),
        }),
        if opts.detached_wake {
            Arc::new(Notify::new())
        } else {
            publishing.host.clone()
        },
    )
    .await
    .unwrap();
    let d = Dispatcher::new(CommandPorts {
        store: Arc::new(gated),
        read: Arc::new(read),
        engine: engine.clone(),
        ids: Arc::new(MonotonicIds::new(SystemClock::new())),
        clock: Arc::new(SystemClock::new()),
        host: Arc::new(host.clone()),
    });
    H {
        env,
        raw,
        gate,
        engine,
        host,
        d: Arc::new(d),
        agent,
        vcs,
        budget,
        knobs,
        publishing,
        host_task: None,
        sync_task: None,
    }
}

/// 新库 + 夹具对话，宿主没起（测试要先装屏障时用）。
pub async fn harness(opts: Opts) -> H {
    let env = Env::new();
    {
        let raw = env.open();
        raw.commit(cmd(1, seed())).await.unwrap();
        raw.close().unwrap();
    }
    assemble(env, opts, Arc::default()).await
}

impl H {
    /// 起宿主的常驻任务。
    pub fn start_host(&mut self) {
        let host = self.host.clone();
        self.host_task = Some(tokio::spawn(async move { host.run().await }));
        // engine-sync：观察者叫醒它，它追平后叫醒宿主（和组装处一样）
        let (engine, wake, host) = (
            self.engine.clone(),
            self.publishing.engine.clone(),
            self.host.clone(),
        );
        self.sync_task = Some(tokio::spawn(iwork_daemon::tasks::engine_sync::run(
            engine,
            wake,
            move || host.wake(),
        )));
    }

    /// 只起宿主的常驻任务，不起 `engine-sync`（它追平后会叫醒宿主）。配合 `Opts::detached_wake` 把唤醒全部压住。
    pub fn start_host_only(&mut self) {
        let host = self.host.clone();
        self.host_task = Some(tokio::spawn(async move { host.run().await }));
    }

    /// 停掉宿主（不封口：测试结束或重启前用），放手存储。
    pub async fn stop(mut self) -> Env {
        if let Some(t) = self.host_task.take() {
            t.abort();
            let _ = t.await;
        }
        if let Some(t) = self.sync_task.take() {
            t.abort();
            let _ = t.await;
        }
        let env = self.env;
        drop(self.d);
        drop(self.host);
        drop(self.engine);
        env
    }

    pub async fn ask(&self, id: CommandId, kind: pb::command::Kind) -> pb::Response {
        self.d
            .handle(
                &ctx(),
                pb::Request {
                    request_id: 1,
                    body: Some(pb::request::Body::Command(pb::Command {
                        command_id: id.to_string(),
                        kind: Some(kind),
                    })),
                },
            )
            .await
    }

    /// 发一条消息，返回受理的那一笔里排上的回合编号。
    pub async fn send(&self, n: u128, text: &str) -> TurnId {
        let r = self
            .ask(
                fresh_id(n),
                pb::command::Kind::SendMessage(pb::SendMessage {
                    conversation_id: cid(CONV).to_string(),
                    text: text.into(),
                    attachment_ids: vec![],
                }),
            )
            .await;
        let Some(pb::response::Body::Accepted(a)) = r.body else {
            panic!("没受理：{r:?}")
        };
        events(&self.raw)
            .await
            .into_iter()
            .filter(|(s, _)| *s >= a.first_seq && *s <= a.last_seq)
            .find_map(|(_, e)| match e {
                DomainEvent::TurnQueued(q) => Some(q.turn.id),
                _ => None,
            })
            .expect("这一笔里有 TurnQueued")
    }

    pub async fn turn_state(&self, t: TurnId) -> TurnState {
        use iwork_ports::read_model::ReadModelPort;
        read_model(&self.raw)
            .turn_with_conversation(t)
            .await
            .unwrap()
            .expect("回合在")
            .0
            .state
    }

    /// 等回合到某个状态（套时限）。
    pub async fn wait_state(&self, t: TurnId, want: impl Fn(TurnState) -> bool) -> TurnState {
        let waited = tokio::time::timeout(Duration::from_secs(20), async {
            loop {
                let s = self.turn_state(t).await;
                if want(s) {
                    return s;
                }
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        })
        .await;
        match waited {
            Ok(s) => s,
            Err(_) => {
                let c = self.host.counters();
                panic!(
                    "20 秒内回合 {t} 没到预期状态，现在是 {:?}；prompt {} 条；宿主 {} 轮、交活 {}、结束 {}、准备的读 {}",
                    self.turn_state(t).await,
                    self.agent.prompt_count(),
                    c.rounds.load(Ordering::SeqCst),
                    c.handed.load(Ordering::SeqCst),
                    c.finished.load(Ordering::SeqCst),
                    c.prepare_reads.load(Ordering::SeqCst),
                )
            }
        }
    }

    /// 等宿主跑完至少 `n` 轮对账。
    pub async fn wait_rounds(&self, n: u64) {
        let c = self.host.counters();
        let waited = tokio::time::timeout(Duration::from_secs(20), async {
            loop {
                let done = c.round_done.notified();
                if c.rounds.load(Ordering::SeqCst) >= n {
                    return;
                }
                done.await;
            }
        })
        .await;
        if waited.is_err() {
            panic!(
                "20 秒内宿主没跑到第 {n} 轮：现在 {} 轮，交活 {}，结束 {}，准备的读 {}",
                c.rounds.load(Ordering::SeqCst),
                c.handed.load(Ordering::SeqCst),
                c.finished.load(Ordering::SeqCst),
                c.prepare_reads.load(Ordering::SeqCst)
            );
        }
    }
}

pub fn ctx() -> ConnectionContext {
    ConnectionContext {
        connection_id: 1,
        session_id: "s".into(),
        peer: PeerSummary {
            pid: 1,
            user: UserId::Uid(1),
        },
        client_kind: pb::ClientKind::Desktop,
        signature_checked: true,
    }
}

pub fn fresh_id(n: u128) -> CommandId {
    CommandId::from_raw(RawUlid::from_parts(1 << 40, n))
}

pub async fn events(raw: &Store) -> Vec<(u64, DomainEvent)> {
    raw.read_events(EventFilter::All, iwork_domain::id::Seq(0), 100_000)
        .await
        .unwrap()
        .into_iter()
        .map(|e| (e.seq.0, e.event))
        .collect()
}

pub async fn within<T>(what: &str, f: impl std::future::Future<Output = T>) -> T {
    tokio::time::timeout(Duration::from_secs(20), f)
        .await
        .unwrap_or_else(|_| panic!("20 秒内没等到：{what}"))
}

/// 内置助手之外的一个普通助手（造「助手不同」的回合用）。
pub fn other_assistant(n: u128) -> DomainEvent {
    AssistantCreated {
        assistant: assistant(n, false),
    }
    .into()
}

pub fn builtin() -> AssistantId {
    AssistantId::BUILTIN_IWORK
}

pub fn conversation() -> ConversationId {
    cid(CONV)
}

trait NextRaw {
    fn next_raw(&self) -> RawUlid;
}
impl NextRaw for MonotonicIds<SystemClock> {
    fn next_raw(&self) -> RawUlid {
        iwork_ports::clock::IdGenerator::next(self)
    }
}

/// 进程没了以后再起来（R 组）：重开同一个库，跑启动表第 8 步的崩溃恢复，再装一套。运行时控制面换新的。
pub async fn restart(env: Env, opts: Opts) -> H {
    {
        use iwork_ports::read_model::ReadModelPort;
        let raw = env.open();
        let unsettled = read_model(&raw).unsettled().await.unwrap();
        let plan = iwork_core::recovery::plan(&unsettled).unwrap();
        if let Some(req) = iwork_daemon::recovery::recovery_request(
            plan,
            CommandId::from_raw(MonotonicIds::new(SystemClock::new()).next_raw()),
        ) {
            raw.commit(req).await.unwrap();
        }
        raw.close().unwrap();
    }
    assemble(
        env,
        Opts {
            extra: vec![],
            ..opts
        },
        Arc::default(),
    )
    .await
}

// ---------- 各测试文件共用的小工具 ----------

/// 屏障：到了发一声，等放行。
pub fn barrier() -> (
    iwork_daemon::host::HostHook,
    mpsc::UnboundedReceiver<()>,
    Arc<Notify>,
) {
    let (tx, rx) = mpsc::unbounded_channel();
    let release = Arc::new(Notify::new());
    let r = release.clone();
    let hook: iwork_daemon::host::HostHook = Arc::new(move || {
        let (tx, r) = (tx.clone(), r.clone());
        Box::pin(async move {
            let go = r.notified();
            let _ = tx.send(());
            go.await;
        })
    });
    (hook, rx, release)
}

pub const WAIT_LIMIT: TurnState = TurnState::Interrupted {
    reason: InterruptReason::ApprovalWaitLimit,
};

/// 让回合在等审批上到期中断：运行时发一个要问用户的请求，会话声明的等待上限很短。
pub async fn interrupted_by_wait_limit(h: &H, n: u128) -> iwork_domain::id::TurnId {
    h.agent.ask.store(true, Ordering::SeqCst);
    *h.agent.wait_limit.lock().unwrap() = Some(std::time::Duration::from_millis(50));
    let finished = h.host.counters().finished.load(Ordering::SeqCst);
    let t = h.send(n, "推一下").await;
    h.wait_state(t, |s| s == WAIT_LIMIT).await;
    // 状态落盘之后，这一趟还要在宿主的锁里记下结束
    within("第一趟记完结束", async {
        while h.host.counters().finished.load(Ordering::SeqCst) <= finished {
            tokio::task::yield_now().await;
        }
    })
    .await;
    t
}

pub fn resume(t: iwork_domain::id::TurnId) -> iwork_proto::pb::command::Kind {
    iwork_proto::pb::command::Kind::ResumeTurn(iwork_proto::pb::ResumeTurn {
        turn_id: t.to_string(),
    })
}

pub fn cancel(t: iwork_domain::id::TurnId) -> iwork_proto::pb::command::Kind {
    iwork_proto::pb::command::Kind::CancelTurn(iwork_proto::pb::CancelTurn {
        turn_id: t.to_string(),
    })
}

pub fn finishes(req: &iwork_ports::store::CommitRequest, to: TurnState) -> bool {
    req.events
        .iter()
        .any(|e| matches!(e, DomainEvent::TurnStateChanged(s) if s.to == to))
}

pub fn accepted(r: &iwork_proto::pb::Response) -> iwork_proto::pb::CommandAccepted {
    match &r.body {
        Some(iwork_proto::pb::response::Body::Accepted(a)) => *a,
        other => panic!("没受理：{other:?}"),
    }
}

pub fn approval_of(
    evs: &[(u64, DomainEvent)],
    t: iwork_domain::id::TurnId,
) -> Vec<iwork_domain::id::ApprovalId> {
    evs.iter()
        .filter_map(|(_, e)| match e {
            DomainEvent::ApprovalRequested(a) if a.approval.turn_id == t => Some(a.approval.id),
            _ => None,
        })
        .collect()
}

pub fn allow(a: iwork_domain::id::ApprovalId) -> iwork_proto::pb::command::Kind {
    iwork_proto::pb::command::Kind::DecideApproval(iwork_proto::pb::DecideApproval {
        approval_id: a.to_string(),
        decision: iwork_proto::pb::ApprovalDecision::Allow.into(),
        remember: 0,
    })
}
