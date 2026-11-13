//! 步 3 动到的写入热路径（T10a 方案第 6 节「存储写入」「每条命令」两行）。每组一行 JSON 到 stdout。
//!
//! `cargo bench -p iwork-daemon --bench command_path`（重测试，套 `lockf /tmp/iwork-heavy-test.lock`）。
//! 同一进程里两边交替跑多轮（每轮换先后），各取每次的中位耗时；本机负载高时只看相对值。开头、结尾各记一次负载。
//!
//! - `gate_commit`：存储单事件提交，裸存储对带封口闸的存储（多拿一把没有争用的锁）。
//! - `command_roundtrip`：同一种提交（`SetPlayfulUi` 的 `GlobalSettingsChanged`）直接 `commit`，对经分发入口受理
//!   （在途表进出 + 排在写队列里查回执 + 提交 + 回执）。两边的提交是同一笔，差的就是分发入口多出来的那一截。
//!
//! - `turn_start_read`（步 4）：会话线在每个回合开始前做的那次准备——三次读（回合、这一轮的请求、算快照的已提交
//!   输入）与路径解析加快照重算（`prepare`）——各自的中位耗时。收尾那次读是改动探针的读，探针按 7/F-2 关着，不量。
//!
//! 都关 fullfsync（只比相对值，两边一样）。

use std::collections::BTreeSet;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Instant;

use iwork_agent_acp::{AcpRuntime, RuntimeLaunch};
use iwork_app::codec::ProtoEventCodec;
use iwork_core::approval::engine::{ApprovalEngine, EnginePorts};
use iwork_daemon::gate::{GatedStore, SealGate};
use iwork_daemon::handler::dispatch::{CommandPorts, Dispatcher, HostState};
use iwork_daemon::time::{MonotonicIds, SystemClock, TokioTimer};
use iwork_domain::common::Actor;
use iwork_domain::event::GlobalSettingsChanged;
use iwork_domain::id::{CommandId, RawUlid};
use iwork_host::fs::OsPathResolver;
use iwork_ipc::peer::UserId;
use iwork_ipc::server::{ConnectionContext, PeerSummary, RequestHandler};
use iwork_ports::store::{CommitRequest, StoragePort};
use iwork_proto::pb;
use iwork_store::{DataDir, ReadDefaults, SqliteProjector, SqliteStore, StoreConfig};

type Store = SqliteStore<ProtoEventCodec>;

#[path = "../../iwork-store/tests/support/fixtures.rs"]
mod fixtures;

const N: usize = 2000;
const ROUNDS: usize = 5;

fn median(mut v: Vec<f64>) -> f64 {
    v.sort_by(|a, b| a.partial_cmp(b).unwrap());
    v[v.len() / 2]
}

fn emit(name: &str, fields: serde_json::Value) {
    let mut obj = serde_json::json!({ "bench": name });
    obj.as_object_mut()
        .unwrap()
        .extend(fields.as_object().unwrap().clone());
    println!("{obj}");
}

fn open_store(dir: &tempfile::TempDir) -> Arc<Store> {
    let data = DataDir::from_root(dir.path().join("data"));
    data.ensure_layout().unwrap();
    let codec = Arc::new(ProtoEventCodec);
    Arc::new(
        SqliteStore::open(
            &data,
            codec.clone(),
            Arc::new(SqliteProjector::new(codec)),
            Arc::new(SystemClock::new()),
            StoreConfig {
                fullfsync: Some(false),
                ..StoreConfig::default()
            },
        )
        .unwrap(),
    )
}

fn id(n: usize) -> CommandId {
    CommandId::from_raw(RawUlid::from_parts(1_794_268_800_000, n as u128 + 1))
}

fn request(n: usize) -> CommitRequest {
    CommitRequest {
        command_id: id(n),
        actor: Actor("bench".into()),
        events: vec![
            GlobalSettingsChanged {
                playful_ui: Some(n.is_multiple_of(2)),
                autostart: None,
                projects_root: None,
            }
            .into(),
        ],
        outbox: vec![],
    }
}

async fn timed<F, Fut>(mut f: F) -> f64
where
    F: FnMut(usize) -> Fut,
    Fut: std::future::Future<Output = ()>,
{
    let mut us = Vec::with_capacity(N);
    for i in 0..N {
        let t = Instant::now();
        f(i).await;
        us.push(t.elapsed().as_secs_f64() * 1e6);
    }
    median(us)
}

