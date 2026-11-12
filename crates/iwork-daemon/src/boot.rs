//! 启动顺序（T04 方案 2.4）。一张有序的表：每一步有名字（写进状态文件的 `Starting{step}`）、
//! 失败时落到 2.2 启动失败表的哪一格。顺序不能换，理由在方案 2.4 的右列。
//!
//! 失败落到哪一格只由 [`BootError`] 的变体决定（[`BootError::code`]、[`BootError::status`]），
//! 各步只负责把自己的错误归到变体上，不另做决定。

use std::ffi::OsString;
use std::future::Future;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Instant;

use iwork_app::codec::ProtoEventCodec;
use iwork_domain::id::{Seq, TurnId};
use iwork_domain::settings::GlobalSettings;
use iwork_ipc::peer::{self, ImageId, SelfIdentity};
use iwork_ipc::rendezvous::{ExeStamp, ExitCode, Phase, Refusal};
use iwork_ports::clock::IdGenerator;
use iwork_ports::read_model::{ReadModelPort, Unsettled};
use iwork_ports::store::{
    CodecError, CommitReceipt, CommitRequest, EventFilter, StorageError, StoragePort, StoredEvent,
};
use iwork_store::{
    DataDir, FsArtifactStore, ReadDefaults, SqliteProjector, SqliteReadModel, SqliteStore,
    StoreConfig,
};

#[cfg(not(unix))]
use crate::args::DaemonArgs;
use crate::dev;
use crate::exit::{exited, process_status};
use crate::time::{MonotonicIds, SystemClock};

/// 后台用的存储类型。
pub type Store = SqliteStore<ProtoEventCodec>;

/// 开发构建的暂停点：状态已写定、锁还没放（见 [`dev`]）。
pub const PAUSE_BEFORE_EXIT: &str = "before-exit";

/// M1 在 Windows 上启动即拒绝时的说明（方案 2.15）。
pub const WINDOWS_UNSUPPORTED: &str =
    "M1 暂不支持 Windows：对端映像校验未通过实测（t0-findings 6.4）";

/// 本程序的版本（写进锁文件与状态文件）。
pub const VERSION: &str = env!("CARGO_PKG_VERSION");

/// 启动表的步骤，按执行顺序。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Step {
    Identity,
    Endpoint,
    Lock,
    StatusLog,
    Store,
    Compat,
    Artifacts,
    DeliveryGate,
    Recovery,
    Server,
    AutostartOff,
    Bind,
    Tasks,
    Ready,
}

impl Step {
    pub const ALL: [Step; 14] = [
        Step::Identity,
        Step::Endpoint,
        Step::Lock,
        Step::StatusLog,
        Step::Store,
        Step::Compat,
        Step::Artifacts,
        Step::DeliveryGate,
        Step::Recovery,
        Step::Server,
        Step::AutostartOff,
        Step::Bind,
        Step::Tasks,
        Step::Ready,
    ];

    /// 写进状态文件的步骤名；也是开发构建故障注入点的点名。
    pub fn name(self) -> &'static str {
        match self {
            Step::Identity => "identity",
            Step::Endpoint => "endpoint",
            Step::Lock => "lock",
            Step::StatusLog => "status+log",
            Step::Store => "store",
            Step::Compat => "compat",
            Step::Artifacts => "artifacts",
            Step::DeliveryGate => "delivery-gate",
            Step::Recovery => "recovery",
            Step::Server => "server",
            Step::AutostartOff => "autostart-off",
            Step::Bind => "bind",
            Step::Tasks => "tasks",
            Step::Ready => "ready",
        }
    }
}

/// 启动失败。变体就是 2.2 启动失败表里的格子。
#[derive(Debug, Clone, PartialEq)]
pub enum BootError {
    /// 第 1 步：算不出自己的身份（含运行映像身份）。不写状态。
    Identity(String),
    /// 第 2 步：端点或数据目录不可用。不写状态。
    Endpoint(String),
    /// 第 5 步存储拒绝打开；第 5a、9 步读到不认识的事件种类。写 `Refused`。
    Refused(Refusal),
    /// 第 10 步：拿到锁却绑不上 socket。写 `Exited{13}`（可重试，不是 `Refused`）。
    Bind(String),
    /// 第 5a、6、8、9、11 步的其他失败。写 `Exited{20}`。
    Fatal { step: Step, message: String },
}

