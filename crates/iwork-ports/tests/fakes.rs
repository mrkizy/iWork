//! 每个端口一个最小内存假实现，证明签名真能写出实现、实现能跨线程用（方案 5.2、第 8 节 S6、S9）；
//! 外加四条行为测试。不引任何异步运行时：用 `pollster::block_on` 驱动 future，用 std 线程并发。
//! 实现两种写法都有：`async fn`（证明它满足 trait 的 `impl Future + Send`），以及先取数再返回
//! `async move` 的显式写法（锁不跨 await 时这样写）。

/// `PathResolver` 的假实现；别的 crate 的测试经 `#[path]` 引入同一份。
mod fake_fs;

use std::collections::HashMap;
use std::future::Future;
use std::ops::Range;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Condvar, Mutex};

use iwork_domain::approval_mode::ProjectDefaultMode;
use iwork_domain::assistant::Assistant;
use iwork_domain::common::{Actor, ArtifactArea, ArtifactRef, Sha256, Timestamp};
use iwork_domain::event::{DomainEvent, ProjectAdded};
use iwork_domain::id::{CommandId, ConversationId, MessageId, ProjectId, RawUlid, Seq, TurnId};
use iwork_domain::project::{PathStatus, Project, ProjectSettings, VcsKind};
use iwork_domain::rule::RuleEntry;
use iwork_domain::runtime::{Runtime, RuntimeKind};
use iwork_domain::settings::GlobalSettings;
use iwork_domain::user_action::ExitStatus;
use iwork_ports::agent::{
    AgentError, AgentPort, AgentSession, AgentUpdate, ContentBlock, PermissionDecision,
    PermissionHandler, PermissionRequest, RuntimeCapabilities, SessionSpec, TurnOutcome,
    UpdateStream,
};
use iwork_ports::artifact::{ArtifactError, ArtifactStore, GcReport};
use iwork_ports::clock::{Clock, IdGenerator, MonotonicInstant};
use iwork_ports::discovery::{DiscoveryError, RuntimeDiscovery, RuntimeProbe};
use iwork_ports::exec::{
    ExecError, ExecutionBackend, ExecutionHandle, OutputSink, UserExecRequest, UserPatchRequest,
};
use iwork_ports::notify::{Notifier, NotifyError, SystemNotification};
use iwork_ports::read_model::{
    ConversationSummary, ConversationView, MessagePage, PendingItem, ProjectSummary, QuitCounts,
    ReadModelPort, RuntimeQuota, SnapshotInputs, SummarySource, TurnWork, Unsettled,
};
use iwork_ports::secret::{SecretError, SecretName, SecretStore, SecretValue};
use iwork_ports::store::{
    CodecError, CommitReceipt, CommitRequest, DeliveryState, EncodedEvent, EventCodec, EventFilter,
    NewOutboxItem, OutboxId, OutboxItem, OutboxSelector, Projector, RebuildReport, RequeueReport,
    StorageError, StoragePort, StoredEvent,
};
use iwork_ports::vcs::{
    BaselineRef, Detection, Diff, DiffRange, ProgressSink, RepoRoot, VcsCapabilities, VcsError,
    VcsProvider, WorkspaceStatus,
};

// ---------- 时钟与 ID ----------

/// 测试可拨动的时钟。
struct FakeClock {
    now_ms: AtomicU64,
}

impl Clock for FakeClock {
    fn now(&self) -> Timestamp {
        Timestamp(self.now_ms.load(Ordering::SeqCst) as i64)
    }
    fn monotonic(&self) -> MonotonicInstant {
        MonotonicInstant(self.now_ms.load(Ordering::SeqCst) * 1_000_000)
    }
}

/// 单调 ULID 生成器的参照实现（`IdGenerator` 文档里的契约）：
/// 同一毫秒递增随机部分；随机部分用尽进下一毫秒；时钟回拨时沿用上一次的毫秒数。
struct MonotonicIdGenerator<'c> {
    clock: &'c FakeClock,
    last: Mutex<Option<RawUlid>>,
    /// 每个新毫秒的随机部分起点（真实现取随机数，这里固定，便于构造溢出用例）
    fresh_random: u128,
}

impl IdGenerator for MonotonicIdGenerator<'_> {
    fn next(&self) -> RawUlid {
        let now = self.clock.now().0 as u64;
        let mut last = self.last.lock().unwrap();
        let id = match *last {
            Some(prev) if now <= prev.timestamp_ms() => prev
                .increment()
                .unwrap_or_else(|| RawUlid::from_parts(prev.timestamp_ms() + 1, self.fresh_random)),
            _ => RawUlid::from_parts(now, self.fresh_random),
        };
        *last = Some(id);
        id
    }
}