fn gate_commit(rt: &tokio::runtime::Runtime) {
    let (mut raw, mut gated) = (Vec::new(), Vec::new());
    for round in 0..ROUNDS {
        let run_raw = || {
            let dir = tempfile::tempdir().unwrap();
            let store = open_store(&dir);
            rt.block_on(timed(|i| {
                let s = store.clone();
                async move {
                    s.commit(request(i)).await.unwrap();
                }
            }))
        };
        let run_gated = || {
            let dir = tempfile::tempdir().unwrap();
            let store = GatedStore::new(open_store(&dir), Arc::new(SealGate::default()));
            rt.block_on(timed(|i| {
                let fut = store.commit(request(i));
                async move {
                    fut.await.unwrap();
                }
            }))
        };
        // 每轮换先后，负载随时间的漂移不偏向一边
        if round.is_multiple_of(2) {
            raw.push(run_raw());
            gated.push(run_gated());
        } else {
            gated.push(run_gated());
            raw.push(run_raw());
        }
    }
    emit(
        "gate_commit",
        serde_json::json!({
            "rounds": ROUNDS,
            "commits_per_round": N,
            "raw_median_us": median(raw.clone()),
            "gated_median_us": median(gated.clone()),
            "raw_rounds_us": raw,
            "gated_rounds_us": gated,
        }),
    );
}

struct Open;
impl HostState for Open {
    fn is_closed(&self) -> bool {
        false
    }
}

