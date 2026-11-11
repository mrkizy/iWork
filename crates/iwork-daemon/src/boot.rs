//! 启动顺序（T04 方案 2.4）。一张有序的表：每一步有名字（写进状态文件的 `Starting{step}`）、
//! 失败时落到 2.2 启动失败表的哪一格。顺序不能换，理由在方案 2.4 的右列。
//!
//! 失败落到哪一格只由 [`BootError`] 的变体决定（[`BootError::code`]、[`BootError::status`]），
//! 各步只负责把自己的错误归到变体上，不另做决定。

use std::ffi::OsString;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Instant;

use iwork_app::codec::ProtoEventCodec;
use iwork_domain::settings::GlobalSettings;
use iwork_ipc::peer::{self, ImageId, SelfIdentity};
use iwork_ipc::rendezvous::{ExeStamp, ExitCode, Phase, Refusal};
use iwork_ports::store::{CodecError, StorageError};
use iwork_store::{DataDir, ReadDefaults, SqliteProjector, SqliteStore, StoreConfig};

#[cfg(not(unix))]
use crate::args::DaemonArgs;
use crate::dev;
use crate::exit::{exited, process_status};
use crate::time::SystemClock;

/// 后台用的存储类型。
pub type Store = SqliteStore<ProtoEventCodec>;

/// 开发构建的暂停点：状态已写定、锁还没放（见 [`dev`]）。
pub const PAUSE_BEFORE_EXIT: &str = "before-exit";

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
                    "库里有本版本不认识的事件种类 {kind}：这个库被更新的版本写过，装回更新的那一版可继续使用"
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
    let unsupported = |m: String| BootError::Identity(m);
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