// ---------- 存储：codec、projector、内存 store ----------

/// 只认 `ProjectAdded` 的测试 codec：载荷是「id 换行 name」。
struct ProjectOnlyCodec;

fn demo_project(id: ProjectId, name: &str) -> Project {
    Project {
        id,
        root_path: PathBuf::from("/work/demo"),
        vcs_kind: VcsKind("v".into()),
        name: name.into(),
        pinned: false,
        last_active_at: None,
        path_status: PathStatus::Ok,
        settings: ProjectSettings {
            default_approval_mode: ProjectDefaultMode::Manual,
            ..ProjectSettings::default()
        },
    }
}

impl EventCodec for ProjectOnlyCodec {
    fn protocol_major(&self) -> u32 {
        1
    }
    fn encode(&self, event: &DomainEvent) -> Result<EncodedEvent, CodecError> {
        match event {
            DomainEvent::ProjectAdded(e) => Ok(EncodedEvent {
                kind: event.kind(),
                payload: format!("{}\n{}", e.project.id, e.project.name).into_bytes(),
            }),
            other => Err(CodecError::Unencodable {
                kind: other.kind(),
                message: "测试 codec 只认 project_added".into(),
            }),
        }
    }
    fn decode(&self, kind: &str, payload: &[u8]) -> Result<DomainEvent, CodecError> {
        let malformed = |field: &str, message: String| CodecError::Malformed {
            kind: kind.into(),
            field: field.into(),
            message,
        };
        if kind != "project_added" {
            return Err(CodecError::UnknownKind { kind: kind.into() });
        }
        let text =
            String::from_utf8(payload.to_vec()).map_err(|e| malformed("payload", e.to_string()))?;
        let (id, name) = text
            .split_once('\n')
            .ok_or_else(|| malformed("payload", "缺少换行".into()))?;
        let id: ProjectId = id
            .parse()
            .map_err(|e: iwork_domain::id::IdParseError| malformed("id", e.to_string()))?;
        Ok(ProjectAdded {
            project: demo_project(id, name),
        }
        .into())
    }
}

/// 投影表：只有 projects。
#[derive(Debug, Clone, Default, PartialEq)]
struct InMemoryTables {
    projects: Vec<(Seq, Project)>,
}

/// 只维护 projects 表的投影，记下被调用了几次。
#[derive(Default)]
struct ProjectsProjector {
    calls: AtomicUsize,
}

impl Projector for ProjectsProjector {
    type Tx = InMemoryTables;
    fn project(
        &self,
        tx: &mut InMemoryTables,
        seq: Seq,
        event: &DomainEvent,
    ) -> Result<(), StorageError> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        if let DomainEvent::ProjectAdded(e) = event {
            tx.projects.push((seq, e.project.clone()));
        }
        Ok(())
    }
}

struct EncodedRow {
    seq: Seq,
    at: Timestamp,
    actor: Actor,
    kind: &'static str,
    payload: Vec<u8>,
}

#[derive(Default)]
struct StoreState {
    events: Vec<EncodedRow>,
    commands: HashMap<CommandId, CommitReceipt>,
    outbox: Vec<OutboxItem>,
    tables: InMemoryTables,
}

/// 内存 store：在线提交与重建都经注入的 codec 与同一个 projector。
struct InMemoryStore<C, P> {
    codec: C,
    projector: P,
    clock: FakeClock,
    state: Mutex<StoreState>,
}

impl<C: EventCodec, P: Projector<Tx = InMemoryTables>> InMemoryStore<C, P> {
    fn new(codec: C, projector: P) -> Self {
        Self {
            codec,
            projector,
            clock: FakeClock {
                now_ms: AtomicU64::new(1_000),
            },
            state: Mutex::default(),
        }
    }

    fn tables(&self) -> InMemoryTables {
        self.state.lock().unwrap().tables.clone()
    }

    fn clear_tables(&self) {
        self.state.lock().unwrap().tables = InMemoryTables::default();
    }

    fn outbox_len(&self) -> usize {
        self.state.lock().unwrap().outbox.len()
    }