impl BootError {
    pub fn code(&self) -> ExitCode {
        match self {
            BootError::Identity(_) => ExitCode::Unsupported,
            BootError::Endpoint(_) => ExitCode::EndpointRefused,
            BootError::Refused(_) => ExitCode::StoreRefused,
            BootError::Bind(_) => ExitCode::BindFailed,
            BootError::Fatal { .. } => ExitCode::Fatal,
        }
    }

    /// 要写进状态文件的阶段；还没拿锁就失败的（不是状态文件的主人）为 `None`。
    pub fn status(&self) -> Option<Phase> {
        match self {
            BootError::Identity(_) | BootError::Endpoint(_) => None,
            BootError::Refused(r) => Some(Phase::Refused(r.clone())),
            BootError::Bind(message) => Some(Phase::Exited {
                code: ExitCode::BindFailed,
                reason: None,
                message: Some(message.clone()),
            }),
            BootError::Fatal { step, message } => Some(Phase::Exited {
                code: ExitCode::Fatal,
                reason: None,
                message: Some(format!("{}：{message}", step.name())),
            }),
        }
    }

    /// 算进程退出状态用的阶段：写了状态的就是那一份，没写的对本地构造的 `Exited` 算（同一个函数）。
    pub fn exit_phase(&self) -> Phase {
        self.status().unwrap_or_else(|| exited(self.code()))
    }

    fn message(&self) -> String {
        match self {
            BootError::Identity(m) | BootError::Endpoint(m) | BootError::Bind(m) => m.clone(),
            BootError::Refused(r) => format!("{r:?}"),
            BootError::Fatal { step, message } => format!("{}：{message}", step.name()),
        }
    }
}

/// 第 5 步：存储打开失败一律是「这一版起不来」（协议门、schema 比程序新、不是 iWork 库、迁移失败）。
pub fn store_open_error(e: StorageError) -> BootError {
    match e {
        StorageError::ProtocolMajorMismatch {
            database,
            program,
            backup,
        } => BootError::Refused(Refusal::ProtocolMajorMismatch {
            database,
            program,
            backup,
        }),
        other => BootError::Refused(Refusal::StoreOpenFailed {
            message: other.to_string(),
        }),
    }
}

/// 第 5a、9 步读事件出错：只有「不认识的事件种类」是「这一版读不了这个库」（T07 PD6），其余是可重试的故障。
pub fn read_error(step: Step, e: StorageError) -> BootError {
    match e {
        StorageError::Codec(CodecError::UnknownKind { kind }) => {
            BootError::Refused(Refusal::StoreOpenFailed {
                message: format!(
                    "库里有本版本不认识的事件种类 {kind}：这个库{}版本写过，装回更新的那一版可继续使用",
                    iwork_ipc::rendezvous::WRITTEN_BY_NEWER
                ),
            })
        }
        other => BootError::Fatal {
            step,
            message: other.to_string(),
        },
    }
}

/// 第 1 步的结果：进程的身份与启动快照。
pub struct Identity {
    pub own: SelfIdentity,
    /// 运行映像的身份，启动时取一次，之后只读（方案 2.9）。
    pub running_image: ImageId,
    /// 本进程的可执行文件路径与它在启动那一刻的戳。
    pub exe: PathBuf,
    pub exe_stamp: ExeStamp,
    /// 第 1 步开始时的单调读数：自行重启的冷却从它算起。
    pub started: Instant,
}

