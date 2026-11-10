//! 测试夹具：假时钟、ID 生成器、临时数据目录、实体构造、逐表导出。
//! 单测（`src/lib.rs` 经 `#[path]` 引入）与集成测试（`tests/support/mod.rs`）共用这一份。
#![allow(dead_code)]

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicI64, AtomicU64, Ordering};

use iwork_app::codec::ProtoEventCodec;
use iwork_domain::action::{Action, ActionFlags, ActionKind, ActionTarget, CanonicalPath, ToolRef};
use iwork_domain::approval::{Approval, ApprovalKind, ApprovalState, ReviewState};
use iwork_domain::approval_mode::{ProjectDefaultMode, RememberScope};
use iwork_domain::assistant::{Assistant, AssistantConfig, AssistantVersion, Avatar};
use iwork_domain::attachment::{Attachment, AttachmentOrigin};
use iwork_domain::common::{
    Actor, ArtifactArea, ArtifactRef, ModelId, ModelSelection, Sha256, Timestamp,
};
use iwork_domain::conversation::{Conversation, ConversationKind, ConversationState};
use iwork_domain::event::DomainEvent;
use iwork_domain::id::*;
use iwork_domain::message::{Author, Message, MessageBody, MessageStatus};
use iwork_domain::project::{PathStatus, Project, ProjectSettings, VcsKind};
use iwork_domain::rule::{
    RuleEffect, RuleEntry, RuleLayer, RuleMatcher, RuleScope, RuleSetVersion, RuleSource,
};
use iwork_domain::runtime::{LoginInfo, Runtime, RuntimeKind, RuntimeState};
use iwork_domain::settings::GlobalSettings;
use iwork_domain::turn::{Turn, TurnKind, TurnState};
use iwork_domain::user_action::{Proposal, UserActionCard, UserActionState};
use iwork_ports::clock::{Clock, IdGenerator, MonotonicInstant};
use iwork_ports::store::{CommitRequest, NewOutboxItem, OutboxTarget};
use iwork_store::tables::PROJECTION_TABLES;
use iwork_store::{
    DataDir, FsArtifactStore, OpenOptions, ReadDefaults, SqliteProjector, SqliteReadModel,
    SqliteStore, StoreConfig,
};

/// 起点：2026-11-10T00:00:00Z。
pub const T0: i64 = 1_794_268_800_000;

/// 可拨的时钟。
#[derive(Debug)]
pub struct FakeClock(AtomicI64);

impl FakeClock {
    pub fn new(ms: i64) -> Self {
        Self(AtomicI64::new(ms))
    }
    pub fn set(&self, ms: i64) {
        self.0.store(ms, Ordering::SeqCst);
    }
    pub fn advance(&self, ms: i64) {
        self.0.fetch_add(ms, Ordering::SeqCst);
    }
}

impl Clock for FakeClock {
    fn now(&self) -> Timestamp {
        Timestamp(self.0.load(Ordering::SeqCst))
    }
    fn monotonic(&self) -> MonotonicInstant {
        MonotonicInstant(self.0.load(Ordering::SeqCst) as u64 * 1_000_000)
    }
}

/// 递增的 ID 生成器（满足单调契约）。
#[derive(Debug, Default)]
pub struct CountingIds(AtomicU64);

impl IdGenerator for CountingIds {
    fn next(&self) -> RawUlid {
        RawUlid::from_parts(T0 as u64, u128::from(self.0.fetch_add(1, Ordering::SeqCst)))
    }
}

pub type Store = SqliteStore<ProtoEventCodec>;
pub type Artifacts = FsArtifactStore<CountingIds, FakeClock>;

/// 一个临时数据目录（系统临时目录下，测试结束自动删）+ 假时钟。
pub struct Env {
    pub tmp: tempfile::TempDir,
    pub dir: DataDir,
    pub clock: Arc<FakeClock>,
}