    /// 同步版提交：整个「事务」在一把锁里，失败时什么都不留。
    fn commit_now(&self, req: CommitRequest) -> Result<CommitReceipt, StorageError> {
        let mut state = self.state.lock().unwrap();
        if let Some(receipt) = state.commands.get(&req.command_id) {
            return Ok(CommitReceipt {
                replayed: true,
                ..*receipt
            });
        }
        let mut staged = StoreState {
            events: Vec::new(),
            commands: HashMap::new(),
            outbox: Vec::new(),
            tables: state.tables.clone(),
        };
        let head = state.events.last().map_or(0, |r| r.seq.0);
        for (i, event) in req.events.iter().enumerate() {
            let seq = Seq(head + 1 + i as u64);
            let encoded = self.codec.encode(event)?;
            self.projector.project(&mut staged.tables, seq, event)?;
            staged.events.push(EncodedRow {
                seq,
                at: self.clock.now(),
                actor: req.actor.clone(),
                kind: encoded.kind,
                payload: encoded.payload,
            });
        }
        let receipt = CommitReceipt {
            first_seq: Seq(head + 1),
            last_seq: Seq(head + req.events.len() as u64),
            replayed: false,
        };
        let next_outbox = state.outbox.len() as u64;
        state.tables = staged.tables;
        state.events.extend(staged.events);
        state.outbox.extend(
            req.outbox
                .into_iter()
                .enumerate()
                .map(|(i, item)| OutboxItem {
                    id: OutboxId(next_outbox + i as u64 + 1),
                    target: item.target,
                    payload: item.payload,
                    state: DeliveryState::Queued,
                    attempts: 0,
                }),
        );
        state.commands.insert(req.command_id, receipt);
        Ok(receipt)
    }
}

impl<C: EventCodec, P: Projector<Tx = InMemoryTables>> StoragePort for InMemoryStore<C, P> {
    fn commit(
        &self,
        req: CommitRequest,
    ) -> impl Future<Output = Result<CommitReceipt, StorageError>> + Send {
        let result = self.commit_now(req);
        async move { result }
    }

    /// 提交在 `commit` 调用的那一下就同步做完（`commit_now`），所以查到的是之前调用过的每一笔。
    fn command_receipt(
        &self,
        command_id: CommandId,
    ) -> impl Future<Output = Result<Option<CommitReceipt>, StorageError>> + Send {
        let found = self
            .state
            .lock()
            .unwrap()
            .commands
            .get(&command_id)
            .map(|r| CommitReceipt {
                replayed: true,
                ..*r
            });
        async move { Ok(found) }
    }

    fn head_seq(&self) -> impl Future<Output = Result<Seq, StorageError>> + Send {
        let head = self
            .state
            .lock()
            .unwrap()
            .events
            .last()
            .map_or(Seq(0), |r| r.seq);
        async move { Ok(head) }
    }

    fn read_events(
        &self,
        filter: EventFilter,
        from: Seq,
        limit: usize,
    ) -> impl Future<Output = Result<Vec<StoredEvent>, StorageError>> + Send {
        let state = self.state.lock().unwrap();
        let result = state
            .events
            .iter()
            .filter(|r| r.seq > from)
            .map(|r| {
                Ok(StoredEvent {
                    seq: r.seq,
                    at: r.at,
                    actor: r.actor.clone(),
                    event: self.codec.decode(r.kind, &r.payload)?,
                })
            })
            .filter(|e: &Result<StoredEvent, StorageError>| match (&filter, e) {
                (EventFilter::All, _) | (_, Err(_)) => true,
                (EventFilter::Streams(selectors), Ok(e)) => {
                    let (kind, id) = e.event.stream();
                    selectors
                        .iter()
                        .any(|s| s.kind == kind && (s.id.is_none() || s.id == id))
                }
            })
            .take(limit)
            .collect();
        async move { result }
    }

    async fn checkpoint_message(
        &self,
        _message_id: MessageId,
        _offset: u64,
        _text: &str,
    ) -> Result<(), StorageError> {
        Ok(())
    }

    fn outbox_pending(
        &self,
        limit: usize,
    ) -> impl Future<Output = Result<Vec<OutboxItem>, StorageError>> + Send {
        let items: Vec<_> = self
            .state
            .lock()
            .unwrap()
            .outbox
            .iter()
            .filter(|i| i.state == DeliveryState::Queued)
            .take(limit)
            .cloned()
            .collect();
        async move { Ok(items) }
    }

    fn outbox_mark(
        &self,
        id: OutboxId,
        state: DeliveryState,
    ) -> impl Future<Output = Result<(), StorageError>> + Send {
        let mut guard = self.state.lock().unwrap();
        let result = match guard.outbox.iter_mut().find(|i| i.id == id) {
            Some(item) => {
                item.state = state;
                Ok(())
            }
            None => Err(StorageError::NotFound {
                entity: "outbox",
                id: id.0.to_string(),
            }),
        };
        async move { result }
    }