fn identify(started: Instant) -> Result<Identity, BootError> {
    // M1 不支持 Windows：对端映像校验没通过实测（方案 2.15）。错误原文前面点明这一句，stderr 与诊断都看得到
    let unsupported = |m: String| {
        if cfg!(windows) {
            BootError::Identity(format!("{WINDOWS_UNSUPPORTED}（{m}）"))
        } else {
            BootError::Identity(m)
        }
    };
    if dev::fail_at(Step::Identity.name()) {
        return Err(unsupported("开发注入：取运行映像身份失败".into()));
    }
    let own = SelfIdentity::detect().map_err(|e| unsupported(e.to_string()))?;
    let running_image =
        peer::running_image_id().map_err(|e| unsupported(format!("取运行映像身份失败：{e}")))?;
    let exe = std::env::current_exe()
        .map_err(|e| unsupported(format!("取不到本进程的可执行文件路径：{e}")))?;
    let exe_stamp = ExeStamp::of(&exe)
        .map_err(|e| unsupported(format!("读 {} 的元数据失败：{e}", exe.display())))?;
    Ok(Identity {
        own,
        running_image,
        exe,
        exe_stamp,
        started,
    })
}

pub(crate) fn report(err: &BootError) {
    eprintln!(
        "iwork-daemon：启动失败（逻辑退出码 {}）：{}",
        err.code().code(),
        err.message()
    );
}

pub(crate) fn status_of(code: u8) -> std::process::ExitCode {
    std::process::ExitCode::from(code)
}

/// 进程入口：解析参数 → 启动表 → 运行 → 退出状态。
pub fn run(raw: Vec<OsString>) -> std::process::ExitCode {
    let managed = crate::args::is_service(&raw);
    let args = match crate::args::parse(raw.clone()) {
        Ok(a) => a,
        Err(e) => {
            eprintln!("iwork-daemon：{e}，拒绝启动");
            return status_of(process_status(&exited(ExitCode::Usage), managed));
        }
    };
    #[cfg(feature = "dev")]
    if args.trust == iwork_ipc::peer::TrustPolicy::DevTrust {
        eprintln!("iwork-daemon：--dev-trust 已开启，跳过签名校验（同用户照查），只用于开发");
    }
    let started = Instant::now();
    let identity = match identify(started) {
        Ok(i) => i,
        Err(e) => {
            report(&e);
            return status_of(process_status(&e.exit_phase(), managed));
        }
    };
    #[cfg(unix)]
    return crate::daemon::run_identified(args, raw, identity);
    #[cfg(not(unix))]
    return run_identified(args, raw, identity);
}

#[cfg(not(unix))]
fn run_identified(args: DaemonArgs, _raw: Vec<OsString>, _id: Identity) -> std::process::ExitCode {
    // M1 不支持 Windows：`SelfIdentity::detect` 在第 1 步就失败，走不到这里（方案 2.15）
    let e = BootError::Identity("M1 暂不支持 Windows".into());
    report(&e);
    status_of(process_status(&e.exit_phase(), args.service))
}

/// 启动表第 5a–8 步要用的存储操作。真实现是 [`StorePorts`]；测试用记录调用次序的假实现
/// 证明「5a 在任何写入之前」（方案 S8 `compat_runs_before_any_write`）。
pub trait BootPorts: Sync {
    fn head_seq(&self) -> impl Future<Output = Result<Seq, StorageError>> + Send;
    fn read_events(
        &self,
        from: Seq,
        limit: usize,
    ) -> impl Future<Output = Result<Vec<StoredEvent>, StorageError>> + Send;
    /// 制品残留清理（写：删文件、删登记行）。
    fn recover_artifacts(&self) -> impl Future<Output = Result<(), String>> + Send;
    fn mark_delivery_started(&self);
    fn unsettled(&self) -> impl Future<Output = Result<Unsettled, StorageError>> + Send;
    fn commit(
        &self,
        req: CommitRequest,
    ) -> impl Future<Output = Result<CommitReceipt, StorageError>> + Send;
}

/// 后台用的 ID 生成器与制品存储类型。
pub type Ids = MonotonicIds<SystemClock>;
pub type Artifacts = FsArtifactStore<Ids, SystemClock>;

