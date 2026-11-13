//! 后台的结构化日志（T04 方案 2.14；设计 10.3；需求 11）：写 `<数据目录>/logs/daemon.log`。
//!
//! 格式、滚动、队列、脱敏都在 `iwork-log`（客户端用同一份，ADR-0013）；这里只放后台自己的：命令、回合、审批的
//! span，以及把 ipc 审计转成后台信号的旁路（[`AuditTap`]）。审计 target（`iwork.ipc.audit`）永远不被级别过滤掉。

use std::path::Path;

use tracing::field::{Field, Visit};
use tracing::{Event, Level, Subscriber};
use tracing_subscriber::Layer;
use tracing_subscriber::filter::Targets;
use tracing_subscriber::layer::Context;

pub use iwork_log::flush_global;

/// 后台日志里永远放行的 target。
const ALWAYS_ON: &[&str] = &[iwork_ipc::audit::TARGET];
const PROGRAM: &str = "iwork-daemon";

/// span 字段名的唯一定义处：命令、回合、审批各一个 span（卡范围 9）。
/// T04 在请求入口开 `command`；`turn`、`approval` 由 T10 在回合宿主里开（`iwork-core` 不依赖输出端）。
pub mod span {
    pub fn command(command_id: &str, kind: &str) -> tracing::Span {
        tracing::info_span!("command", command_id, kind)
    }
    pub fn turn(turn_id: &str) -> tracing::Span {
        tracing::info_span!("turn", turn_id)
    }
    pub fn approval(approval_id: &str) -> tracing::Span {
        tracing::info_span!("approval", approval_id)
    }
}

/// ipc 审计里要交给后台自己处理的信号（方案 2.10、2.9）。日志旁路按审计 target 与事件名过滤出来，不改日志本身。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AuditSignal {
    /// 服务端拒绝了来连的客户端（升级后新客户端被旧后台拒）：安装检查立刻跑一次。
    PeerRejectedByServer,
    /// 连接因后台自身问题被关（`cause.is_server_fault()` 那条 `error`）：`cause` 是 `FailCause::as_str()`。
    ServerFault { cause: String },
}

/// 审计旁路。走日志而不是回调，是 T03 方案 2.12 定下的接口。
pub struct AuditTap {
    tx: tokio::sync::broadcast::Sender<AuditSignal>,
}

impl AuditTap {
    pub fn new(tx: tokio::sync::broadcast::Sender<AuditSignal>) -> Self {
        Self { tx }
    }
}

#[derive(Default)]
struct AuditFields {
    event: Option<String>,
    role: Option<String>,
    cause: Option<String>,
}

impl Visit for AuditFields {
    fn record_str(&mut self, field: &Field, value: &str) {
        let slot = match field.name() {
            "event" => &mut self.event,
            "role" => &mut self.role,
            "cause" => &mut self.cause,
            _ => return,
        };
        *slot = Some(value.to_owned());
    }
    fn record_debug(&mut self, field: &Field, value: &dyn std::fmt::Debug) {
        self.record_str(field, &format!("{value:?}"));
    }
}

/// 纯函数：一条审计事件对应的信号。
pub fn audit_signal(
    level: &Level,
    event: &str,
    role: Option<&str>,
    cause: Option<&str>,
) -> Option<AuditSignal> {
    use iwork_ipc::audit::{EVENT_CONNECTION_FAILED, EVENT_PEER_REJECTED};
    if event == EVENT_PEER_REJECTED && role == Some("server") {
        return Some(AuditSignal::PeerRejectedByServer);
    }
    // 后台自身问题另记一条 `error` 级别的同名事件；`warn` 那条是所有关连接都有的
    if event == EVENT_CONNECTION_FAILED && *level == Level::ERROR {
        return cause.map(|c| AuditSignal::ServerFault {
            cause: c.to_owned(),
        });
    }
    None
}

impl<S: Subscriber> Layer<S> for AuditTap {
    fn on_event(&self, event: &Event<'_>, _ctx: Context<'_, S>) {
        let meta = event.metadata();
        if meta.target() != iwork_ipc::audit::TARGET {
            return;
        }
        let mut f = AuditFields::default();
        event.record(&mut f);
        let Some(name) = f.event.as_deref() else {
            return;
        };
        if let Some(sig) = audit_signal(meta.level(), name, f.role.as_deref(), f.cause.as_deref()) {
            // 没有接收方（任务还没起）时丢掉：这些信号只是「加速」或「核对一次」
            let _ = self.tx.send(sig);
        }
    }
}