    fn outbox_requeue_dispatched(
        &self,
        redeliver_prefixes: Vec<String>,
        fail_prefixes: Vec<String>,
    ) -> impl Future<Output = Result<RequeueReport, StorageError>> + Send {
        // NoneOf 命中 = 不以其中任何前缀开头；两边都不沾的原样留在 Dispatched
        let not_redeliver = OutboxSelector::NoneOf(redeliver_prefixes);
        let not_fail = OutboxSelector::NoneOf(fail_prefixes);
        let mut report = RequeueReport::default();
        for item in &mut self.state.lock().unwrap().outbox {
            if item.state != DeliveryState::Dispatched {
                continue;
            }
            if !not_redeliver.matches(&item.target) {
                item.state = DeliveryState::Queued;
                report.requeued += 1;
            } else if !not_fail.matches(&item.target) {
                item.state = DeliveryState::Failed;
                report.failed += 1;
            }
        }
        async move { Ok(report) }
    }

    fn outbox_pending_where(
        &self,
        selector: OutboxSelector,
        limit: usize,
    ) -> impl Future<Output = Result<Vec<OutboxItem>, StorageError>> + Send {
        let items: Vec<_> = self
            .state
            .lock()
            .unwrap()
            .outbox
            .iter()
            .filter(|i| i.state == DeliveryState::Queued && selector.matches(&i.target))
            .take(limit)
            .cloned()
            .collect();
        async move { Ok(items) }
    }

    fn rebuild_projections(
        &self,
    ) -> impl Future<Output = Result<RebuildReport, StorageError>> + Send {
        let result = (|| {
            let mut state = self.state.lock().unwrap();
            let mut tables = InMemoryTables::default();
            for row in &state.events {
                let event = self.codec.decode(row.kind, &row.payload)?;
                self.projector.project(&mut tables, row.seq, &event)?;
            }
            state.tables = tables;
            Ok(RebuildReport {
                events: state.events.len() as u64,
                last_seq: state.events.last().map_or(Seq(0), |r| r.seq),
            })
        })();
        async move { result }
    }
}

// ---------- 执行 ----------

/// 永不自行结束的「进程」：只有 stop 能让它结束。
#[derive(Clone, Default)]
struct FakeHandle {
    inner: Arc<(Mutex<Option<ExitStatus>>, Condvar)>,
}

/// 停止时记下的结束信号（SIGTERM）。
const STOP_SIGNAL: i32 = 15;

impl ExecutionHandle for FakeHandle {
    fn stop(&self) -> impl Future<Output = Result<(), ExecError>> + Send {
        let inner = Arc::clone(&self.inner);
        async move {
            let (exit, ended) = &*inner;
            let mut exit = exit.lock().unwrap();
            // 幂等：已结束的保留原结果
            exit.get_or_insert(ExitStatus::Signal(STOP_SIGNAL));
            ended.notify_all();
            Ok(())
        }
    }

    fn wait(&self) -> impl Future<Output = Result<ExitStatus, ExecError>> + Send {
        let inner = Arc::clone(&self.inner);
        async move {
            let (exit, ended) = &*inner;
            let guard = ended
                .wait_while(exit.lock().unwrap(), |exit| exit.is_none())
                .unwrap();
            Ok(guard.expect("wait_while 只在有结果后返回"))
        }
    }
}

struct FakeExecution;

impl ExecutionBackend for FakeExecution {
    type Handle = FakeHandle;
    async fn run_as_user<S: OutputSink + 'static>(
        &self,
        _req: UserExecRequest,
        _out: S,
    ) -> Result<FakeHandle, ExecError> {
        Ok(FakeHandle::default())
    }
    async fn apply_patch_as_user(&self, patch: UserPatchRequest) -> Result<(), ExecError> {
        Err(ExecError::PatchConflict { path: patch.path.0 })
    }
}

struct DiscardOutput;

impl OutputSink for DiscardOutput {
    fn output(&self, _chunk: &[u8]) {}
}

// ---------- 其余端口：最小实现，只为证明签名可实现且 Send ----------

struct FakeAgent;
struct FakeSession;
struct FakeUpdates(Vec<AgentUpdate>);

impl UpdateStream for FakeUpdates {
    fn next(&mut self) -> impl Future<Output = Option<AgentUpdate>> + Send {
        let next = self.0.pop();
        async move { next }
    }
}

impl AgentSession for FakeSession {
    type Updates = FakeUpdates;
    fn runtime_session_id(&self) -> Option<&str> {
        Some("s1")
    }
    fn permission_wait_limit(&self) -> Option<std::time::Duration> {
        None
    }
    fn sandbox(&self) -> iwork_ports::agent::SandboxStatus {
        iwork_ports::agent::SandboxStatus::Unavailable
    }
    async fn terminate(&self) -> Result<(), AgentError> {
        Ok(())
    }
    async fn prompt(&self, _blocks: Vec<ContentBlock>) -> Result<FakeUpdates, AgentError> {
        Ok(FakeUpdates(vec![AgentUpdate::Finished(
            TurnOutcome::Completed,
        )]))
    }
    async fn cancel(&self) -> Result<(), AgentError> {
        Ok(())
    }
    async fn close(self) -> Result<(), AgentError> {
        Ok(())
    }
}