/// 真实的启动端口：存储、它的读模型与制品存储。
pub struct StorePorts {
    pub store: Arc<Store>,
    pub read: SqliteReadModel<ProtoEventCodec>,
    pub artifacts: Artifacts,
}

impl BootPorts for StorePorts {
    fn head_seq(&self) -> impl Future<Output = Result<Seq, StorageError>> + Send {
        self.store.head_seq()
    }
    fn read_events(
        &self,
        from: Seq,
        limit: usize,
    ) -> impl Future<Output = Result<Vec<StoredEvent>, StorageError>> + Send {
        self.store.read_events(EventFilter::All, from, limit)
    }
    async fn recover_artifacts(&self) -> Result<(), String> {
        let report = self.artifacts.recover().await.map_err(|e| e.to_string())?;
        tracing::info!(?report, "制品残留清理完成");
        Ok(())
    }
    fn mark_delivery_started(&self) {
        self.store.mark_delivery_started();
    }
    fn unsettled(&self) -> impl Future<Output = Result<Unsettled, StorageError>> + Send {
        self.read.unsettled()
    }
    fn commit(
        &self,
        req: CommitRequest,
    ) -> impl Future<Output = Result<CommitReceipt, StorageError>> + Send {
        self.store.commit(req)
    }
}

/// 第 5a 步翻事件时一页多少条。
const COMPAT_PAGE: usize = 1000;

/// 第 5a 步：**只读**，把已有事件从头读一遍、逐条解码，结果丢掉。库里有本版本不认识的事件种类就在这里拒绝——
/// 它之前本进程没有做过任何逻辑写入，之后才轮到制品恢复与崩溃恢复去写（方案 2.4、S8）。
pub async fn compat(ports: &impl BootPorts) -> Result<u64, BootError> {
    let head = ports
        .head_seq()
        .await
        .map_err(|e| read_error(Step::Compat, e))?;
    let mut cursor = Seq(0);
    let mut read = 0u64;
    while cursor < head {
        let page = ports
            .read_events(cursor, COMPAT_PAGE)
            .await
            .map_err(|e| read_error(Step::Compat, e))?;
        let Some(last) = page.last() else { break };
        cursor = last.seq;
        read += page.len() as u64;
    }
    Ok(read)
}

/// 第 5a、6、7、8 步，按这个顺序。`enter` 在每一步开始时调（写 `Starting{step}`；开发构建返回 `false` 表示注入失败）。
/// 返回被标成「已中断」的回合。
pub async fn prepare(
    ports: &impl BootPorts,
    ids: &impl IdGenerator,
    mut enter: impl FnMut(Step) -> bool,
) -> Result<Vec<TurnId>, BootError> {
    if !enter(Step::Compat) {
        return Err(injected(Step::Compat));
    }
    compat(ports).await?;
    if !enter(Step::Artifacts) {
        return Err(injected(Step::Artifacts));
    }
    ports
        .recover_artifacts()
        .await
        .map_err(|message| BootError::Fatal {
            step: Step::Artifacts,
            message,
        })?;
    // 第 7 步：这之后才允许恢复写事件、才允许投递（T02 流程）。没有失败可言
    enter(Step::DeliveryGate);
    ports.mark_delivery_started();
    if !enter(Step::Recovery) {
        return Err(injected(Step::Recovery));
    }
    crate::recovery::recover(ports, ids).await
}

/// 启动表第 9 步里、提交观察者装上之后、`bind` 之前做自己恢复的参与方（T10 接回合宿主与审批引擎的 `recover`）。
/// 返回的错误按 [`read_error`] 归类：不认识的事件种类必须原样传上来（归 12），不能包成别的错误。
pub trait StartupParticipant: Send + Sync {
    fn start<'a>(
        &'a self,
        store: &'a Store,
    ) -> std::pin::Pin<Box<dyn Future<Output = Result<(), StorageError>> + Send + 'a>>;
}