impl Env {
    pub fn new() -> Self {
        let tmp = tempfile::tempdir().expect("建临时目录");
        let dir = DataDir::from_root(tmp.path().join("data"));
        Self {
            tmp,
            dir,
            clock: Arc::new(FakeClock::new(T0)),
        }
    }

    pub fn config() -> StoreConfig {
        StoreConfig {
            // 测试不关心断电，关掉 fullfsync 省时间；基准两种都量
            fullfsync: Some(false),
            ..StoreConfig::default()
        }
    }

    pub fn open(&self) -> Store {
        self.open_with(Self::config(), OpenOptions::default())
            .expect("打开存储")
    }

    pub fn open_with(
        &self,
        config: StoreConfig,
        options: OpenOptions,
    ) -> Result<Store, iwork_ports::store::StorageError> {
        let codec = Arc::new(ProtoEventCodec);
        let projector = Arc::new(SqliteProjector::new(codec.clone()));
        SqliteStore::open_with(
            &self.dir,
            codec,
            projector,
            self.clock.clone(),
            config,
            options,
        )
    }

    /// 用自定义投影器打开（测试里包一层注入失败或计数）。
    pub fn open_projector<P>(&self, projector: Arc<P>, config: StoreConfig) -> Store
    where
        P: iwork_ports::store::Projector<Tx = iwork_store::WriteTx> + 'static,
    {
        SqliteStore::open_with(
            &self.dir,
            Arc::new(ProtoEventCodec),
            projector,
            self.clock.clone(),
            config,
            OpenOptions::default(),
        )
        .expect("打开存储")
    }

    pub fn artifacts(&self, store: &Store) -> Artifacts {
        FsArtifactStore::new(store, Arc::new(CountingIds::default()), self.clock.clone())
            .expect("起制品存储")
    }

    pub fn db(&self) -> PathBuf {
        self.dir.db_path()
    }
}

pub fn read_model(store: &Store) -> SqliteReadModel<ProtoEventCodec> {
    store.read_model(ReadDefaults {
        global_settings: GlobalSettings {
            playful_ui: true,
            autostart: false,
            projects_root: PathBuf::from("/home/demo/iWork/projects"),
        },
    })
}

pub fn block<F: std::future::Future>(f: F) -> F::Output {
    pollster::block_on(f)
}

// ---------- ID ----------

fn raw(n: u128) -> RawUlid {
    RawUlid::from_u128(n)
}
pub fn pid(n: u128) -> ProjectId {
    ProjectId::from_raw(raw(n))
}
pub fn aid(n: u128) -> AssistantId {
    AssistantId::from_raw(raw(n))
}
pub fn cid(n: u128) -> ConversationId {
    ConversationId::from_raw(raw(n))
}
pub fn mid(n: u128) -> MessageId {
    MessageId::from_raw(raw(n))
}
pub fn tid(n: u128) -> TurnId {
    TurnId::from_raw(raw(n))
}
pub fn apid(n: u128) -> ApprovalId {
    ApprovalId::from_raw(raw(n))
}
pub fn uaid(n: u128) -> UserActionId {
    UserActionId::from_raw(raw(n))
}
pub fn rid(n: u128) -> RuleId {
    RuleId::from_raw(raw(n))
}
pub fn atid(n: u128) -> AttachmentId {
    AttachmentId::from_raw(raw(n))
}
pub fn rtid(n: u128) -> RuntimeId {
    RuntimeId::from_raw(raw(n))
}
pub fn jid(n: u128) -> JobId {
    JobId::from_raw(raw(n))
}
pub fn cmd_id(n: u128) -> CommandId {
    CommandId::from_raw(raw(n))
}

/// 一条命令，事件全部由用户发起。
pub fn cmd(n: u128, events: Vec<DomainEvent>) -> CommitRequest {
    CommitRequest {
        command_id: cmd_id(n),
        actor: Actor("user".into()),
        events,
        outbox: vec![],
    }
}