impl AgentPort for FakeAgent {
    type Session = FakeSession;
    async fn capabilities(&self) -> Result<RuntimeCapabilities, AgentError> {
        Ok(RuntimeCapabilities {
            supports_resume: true,
            supports_image_input: false,
            effort_levels: vec![],
            sandbox_platform: iwork_ports::agent::SandboxPlatform::Unsupported,
            model_options: vec![],
        })
    }
    async fn open_session<H: PermissionHandler + 'static>(
        &self,
        _spec: SessionSpec,
        _on_permission: H,
    ) -> Result<FakeSession, AgentError> {
        Ok(FakeSession)
    }
}

struct DenyAll;

impl PermissionHandler for DenyAll {
    async fn on_permission(&self, _req: PermissionRequest) -> PermissionDecision {
        PermissionDecision::Deny {
            message: "测试".into(),
        }
    }
}

struct FakeVcs;

impl VcsProvider for FakeVcs {
    fn capabilities(&self) -> VcsCapabilities {
        VcsCapabilities {
            cheap_isolated_workspace: false,
        }
    }
    async fn detect(&self, _path: &Path) -> Result<Detection, VcsError> {
        Ok(Detection::NotVersionControlled)
    }
    async fn init(&self, _path: &Path) -> Result<(), VcsError> {
        Ok(())
    }
    async fn clone_repository<S: ProgressSink + 'static>(
        &self,
        _url: &str,
        _dest: &Path,
        _progress: S,
    ) -> Result<(), VcsError> {
        Ok(())
    }
    async fn baseline(&self, _repo: &RepoRoot) -> Result<BaselineRef, VcsError> {
        Ok(BaselineRef("b".into()))
    }
    async fn status(&self, _repo: &RepoRoot) -> Result<WorkspaceStatus, VcsError> {
        Ok(WorkspaceStatus::default())
    }
    async fn diff(&self, _repo: &RepoRoot, _range: DiffRange) -> Result<Diff, VcsError> {
        Ok(Diff::default())
    }
}

struct FakeArtifacts;

impl ArtifactStore for FakeArtifacts {
    async fn put_bytes(
        &self,
        area: ArtifactArea,
        data: Vec<u8>,
    ) -> Result<ArtifactRef, ArtifactError> {
        Ok(ArtifactRef {
            sha256: Sha256([0; 32]),
            area,
            size: data.len() as u64,
        })
    }
    fn put_file(
        &self,
        _area: ArtifactArea,
        path: &Path,
    ) -> impl Future<Output = Result<ArtifactRef, ArtifactError>> + Send {
        let path = path.to_path_buf();
        async move {
            Err(ArtifactError::Io {
                path,
                message: "假实现不读文件".into(),
            })
        }
    }
    fn read_range(
        &self,
        r: &ArtifactRef,
        _range: Range<u64>,
    ) -> impl Future<Output = Result<Vec<u8>, ArtifactError>> + Send {
        let sha256 = r.sha256;
        async move { Err(ArtifactError::NotFound { sha256 }) }
    }
    async fn retain(&self, _r: &ArtifactRef) -> Result<(), ArtifactError> {
        Ok(())
    }
    async fn release(&self, _r: &ArtifactRef) -> Result<(), ArtifactError> {
        Ok(())
    }
    async fn collect_garbage(&self) -> Result<GcReport, ArtifactError> {
        Ok(GcReport::default())
    }
}

#[derive(Default)]
struct FakeSecrets(Mutex<HashMap<String, Vec<u8>>>);

impl SecretStore for FakeSecrets {
    fn set(
        &self,
        name: &SecretName,
        value: SecretValue,
    ) -> impl Future<Output = Result<(), SecretError>> + Send {
        self.0
            .lock()
            .unwrap()
            .insert(name.0.clone(), value.expose().to_vec());
        async { Ok(()) }
    }
    fn get(
        &self,
        name: &SecretName,
    ) -> impl Future<Output = Result<Option<SecretValue>, SecretError>> + Send {
        let value = self
            .0
            .lock()
            .unwrap()
            .get(&name.0)
            .cloned()
            .map(SecretValue::new);
        async move { Ok(value) }
    }
    fn delete(&self, name: &SecretName) -> impl Future<Output = Result<(), SecretError>> + Send {
        self.0.lock().unwrap().remove(&name.0);
        async { Ok(()) }
    }
}

struct FakeNotifier;