fn ctx() -> ConnectionContext {
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

type Gated = GatedStore<Store>;
type Disp = Dispatcher<
    Gated,
    iwork_store::SqliteReadModel<ProtoEventCodec>,
    ApprovalEngine<
        SystemClock,
        MonotonicIds<SystemClock>,
        Gated,
        AcpRuntime,
        TokioTimer,
        OsPathResolver,
    >,
>;

/// 分发入口：真引擎 + 带闸存储 + 宿主开着。
async fn dispatcher(raw: &Arc<Store>, gated: &Gated, dir: &tempfile::TempDir) -> Arc<Disp> {
    let (engine, _) = ApprovalEngine::start(
        EnginePorts {
            clock: SystemClock::new(),
            ids: MonotonicIds::new(SystemClock::new()),
            store: gated.clone(),
            reviewer: AcpRuntime::new(RuntimeLaunch {
                program: PathBuf::from("/nonexistent"),
                args: vec![],
                env: vec![],
            }),
            timer: TokioTimer,
            fs: OsPathResolver::new(dir.path().into(), false),
        },
        BTreeSet::new(),
        |_, _| None,
    )
    .await
    .unwrap();
    Arc::new(Dispatcher::new(CommandPorts {
        store: Arc::new(gated.clone()),
        read: Arc::new(raw.read_model(ReadDefaults {
            global_settings: iwork_domain::settings::GlobalSettings {
                playful_ui: true,
                autostart: false,
                projects_root: dir.path().join("projects"),
            },
        })),
        engine,
        ids: Arc::new(MonotonicIds::new(SystemClock::new())),
        clock: Arc::new(SystemClock::new()),
        host: Arc::new(Open),
    }))
}

fn command_roundtrip(rt: &tokio::runtime::Runtime) {
    let (mut direct, mut dispatched) = (Vec::new(), Vec::new());
    for round in 0..ROUNDS {
        let run_direct = || {
            let dir = tempfile::tempdir().unwrap();
            let store = GatedStore::new(open_store(&dir), Arc::new(SealGate::default()));
            rt.block_on(timed(|i| {
                let fut = store.commit(request(i));
                async move {
                    fut.await.unwrap();
                }
            }))
        };
        // 每轮换先后（同 gate_commit）
        if round.is_multiple_of(2) {
            direct.push(run_direct());
        }
        let dir = tempfile::tempdir().unwrap();
        let raw = open_store(&dir);
        let gated = GatedStore::new(raw.clone(), Arc::new(SealGate::default()));
        let d = rt.block_on(dispatcher(&raw, &gated, &dir));
        dispatched.push(rt.block_on(timed(|i| {
            let d = d.clone();
            async move {
                let r = d
                    .handle(
                        &ctx(),
                        pb::Request {
                            request_id: i as u64,
                            body: Some(pb::request::Body::Command(pb::Command {
                                command_id: id(i).to_string(),
                                kind: Some(pb::command::Kind::SetPlayfulUi(pb::SetPlayfulUi {
                                    enabled: i.is_multiple_of(2),
                                })),
                            })),
                        },
                    )
                    .await;
                assert!(matches!(r.body, Some(pb::response::Body::Accepted(_))));
            }
        })));
        if !round.is_multiple_of(2) {
            direct.push(run_direct());
        }
    }
    emit(
        "command_roundtrip",
        serde_json::json!({
            "rounds": ROUNDS,
            "commands_per_round": N,
            "direct_commit_median_us": median(direct.clone()),
            "dispatched_median_us": median(dispatched.clone()),
            "direct_rounds_us": direct,
            "dispatched_rounds_us": dispatched,
        }),
    );
}

/// 步 4：回合开始前的准备。夹具：内置助手 + 一个项目对话 + 经分发入口排上的一个回合。
fn turn_start_read(rt: &tokio::runtime::Runtime) {
    use iwork_daemon::host::ports::HostEnv;
    use iwork_daemon::host::prepare::prepare;
    use iwork_ports::read_model::ReadModelPort;
    let dir = tempfile::tempdir().unwrap();
    let raw = open_store(&dir);
    let gated = GatedStore::new(raw.clone(), Arc::new(SealGate::default()));
    let read = raw.read_model(ReadDefaults {
        global_settings: iwork_domain::settings::GlobalSettings {
            playful_ui: true,
            autostart: false,
            projects_root: dir.path().join("projects"),
        },
    });
    let turn_id = rt.block_on(async {
        iwork_daemon::assembly::register_builtin_assistant(&gated, &read, &SystemClock::new())
            .await
            .unwrap();
        raw.commit(CommitRequest {
            command_id: id(900_000),
            actor: Actor("bench".into()),
            events: vec![
                iwork_domain::event::ProjectAdded {
                    project: fixtures::project(7),
                }
                .into(),
                iwork_domain::event::ConversationCreated {
                    conversation: fixtures::project_conversation(10, 7),
                }
                .into(),
            ],
            outbox: vec![],
        })
        .await
        .unwrap();
        let d = dispatcher(&raw, &gated, &dir).await;
        let r = d
            .handle(
                &ctx(),
                pb::Request {
                    request_id: 1,
                    body: Some(pb::request::Body::Command(pb::Command {
                        command_id: id(900_001).to_string(),
                        kind: Some(pb::command::Kind::SendMessage(pb::SendMessage {
                            conversation_id: fixtures::cid(10).to_string(),
                            text: "hi".into(),
                            attachment_ids: vec![],
                        })),
                    })),
                },
            )
            .await;
        assert!(matches!(r.body, Some(pb::response::Body::Accepted(_))));
        let events = raw
            .read_events(
                iwork_ports::store::EventFilter::All,
                iwork_domain::id::Seq(0),
                1000,
            )
            .await
            .unwrap();
        events
            .into_iter()
            .find_map(|e| match e.event {
                iwork_domain::event::DomainEvent::TurnQueued(q) => Some(q.turn.id),
                _ => None,
            })
            .expect("排上了回合")
    });
    let env = HostEnv {
        platform: HostEnv::platform(),
        home: dir.path().join("home"),
        data_dir: dir.path().join("data"),
        tmp_dirs: vec![std::env::temp_dir()],
    };
    let fs = OsPathResolver::new(env.home.clone(), false);
    let (mut reads, mut prepares) = (Vec::with_capacity(N), Vec::with_capacity(N));
    rt.block_on(async {
        for _ in 0..N {
            let t = Instant::now();
            let (turn, c) = read.turn_with_conversation(turn_id).await.unwrap().unwrap();
            let _request = read.turn_request(turn.id).await.unwrap();
            let inputs = read.snapshot_inputs(c.id).await.unwrap();
            reads.push(t.elapsed().as_secs_f64() * 1e6);
            let t = Instant::now();
            let prepared = prepare(&env, &fs, &inputs, &turn).unwrap();
            prepares.push(t.elapsed().as_secs_f64() * 1e6);
            std::hint::black_box(prepared);
        }
    });
    emit(
        "turn_start_read",
        serde_json::json!({
            "samples": N,
            "reads_median_us": median(reads),
            "prepare_median_us": median(prepares),
        }),
    );
}

fn main() {
    let filter: Vec<String> = std::env::args()
        .skip(1)
        .filter(|a| !a.starts_with("--"))
        .collect();
    let on = |name: &str| filter.is_empty() || filter.iter().any(|f| name.contains(f.as_str()));
    if let Ok(o) = std::process::Command::new("sysctl")
        .args(["-n", "vm.loadavg"])
        .output()
    {
        emit(
            "environment",
            serde_json::json!({ "loadavg": String::from_utf8_lossy(&o.stdout).trim() }),
        );
    }
    let rt = tokio::runtime::Runtime::new().unwrap();
    if on("gate_commit") {
        gate_commit(&rt);
    }
    if on("command_roundtrip") {
        command_roundtrip(&rt);
    }
    if on("turn_start_read") {
        turn_start_read(&rt);
    }
    if let Ok(o) = std::process::Command::new("sysctl")
        .args(["-n", "vm.loadavg"])
        .output()
    {
        emit(
            "environment_end",
            serde_json::json!({ "loadavg": String::from_utf8_lossy(&o.stdout).trim() }),
        );
    }
}