pub fn outbox(target: &str, payload: &[u8]) -> NewOutboxItem {
    NewOutboxItem {
        target: OutboxTarget(target.into()),
        payload: payload.to_vec(),
    }
}

// ---------- 实体 ----------

pub fn model() -> ModelSelection {
    ModelSelection {
        model_id: ModelId("m1".into()),
        effort: None,
    }
}

pub fn project(n: u128) -> Project {
    Project {
        id: pid(n),
        root_path: PathBuf::from(format!("/work/p{n}")),
        vcs_kind: VcsKind("v".into()),
        name: format!("p{n}"),
        pinned: false,
        last_active_at: None,
        path_status: PathStatus::Ok,
        settings: ProjectSettings {
            default_approval_mode: ProjectDefaultMode::Manual,
            ..ProjectSettings::default()
        },
    }
}

pub fn assistant(n: u128, builtin: bool) -> Assistant {
    Assistant {
        id: aid(n),
        builtin,
        current: AssistantVersion {
            version: 1,
            config: AssistantConfig {
                name: format!("a{n}"),
                avatar: Avatar {
                    initial: "A".into(),
                    color: "#123456".into(),
                },
                prompt: "p".into(),
                runtime_id: rtid(1),
                model: model(),
                disabled_groups: BTreeSet::new(),
            },
            created_at: Timestamp(T0),
        },
    }
}

pub fn project_conversation(n: u128, project: u128) -> Conversation {
    Conversation {
        id: cid(n),
        kind: ConversationKind::Project {
            project_id: pid(project),
        },
        state: ConversationState::Active,
        last_read_seq: Seq(0),
        model_override: None,
        approval_mode: None,
        updated_at: Timestamp(T0),
    }
}

pub fn dm(n: u128, assistant: u128) -> Conversation {
    Conversation {
        id: cid(n),
        kind: ConversationKind::AssistantDm {
            assistant_id: aid(assistant),
            context_project: None,
        },
        state: ConversationState::Active,
        last_read_seq: Seq(0),
        model_override: None,
        approval_mode: None,
        updated_at: Timestamp(T0),
    }
}

pub fn user_message(n: u128, conv: u128, text: &str) -> Message {
    Message {
        id: mid(n),
        conversation_id: cid(conv),
        author: Author::User,
        context_project_id: None,
        status: MessageStatus::Sent,
        body: MessageBody::Inline(text.into()),
        attachments: vec![],
        created_at: Timestamp(T0),
        turn_id: None,
    }
}

pub fn assistant_message(n: u128, conv: u128, turn: u128) -> Message {
    Message {
        id: mid(n),
        conversation_id: cid(conv),
        author: Author::Assistant {
            id: aid(1),
            name_snapshot: "iWork".into(),
        },
        context_project_id: None,
        status: MessageStatus::Streaming,
        body: MessageBody::Inline(String::new()),
        attachments: vec![],
        created_at: Timestamp(T0),
        turn_id: Some(tid(turn)),
    }
}

pub fn turn(n: u128, conv: u128) -> Turn {
    Turn {
        id: tid(n),
        conversation_id: cid(conv),
        assistant_id: aid(1),
        assistant_version: 1,
        runtime_id: rtid(1),
        model: model(),
        runtime_session_id: None,
        kind: TurnKind::Assistant,
        state: TurnState::Queued,
        started_at: Timestamp(T0),
        ended_at: None,
    }
}

pub fn approval(n: u128, turn: u128, conv: u128) -> Approval {
    Approval {
        id: apid(n),
        turn_id: tid(turn),
        conversation_id: cid(conv),
        kind: ApprovalKind::Ask,
        action: Action {
            kind: ActionKind::Execute,
            target: ActionTarget::Command {
                program: "npm".into(),
                subcommand: Some("test".into()),
                argv: vec!["npm".into(), "test".into()],
            },
            tool: ToolRef("Bash".into()),
            raw_summary: "npm test".into(),
            flags: ActionFlags::default(),
        },
        matched_rules: vec![],
        ruleset_version: RuleSetVersion("r1".into()),
        state: ApprovalState::Pending,
        review: ReviewState::NotReviewed,
        offered_remember: vec![RememberScope::Once],
        remember_label: "本次".into(),
        requested_at: Timestamp(T0),
    }
}