impl Notifier for FakeNotifier {
    async fn notify(&self, _n: SystemNotification) -> Result<(), NotifyError> {
        Ok(())
    }
}

struct FakeDiscovery;

impl RuntimeDiscovery for FakeDiscovery {
    async fn discover(&self, _kind: &RuntimeKind) -> Result<RuntimeProbe, DiscoveryError> {
        Ok(RuntimeProbe {
            cli_path: None,
            cli_version: None,
            login: None,
        })
    }
}

struct FakeReadModel;

impl ReadModelPort for FakeReadModel {
    async fn conversation(
        &self,
        id: ConversationId,
        _page: MessagePage,
    ) -> Result<ConversationView, StorageError> {
        Err(StorageError::NotFound {
            entity: "conversation",
            id: id.to_string(),
        })
    }
    async fn conversations(
        &self,
        _project: Option<ProjectId>,
    ) -> Result<Vec<ConversationSummary>, StorageError> {
        Ok(vec![])
    }
    async fn projects(&self) -> Result<Vec<ProjectSummary>, StorageError> {
        Ok(vec![])
    }
    async fn assistants(&self) -> Result<Vec<Assistant>, StorageError> {
        Ok(vec![])
    }
    async fn pending_items(&self) -> Result<Vec<PendingItem>, StorageError> {
        Ok(vec![])
    }
    async fn quota(&self) -> Result<Vec<RuntimeQuota>, StorageError> {
        Ok(vec![])
    }
    async fn runtimes(&self) -> Result<Vec<Runtime>, StorageError> {
        Ok(vec![])
    }
    async fn rules(&self, _project: Option<ProjectId>) -> Result<Vec<RuleEntry>, StorageError> {
        Ok(vec![])
    }
    async fn security_lists(&self) -> Result<Vec<RuleEntry>, StorageError> {
        Ok(vec![])
    }
    async fn settings(&self) -> Result<GlobalSettings, StorageError> {
        Ok(GlobalSettings {
            playful_ui: true,
            autostart: false,
            projects_root: PathBuf::from("/work"),
        })
    }
    async fn unsettled(&self) -> Result<Unsettled, StorageError> {
        Ok(Unsettled::default())
    }
    async fn summary_source(
        &self,
        turn: TurnId,
        _line: Option<ProjectId>,
        _recent_limit: u32,
    ) -> Result<SummarySource, StorageError> {
        Err(StorageError::NotFound {
            entity: "turn",
            id: turn.to_string(),
        })
    }
    async fn artifact_is_referenced(&self, _r: &ArtifactRef) -> Result<bool, StorageError> {
        Ok(false)
    }
    async fn turn_work(&self) -> Result<TurnWork, StorageError> {
        Ok(TurnWork {
            head: Seq(0),
            conversations: vec![],
        })
    }
    async fn last_session_binding(
        &self,
        _c: ConversationId,
    ) -> Result<Option<String>, StorageError> {
        Ok(None)
    }
    async fn session_ids_of(&self, _c: ConversationId) -> Result<Vec<String>, StorageError> {
        Ok(vec![])
    }
    async fn quit_counts(&self) -> Result<QuitCounts, StorageError> {
        Ok(QuitCounts::default())
    }
    async fn snapshot_inputs(&self, c: ConversationId) -> Result<SnapshotInputs, StorageError> {
        Err(StorageError::NotFound {
            entity: "conversation",
            id: c.to_string(),
        })
    }
}

// ---------- Send 编译期断言（第 8 节 S6） ----------

fn assert_send<T: Send>() {}
fn assert_send_sync<T: Send + Sync>() {}

/// 编译得过就是通过：每个假实现都能放进多线程运行时，返回的 future 也是 Send。
#[test]
fn every_fake_is_send() {
    assert_send_sync::<FakeClock>();
    assert_send_sync::<MonotonicIdGenerator<'static>>();
    assert_send_sync::<ProjectOnlyCodec>();
    assert_send_sync::<ProjectsProjector>();
    assert_send_sync::<InMemoryStore<ProjectOnlyCodec, ProjectsProjector>>();
    assert_send_sync::<FakeAgent>();
    assert_send_sync::<FakeSession>();
    assert_send::<FakeUpdates>();
    assert_send_sync::<DenyAll>();
    assert_send_sync::<FakeVcs>();
    assert_send_sync::<FakeArtifacts>();
    assert_send_sync::<FakeExecution>();
    assert_send_sync::<FakeHandle>();
    assert_send_sync::<FakeSecrets>();
    assert_send_sync::<FakeNotifier>();
    assert_send_sync::<FakeDiscovery>();
    assert_send_sync::<FakeReadModel>();

    fn future_is_send<F: Future + Send>(_: &F) {}
    let store = InMemoryStore::new(ProjectOnlyCodec, ProjectsProjector::default());
    future_is_send(&store.head_seq());
    future_is_send(&FakeHandle::default().wait());
    future_is_send(&FakeAgent.capabilities());
    future_is_send(&FakeReadModel.projects());
}