/// 级别过滤：`IWORK_LOG` 解析不了时用默认的 `info`；审计 target 永远放行。
pub fn filter_from(spec: Option<&str>) -> Targets {
    iwork_log::filter_from(spec, ALWAYS_ON, PROGRAM)
}

/// 装好的后台日志。丢弃时把队列排空、停写线程（停机第 7 步「日志线程排空」）。
pub struct LogGuard {
    _inner: iwork_log::LogGuard,
    /// 审计旁路的发送端：要收信号的任务各 `subscribe` 一个。
    pub audit: tokio::sync::broadcast::Sender<AuditSignal>,
}

/// 装全局日志：写 `dir/daemon.log`；打不开就只写 stderr（启动表第 4 步：日志目录写不了时继续）。叠上审计旁路。
pub fn init(dir: &Path) -> LogGuard {
    let (audit, _) = tokio::sync::broadcast::channel(64);
    let inner = iwork_log::init(
        iwork_log::LogConfig {
            dir: Ok(dir.to_path_buf()),
            file: iwork_log::DAEMON_LOG,
            program: PROGRAM,
            always_on: ALWAYS_ON,
        },
        Some(Box::new(AuditTap::new(audit.clone()))),
    );
    LogGuard {
        _inner: inner,
        audit,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    use tracing_subscriber::layer::SubscriberExt;

    /// 本线程装的订阅者（`with_default` / `set_default`）收不全事件的竞态，在这里堵上；整个测试程序只装一次。
    ///
    /// tracing 按「第一次命中某个 callsite 时有哪些订阅者」算 interest 并缓存。只有一个分发器时，
    /// 它用命中线程自己的默认订阅者来算：别的测试线程（比如 `handler` 的测试走 `span::command`、
    /// 或碰到 ipc 审计）没装订阅者，先命中就会把 callsite 缓存成 never，本线程的订阅者从此收不到。
    /// 全局装一个对谁都答 sometimes、自己不收事件的订阅者，没装订阅者的线程算出来的也不再是 never。
    /// 同一个竞态在 iwork-ipc 有一个必红的复现：`crates/iwork-ipc/tests/audit_capture.rs`。
    fn keep_callsites_interested() {
        static ONCE: std::sync::Once = std::sync::Once::new();
        ONCE.call_once(|| {
            tracing::subscriber::set_global_default(KeepInterest)
                .expect("iwork-daemon 的单元测试里只有这里装全局订阅者");
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

    /// 对拍（方案 2.10）：用内存传输真的触发一次服务端拒绝与一次交接超时，旁路都收得到；
    /// 对照：被拒的另一端（客户端）那条 `peer_rejected` 不算。
    #[tokio::test]
    async fn audit_tap_sees_real_ipc_faults() {
        use iwork_ipc::client::{Client, ClientConfig};
        use iwork_ipc::peer::{
            ImageId, Manifest, PeerIdentity, SelfIdentity, SignerIdentity, UserId,
        };
        use iwork_ipc::server::{EventSource, ReplayPage, Server, ServerConfig, SourceError};
        use iwork_ipc::transport::memory;
        use iwork_proto::pb;
        use iwork_proto::routing::StreamKey;
        use std::path::PathBuf;
        use std::time::Duration;

        struct NeverSnapshot;
        impl EventSource for NeverSnapshot {
            async fn head_seq(&self) -> Result<u64, SourceError> {
                Ok(0)
            }
            fn scope_streams(&self, scope: &pb::SubscriptionScope) -> Vec<StreamKey> {
                iwork_app::subscription::scope_streams(scope)
            }
            async fn replay(
                &self,
                _: &[StreamKey],
                _: u64,
                to: u64,
                _: usize,
            ) -> Result<ReplayPage, SourceError> {
                Ok(ReplayPage {
                    events: vec![],
                    next_from: to,
                })
            }
            async fn snapshot(
                &self,
                _: &[pb::SubscriptionScope],
            ) -> Result<(pb::Snapshot, u64), SourceError> {
                std::future::pending().await
            }
        }

        const UID: u32 = 501;
        fn image(b: u8) -> ImageId {
            ImageId::Sha256([b; 32])
        }
        let manifest = Manifest::parse(&format!(
            r#"{{"manifest_version":1,"app_version":"1.0.0","binaries":{{"iwork":["{}","{}"]}}}}"#,
            image(0x51),
            image(0xC1)
        ))
        .unwrap();
        let own = |b: u8| SelfIdentity {
            user: UserId::Uid(UID),
            signer: SignerIdentity::Image(image(b)),
            exe_dir: PathBuf::from("/opt/iwork"),
            manifest: Some(manifest.clone()),
        };
        fn peer(uid: u32, b: u8) -> PeerIdentity {
            PeerIdentity {
                pid: 7,
                user: UserId::Uid(uid),
                exe: Some(PathBuf::from("/opt/iwork/iwork")),
                signer: SignerIdentity::Image(image(b)),
            }
        }

        let (tx, mut rx) = tokio::sync::broadcast::channel(16);
        keep_callsites_interested();
        let _guard = tracing::subscriber::set_default(
            tracing_subscriber::registry().with(AuditTap::new(tx)),
        );
        let mut cfg = ServerConfig::new("t");
        cfg.handover_timeout = Duration::from_millis(100);
        let (server, _publisher) =
            Server::new(cfg, own(0x51), crate::handler::NotWired, NeverSnapshot);
        let (listener, good) =
            memory::endpoint("tap", peer(UID, 0xC1), Arc::new(move |_| peer(UID, 0x51)));
        let token = tokio_util::sync::CancellationToken::new();
        let serve = tokio::spawn(server.serve(listener, token.clone()));

        // 一次交接超时：订阅要快照，快照永远不返回
        let (client, _incoming) = Client::connect(
            &good,
            &iwork_ipc::transport::Endpoint::Memory("tap".into()),
            &own(0xC1),
            ClientConfig::new(pb::ClientKind::Desktop, "t"),
        )
        .await
        .expect("握手");
        let scope = pb::SubscriptionScope {
            kind: Some(pb::subscription_scope::Kind::Settings(pb::SettingsScope {})),
        };
        let _ = client.subscribe(1, scope, 0).await;
        // 一次服务端拒绝：另一个用户来连
        let stranger = good.with_client_identity(peer(UID + 1, 0xC1));
        let _ = Client::connect(
            &stranger,
            &iwork_ipc::transport::Endpoint::Memory("tap".into()),
            &own(0xC1),
            ClientConfig::new(pb::ClientKind::Desktop, "t"),
        )
        .await;

        let mut got = Vec::new();
        while got.len() < 2 {
            let sig = tokio::time::timeout(Duration::from_secs(5), rx.recv())
                .await
                .expect("旁路没收到信号")
                .unwrap();
            got.push(sig);
        }
        got.sort_by_key(|s| format!("{s:?}"));
        assert_eq!(
            got,
            [
                AuditSignal::PeerRejectedByServer,
                AuditSignal::ServerFault {
                    cause: "HandoverTimeout".into()
                },
            ]
        );
        token.cancel();
        let _ = serve.await;
    }

    #[test]
    fn audit_signal_table() {
        use iwork_ipc::audit::{EVENT_CONNECTION_FAILED, EVENT_PEER_REJECTED};
        assert_eq!(
            audit_signal(&Level::WARN, EVENT_PEER_REJECTED, Some("server"), None),
            Some(AuditSignal::PeerRejectedByServer)
        );
        assert_eq!(
            audit_signal(&Level::WARN, EVENT_PEER_REJECTED, Some("client"), None),
            None
        );
        assert_eq!(
            audit_signal(
                &Level::ERROR,
                EVENT_CONNECTION_FAILED,
                None,
                Some("PublishStalled")
            ),
            Some(AuditSignal::ServerFault {
                cause: "PublishStalled".into()
            })
        );
        // `warn` 那条是所有关连接都有的，不算后台故障
        assert_eq!(
            audit_signal(
                &Level::WARN,
                EVENT_CONNECTION_FAILED,
                None,
                Some("QueueFull")
            ),
            None
        );
    }

    #[test]
    fn audit_target_is_never_filtered_out() {
        let f = filter_from(Some("off"));
        assert!(iwork_log::enabled(
            &f,
            iwork_ipc::audit::TARGET,
            Level::WARN
        ));
        assert!(!iwork_log::enabled(&f, "iwork_daemon", Level::ERROR));
        let d = filter_from(None);
        assert!(iwork_log::enabled(&d, "iwork_daemon", Level::INFO));
        assert!(!iwork_log::enabled(&d, "iwork_daemon", Level::DEBUG));
    }
}