pub fn card(n: u128, turn: u128, conv: u128) -> UserActionCard {
    UserActionCard {
        id: uaid(n),
        turn_id: tid(turn),
        conversation_id: cid(conv),
        proposal: Proposal::Command {
            command: "rm -rf .git/hooks/x".into(),
            cwd: CanonicalPath(PathBuf::from("/work/p1")),
        },
        reason: "底线".into(),
        proposal_hash: Sha256([7; 32]),
        state: UserActionState::Pending,
        output_artifact: None,
    }
}

pub fn rule(n: u128, layer: RuleLayer, scope: RuleScope) -> RuleEntry {
    RuleEntry {
        id: rid(n),
        layer,
        matcher: RuleMatcher::PathUnder(PathBuf::from("/secret")),
        effect: RuleEffect::Deny,
        source: RuleSource::User,
        scope,
        created_at: Timestamp(T0),
        label: format!("r{n}"),
    }
}

pub fn attachment(n: u128, conv: u128, origin: AttachmentOrigin) -> Attachment {
    Attachment {
        id: atid(n),
        conversation_id: cid(conv),
        sha256: Sha256([9; 32]),
        size: 3,
        mime: "text/plain".into(),
        name: "a.txt".into(),
        origin,
        needs_confirmation: false,
        created_at: Timestamp(T0),
    }
}

pub fn runtime(n: u128) -> Runtime {
    Runtime {
        id: rtid(n),
        kind: RuntimeKind("cli".into()),
        display_name: "CLI".into(),
        cli_path: Some(PathBuf::from("/usr/local/bin/cli")),
        cli_version: Some("1.0".into()),
        adapter_version: "0.1".into(),
        state: RuntimeState::ReadyVerified,
        login: Some(LoginInfo {
            logged_in: true,
            auth_method: "oauth".into(),
            subscription_type: "pro".into(),
            login_expires_at: None,
        }),
    }
}

pub fn artifact(byte: u8, area: ArtifactArea) -> ArtifactRef {
    ArtifactRef {
        sha256: Sha256([byte; 32]),
        area,
        size: 10,
    }
}

// ---------- 逐表导出 ----------

/// 一张表按主键排序的全部行（每行渲染成文本，比较时逐字段）。
pub fn dump_table(db: &Path, table: &str, order_by: &str) -> Vec<String> {
    let conn =
        rusqlite::Connection::open_with_flags(db, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)
            .expect("打开只读连接");
    let mut stmt = conn
        .prepare(&format!("SELECT * FROM {table} ORDER BY {order_by}"))
        .unwrap();
    let n = stmt.column_count();
    stmt.query_map([], |r| {
        let vals: Vec<rusqlite::types::Value> =
            (0..n).map(|i| r.get(i)).collect::<Result<_, _>>()?;
        Ok(format!("{vals:?}"))
    })
    .unwrap()
    .collect::<Result<_, _>>()
    .unwrap()
}

/// 全部投影表，按登记表的顺序。
pub fn dump_projections(db: &Path) -> Vec<(&'static str, Vec<String>)> {
    PROJECTION_TABLES
        .iter()
        .map(|t| (t.name, dump_table(db, t.name, &t.key.join(", "))))
        .collect()
}

/// 查一个整数。
pub fn scalar(db: &Path, sql: &str) -> i64 {
    let conn =
        rusqlite::Connection::open_with_flags(db, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)
            .unwrap();
    conn.query_row(sql, [], |r| r.get(0)).unwrap()
}