/// 假实现确实能被调用（防「只编译不能用」）。
#[test]
fn fakes_are_callable() {
    let secrets = FakeSecrets::default();
    let name = SecretName("mcp.token".into());
    pollster::block_on(secrets.set(&name, SecretValue::new(b"s3cret".to_vec()))).unwrap();
    let got = pollster::block_on(secrets.get(&name)).unwrap().unwrap();
    assert_eq!(got.expose(), b"s3cret");
    assert_eq!(
        format!("{got:?}"),
        "<redacted>",
        "密钥值的 Debug 不能带明文"
    );

    let mut updates = pollster::block_on(FakeSession.prompt(vec![])).unwrap();
    assert_eq!(
        pollster::block_on(updates.next()),
        Some(AgentUpdate::Finished(TurnOutcome::Completed))
    );
    assert_eq!(pollster::block_on(updates.next()), None);
    assert!(matches!(
        pollster::block_on(DenyAll.on_permission(PermissionRequest {
            updates_before: 0,
            tool: iwork_domain::action::ToolRef("t".into()),
            raw_input: iwork_ports::agent::RawToolInput("{}".into()),
            bypass_sandbox: false,
            cwd: None,
        })),
        PermissionDecision::Deny { .. }
    ));
}

// ---------- 行为测试 ----------

/// `StoragePort::commit` 的契约「调用返回时已经入队」（T10a 方案 2.8 第 0 步）：调用之后不推进返回的
/// future、直接丢掉，这一笔照样落盘、查得到回执。真存储上的同名用例在 `iwork-store` 的 `receipt_tests.rs`。
#[test]
fn commit_lands_without_polling_its_future() {
    let store = InMemoryStore::new(ProjectOnlyCodec, ProjectsProjector::default());
    let id = CommandId::from_raw(RawUlid::from_u128(9));
    let event: DomainEvent = ProjectAdded {
        project: demo_project(ProjectId::from_raw(RawUlid::from_u128(42)), "demo"),
    }
    .into();
    drop(store.commit(CommitRequest {
        command_id: id,
        actor: Actor("user".into()),
        events: vec![event],
        outbox: vec![],
    }));
    assert_eq!(
        pollster::block_on(store.command_receipt(id)).unwrap(),
        Some(CommitReceipt {
            first_seq: Seq(1),
            last_seq: Seq(1),
            replayed: true,
        })
    );
    assert_eq!(store.tables().projects.len(), 1);
    // 没提交过的：None
    let other = CommandId::from_raw(RawUlid::from_u128(10));
    assert_eq!(
        pollster::block_on(store.command_receipt(other)).unwrap(),
        None
    );
}

/// 同一非空事件走在线提交与历史重建两条路径：表相同、projector 各调一次、重建不写 outbox。
#[test]
fn store_commit_and_rebuild_project_same_event() {
    let store = InMemoryStore::new(ProjectOnlyCodec, ProjectsProjector::default());
    let project = demo_project(ProjectId::from_raw(RawUlid::from_u128(42)), "demo");
    let event: DomainEvent = ProjectAdded {
        project: project.clone(),
    }
    .into();

    let receipt = pollster::block_on(store.commit(CommitRequest {
        command_id: CommandId::from_raw(RawUlid::from_u128(7)),
        actor: Actor("user".into()),
        events: vec![event.clone()],
        outbox: vec![NewOutboxItem {
            target: iwork_ports::store::OutboxTarget("runtime".into()),
            payload: vec![1],
        }],
    }))
    .unwrap();
    assert_eq!(
        receipt,
        CommitReceipt {
            first_seq: Seq(1),
            last_seq: Seq(1),
            replayed: false
        }
    );

    // 读回来的是解码后的同一个事件
    let stored = pollster::block_on(store.read_events(EventFilter::All, Seq(0), 10)).unwrap();
    assert_eq!(stored.len(), 1);
    assert_eq!(stored[0].event, event);

    let online = store.tables();
    assert_eq!(
        online.projects,
        vec![(Seq(1), project)],
        "projects 表有一行 demo"
    );
    assert_eq!(store.projector.calls.load(Ordering::SeqCst), 1);
    let outbox_before = store.outbox_len();
    assert_eq!(outbox_before, 1);

    store.clear_tables();
    assert_eq!(
        store.tables(),
        InMemoryTables::default(),
        "对照：清空后确实为空"
    );
    let report = pollster::block_on(store.rebuild_projections()).unwrap();
    assert_eq!(
        report,
        RebuildReport {
            events: 1,
            last_seq: Seq(1)
        }
    );

    assert_eq!(store.tables(), online, "重建后的表与在线提交后逐字段相同");
    assert_eq!(
        store.projector.calls.load(Ordering::SeqCst),
        2,
        "在线一次 + 重建一次，同一个 projector"
    );
    assert_eq!(store.outbox_len(), outbox_before, "重建不写 outbox");

    // 同一 command_id 重发：不写任何东西，返回上次的序号
    let replay = pollster::block_on(store.commit(CommitRequest {
        command_id: CommandId::from_raw(RawUlid::from_u128(7)),
        actor: Actor("user".into()),
        events: vec![event],
        outbox: vec![],
    }))
    .unwrap();
    assert!(replay.replayed);
    assert_eq!((replay.first_seq, replay.last_seq), (Seq(1), Seq(1)));
    assert_eq!(pollster::block_on(store.head_seq()).unwrap(), Seq(1));
}