/// 当前的参与方。T04 交付时没有；开发构建可经环境变量加一个假的（证明这个位置上的提交在 `bind` 之前落盘、
/// 且不被重复发布）。
#[cfg(not(feature = "dev"))]
pub fn startup_participants() -> Vec<Box<dyn StartupParticipant>> {
    Vec::new()
}

#[cfg(feature = "dev")]
pub fn startup_participants() -> Vec<Box<dyn StartupParticipant>> {
    if std::env::var(dev_participant::ENV).is_ok() {
        vec![Box::new(dev_participant::CommitOne)]
    } else {
        Vec::new()
    }
}

#[cfg(feature = "dev")]
mod dev_participant {
    use super::*;
    use iwork_domain::common::Actor;
    use iwork_domain::event::GlobalSettingsChanged;
    use iwork_domain::id::CommandId;
    use iwork_ports::clock::IdGenerator;

    pub const ENV: &str = "IWORK_DEV_STARTUP_COMMIT";

    /// 提交一条设置变更（`playful_ui = false`）。
    pub struct CommitOne;

    impl StartupParticipant for CommitOne {
        fn start<'a>(
            &'a self,
            store: &'a Store,
        ) -> std::pin::Pin<Box<dyn Future<Output = Result<(), StorageError>> + Send + 'a>> {
            Box::pin(async move {
                let ids = MonotonicIds::new(SystemClock::new());
                store
                    .commit(CommitRequest {
                        command_id: CommandId::from_raw(ids.next()),
                        actor: Actor("system:dev-participant".into()),
                        events: vec![
                            GlobalSettingsChanged {
                                playful_ui: Some(false),
                                autostart: None,
                                projects_root: None,
                            }
                            .into(),
                        ],
                        outbox: vec![],
                    })
                    .await
                    .map(|_| ())
            })
        }
    }
}

/// 开发构建注入的失败（归到这一步的 `Fatal`）。
pub fn injected(step: Step) -> BootError {
    BootError::Fatal {
        step,
        message: "开发注入的失败".into(),
    }
}

pub fn starting(step: Step) -> Phase {
    Phase::Starting {
        step: step.name().into(),
    }
}

/// 第 5 步：打开存储。
pub fn open_store(data: &DataDir, clock: Arc<SystemClock>) -> Result<Arc<Store>, BootError> {
    if dev::fail_at(Step::Store.name()) {
        return Err(store_open_error(StorageError::Io {
            message: "开发注入：存储打开失败".into(),
        }));
    }
    let codec = Arc::new(ProtoEventCodec);
    SqliteStore::open(
        data,
        codec.clone(),
        Arc::new(SqliteProjector::new(codec)),
        clock,
        StoreConfig::default(),
    )
    .map(Arc::new)
    .map_err(store_open_error)
}