/// 一个线程在等，另一个克隆上停止：等待随之返回被信号结束。
#[test]
fn exec_stop_while_waiting_ends_wait() {
    let handle = pollster::block_on(FakeExecution.run_as_user(
        UserExecRequest {
            card_id: iwork_domain::id::UserActionId::from_raw(RawUlid::from_u128(1)),
            command: "sleep forever".into(),
            cwd: iwork_domain::action::CanonicalPath("/".into()),
        },
        DiscardOutput,
    ))
    .unwrap();
    let waiter = {
        let h1 = handle.clone();
        std::thread::spawn(move || pollster::block_on(h1.wait()))
    };
    // 对照：停止之前等待者确实还没结束
    std::thread::sleep(std::time::Duration::from_millis(50));
    assert!(
        !waiter.is_finished(),
        "进程不会自行结束，等待者此时应仍在等"
    );

    let h2 = handle.clone();
    pollster::block_on(h2.stop()).unwrap();
    assert_eq!(waiter.join().unwrap(), Ok(ExitStatus::Signal(STOP_SIGNAL)));
}

/// 结束后再等直接返回同一结果；再停止是幂等的。
#[test]
fn exec_wait_twice_same_result() {
    let handle = FakeHandle::default();
    pollster::block_on(handle.stop()).unwrap();
    let first = pollster::block_on(handle.wait());
    let second = pollster::block_on(handle.clone().wait());
    assert_eq!(first, Ok(ExitStatus::Signal(STOP_SIGNAL)));
    assert_eq!(second, first);
    assert_eq!(pollster::block_on(handle.stop()), Ok(()));
    assert_eq!(pollster::block_on(handle.wait()), first, "再停止不改结果");
}

/// 同一毫秒、随机部分用尽、时钟回拨三种情况下，生成的 ID 仍严格递增。
#[test]
fn id_generator_monotonic_within_ms() {
    let clock = FakeClock {
        now_ms: AtomicU64::new(5_000),
    };
    let max_random = (1u128 << 80) - 1;
    let ids = MonotonicIdGenerator {
        clock: &clock,
        last: Mutex::new(None),
        fresh_random: max_random - 2,
    };

    let mut generated = Vec::new();
    // 同一毫秒内连续生成，随机部分从「最大值 - 2」起，第 4 个用尽进到下一毫秒
    for _ in 0..5 {
        generated.push(ids.next());
    }
    assert_eq!(generated[0].timestamp_ms(), 5_000);
    assert_eq!(generated[2].random(), max_random);
    assert_eq!(
        generated[3].timestamp_ms(),
        5_001,
        "随机部分用尽进到下一毫秒"
    );
    // 时钟回拨到 4_000：沿用上一次的毫秒数继续递增
    clock.now_ms.store(4_000, Ordering::SeqCst);
    for _ in 0..3 {
        generated.push(ids.next());
    }
    assert!(generated.iter().all(|id| id.timestamp_ms() >= 5_000));
    // 时钟前进：用新的毫秒数
    clock.now_ms.store(9_000, Ordering::SeqCst);
    generated.push(ids.next());
    assert_eq!(generated.last().unwrap().timestamp_ms(), 9_000);

    for pair in generated.windows(2) {
        assert!(pair[0] < pair[1], "{:?} 不小于 {:?}", pair[0], pair[1]);
    }
    // 文本顺序与数值顺序一致：按字符串排序也是创建顺序
    let texts: Vec<String> = generated
        .iter()
        .map(|raw| ProjectId::from_raw(*raw).to_string())
        .collect();
    let mut sorted = texts.clone();
    sorted.sort();
    assert_eq!(texts, sorted);
}