/// 读模型的默认值：只在读侧补。`projects_root` 的默认值归 T10（T04 不读这一项）。
pub fn read_defaults() -> ReadDefaults {
    ReadDefaults {
        global_settings: GlobalSettings {
            playful_ui: true,
            autostart: false,
            projects_root: std::env::home_dir().unwrap_or_default(),
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use iwork_ipc::rendezvous::{Relaunchable, relaunchable};

    /// 记录调用次序的假端口。`events` 里种类名是 `zz_future_kind` 的那条读出来报 `UnknownKind`。
    struct Recording {
        head: u64,
        unknown_at: Option<u64>,
        unsettled: Unsettled,
        log: std::sync::Mutex<Vec<&'static str>>,
    }

    impl Recording {
        fn push(&self, op: &'static str) {
            self.log.lock().unwrap().push(op);
        }
        fn log(&self) -> Vec<&'static str> {
            self.log.lock().unwrap().clone()
        }
    }

    impl BootPorts for Recording {
        async fn head_seq(&self) -> Result<Seq, StorageError> {
            self.push("head");
            Ok(Seq(self.head))
        }
        async fn read_events(
            &self,
            from: Seq,
            limit: usize,
        ) -> Result<Vec<StoredEvent>, StorageError> {
            self.push("read");
            let end = (from.0 + limit as u64).min(self.head);
            if let Some(bad) = self.unknown_at
                && bad > from.0
                && bad <= end
            {
                return Err(StorageError::Codec(CodecError::UnknownKind {
                    kind: "zz_future_kind".into(),
                }));
            }
            Ok((from.0 + 1..=end)
                .map(|n| StoredEvent {
                    seq: Seq(n),
                    at: iwork_domain::common::Timestamp(0),
                    actor: iwork_domain::common::Actor("t".into()),
                    event: iwork_domain::event::GlobalSettingsChanged {
                        playful_ui: None,
                        autostart: None,
                        projects_root: None,
                    }
                    .into(),
                })
                .collect())
        }
        async fn recover_artifacts(&self) -> Result<(), String> {
            self.push("WRITE artifacts.recover");
            Ok(())
        }
        fn mark_delivery_started(&self) {
            self.push("mark_delivery_started");
        }
        async fn unsettled(&self) -> Result<Unsettled, StorageError> {
            self.push("unsettled");
            Ok(self.unsettled.clone())
        }
        async fn commit(&self, _req: CommitRequest) -> Result<CommitReceipt, StorageError> {
            self.push("WRITE commit");
            Ok(CommitReceipt {
                first_seq: Seq(1),
                last_seq: Seq(1),
                replayed: false,
            })
        }
    }

    fn unsettled_running_turn() -> Unsettled {
        let mut t = crate::fixtures::turn(20, 10);
        t.state = iwork_domain::turn::TurnState::Running;
        Unsettled {
            turns: vec![t],
            streaming: vec![],
            cards: vec![],
        }
    }

    /// 方案 S8：5a 把事件翻完之前没有任何写入（制品恢复、提交都算写）；读到不认识的种类时一次写入都没有。
    #[test]
    fn compat_runs_before_any_write() {
        let ids = MonotonicIds::new(SystemClock::new());
        let ok = Recording {
            head: 2500,
            unknown_at: None,
            unsettled: unsettled_running_turn(),
            log: Default::default(),
        };
        let mut entered = Vec::new();
        let interrupted = pollster::block_on(prepare(&ok, &ids, |s| {
            entered.push(s);
            true
        }))
        .unwrap();
        assert_eq!(interrupted.len(), 1);
        let log = ok.log();
        let reads = log.iter().filter(|op| **op == "read").count();
        assert_eq!(reads, 3, "2500 条按 1000 一页翻三页：{log:?}");
        let last_read = log.iter().rposition(|op| *op == "read").unwrap();
        let first_write = log.iter().position(|op| op.starts_with("WRITE")).unwrap();
        assert!(last_read < first_write, "{log:?}");
        assert!(log.contains(&"WRITE commit"), "对照：确实有写入：{log:?}");
        assert_eq!(
            entered,
            [
                Step::Compat,
                Step::Artifacts,
                Step::DeliveryGate,
                Step::Recovery
            ]
        );

        let bad = Recording {
            head: 2500,
            unknown_at: Some(1700),
            unsettled: unsettled_running_turn(),
            log: Default::default(),
        };
        let err = pollster::block_on(prepare(&bad, &ids, |_| true)).unwrap_err();
        assert_eq!(err.code(), ExitCode::StoreRefused);
        let log = bad.log();
        assert!(
            log.iter()
                .all(|op| !op.starts_with("WRITE") && *op != "mark_delivery_started"),
            "{log:?}"
        );
    }

    /// 对拍：存储（T02）真的因为「schema 比程序新」拒绝打开时，拒绝原文带着「被更新的」标记，诊断页能据此提示
    /// 装回更新的那一版（`Refusal::written_by_newer`）。
    #[test]
    fn newer_schema_refusal_is_marked_written_by_newer() {
        let t = tempfile::tempdir().unwrap();
        let dir = DataDir::from_root(t.path().join("data"));
        let clock = Arc::new(SystemClock::new());
        drop(open_store(&dir, clock.clone()).unwrap());
        rusqlite::Connection::open(dir.db_path())
            .unwrap()
            .execute(
                "UPDATE meta SET value = '999' WHERE key = 'schema_version'",
                [],
            )
            .unwrap();
        match open_store(&dir, clock) {
            Err(BootError::Refused(r)) => assert!(r.written_by_newer(), "{r:?}"),
            Err(other) => panic!("{other:?}"),
            Ok(_) => panic!("应拒绝打开"),
        }
    }

    #[test]
    fn step_names_are_unique() {
        let mut names: Vec<_> = Step::ALL.iter().map(|s| s.name()).collect();
        names.sort();
        names.dedup();
        assert_eq!(names.len(), Step::ALL.len());
    }

    /// 只有 `UnknownKind` 归 12（`Refused`），其余存储错误仍归 20（T07 PD6；方案 S8）。
    #[test]
    fn unknown_kind_maps_to_refused_not_fatal() {
        for step in [Step::Compat, Step::Server] {
            let e = read_error(
                step,
                StorageError::Codec(CodecError::UnknownKind {
                    kind: "zz_future_kind".into(),
                }),
            );
            assert_eq!(e.code(), ExitCode::StoreRefused);
            match e.status() {
                Some(Phase::Refused(Refusal::StoreOpenFailed { message })) => {
                    assert!(message.contains("zz_future_kind"), "{message}");
                    assert!(message.contains("更新的版本"), "{message}");
                    assert!(
                        Refusal::StoreOpenFailed {
                            message: message.clone()
                        }
                        .written_by_newer(),
                        "{message}"
                    );
                }
                other => panic!("{other:?}"),
            }
            assert_eq!(
                relaunchable(&e.exit_phase()),
                Relaunchable::NotWithThisProgram
            );
            for other in [
                StorageError::Io {
                    message: "x".into(),
                },
                StorageError::Busy,
                StorageError::Codec(CodecError::Malformed {
                    kind: "k".into(),
                    field: "f".into(),
                    message: "m".into(),
                }),
            ] {
                let e = read_error(step, other);
                assert_eq!(e.code(), ExitCode::Fatal);
                assert_eq!(relaunchable(&e.exit_phase()), Relaunchable::Yes);
            }
        }
    }

    /// 启动失败表（方案 2.2）的每一格：逻辑码、写不写状态、`relaunchable`。
    #[test]
    fn failure_table_cells() {
        let mismatch = store_open_error(StorageError::ProtocolMajorMismatch {
            database: 2,
            program: 1,
            backup: PathBuf::from("/b/x"),
        });
        let cases: Vec<(BootError, ExitCode, bool, Relaunchable)> = vec![
            (
                BootError::Identity("x".into()),
                ExitCode::Unsupported,
                false,
                Relaunchable::NotWithThisProgram,
            ),
            (
                BootError::Endpoint("x".into()),
                ExitCode::EndpointRefused,
                false,
                Relaunchable::NotWithThisProgram,
            ),
            (
                mismatch,
                ExitCode::StoreRefused,
                true,
                Relaunchable::NotWithThisProgram,
            ),
            (
                store_open_error(StorageError::Io {
                    message: "schema 比程序新".into(),
                }),
                ExitCode::StoreRefused,
                true,
                Relaunchable::NotWithThisProgram,
            ),
            (
                BootError::Bind("x".into()),
                ExitCode::BindFailed,
                true,
                Relaunchable::Yes,
            ),
            (
                BootError::Fatal {
                    step: Step::Recovery,
                    message: "x".into(),
                },
                ExitCode::Fatal,
                true,
                Relaunchable::Yes,
            ),
        ];
        for (e, code, writes, rel) in cases {
            assert_eq!(e.code(), code, "{e:?}");
            assert_eq!(e.status().is_some(), writes, "{e:?}");
            assert_eq!(relaunchable(&e.exit_phase()), rel, "{e:?}");
        }
    }
}
