//! 后台服务生命周期基准（T04 方案第 6 节）。每组一行 JSON 到 stdout。
//!
//! `cargo bench -p iwork-daemon --features dev --bench lifecycle`（要 dev：测试进程不是受信映像，后台带 `--dev-trust`）。
//! 重测试排队：套 `lockf /tmp/iwork-heavy-test.lock`。本机负载高时绝对值不可信，只看同一时段的相对值。
//!
//! - `daemon_cold_start{events}`：空库 / 1 万 / 10 万事件，量「进程启动 → 握手成功」，取中位数；
//! - `client_reconnect_running`：后台在跑时新开一次连接（双向身份校验 + 握手）；
//! - `client_connect_cold`：后台没在跑，`Launcher::connect_or_launch` 从调用到连上；
//! - `idle_cpu`：就绪后空闲 `IWORK_BENCH_IDLE_SECS`（默认 60）秒的 CPU 占用；
//! - 缩小规模（CI 冒烟用）：`IWORK_BENCH_COLD_EVENTS_MAX` 只跑不超过它的冷启动档；
//! - `write_commit_observer`：存储单事件提交，装真的 `PublishObserver`（转 pb、算路由、推给 ipc 的 Publisher）
//!   与不装观察者交替各跑几轮，每次提交的中位耗时（热路径前后对比，AGENTS.md「性能」）。两种事件：
//!   `GlobalSettingsChanged`（不叫醒谁）与 `ConversationDeleting`（在回合宿主的唤醒登记表里、也在引擎的流里，
//!   观察者两个都叫醒）。每轮换先后。

use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::Arc;
use std::time::{Duration, Instant};

use iwork_app::codec::ProtoEventCodec;
use iwork_domain::common::Actor;
use iwork_domain::event::{
    ConversationCreated, ConversationDeleting, DomainEvent, GlobalSettingsChanged, ProjectAdded,
};
use iwork_domain::id::{CommandId, RawUlid};
use iwork_ipc::client::{Client, ClientConfig};
use iwork_ipc::peer::{SelfIdentity, TrustPolicy};
use iwork_ipc::server::{Server, ServerConfig};
use iwork_ipc::{LocalConnector, LocalEndpoint};
use iwork_ports::store::{CommitRequest, StoragePort};
use iwork_proto::pb;
use iwork_store::{DataDir, SqliteProjector, SqliteStore, StoreConfig};

const DAEMON: &str = env!("CARGO_BIN_EXE_iwork-daemon");

#[path = "../../iwork-store/tests/support/fixtures.rs"]
mod fixtures;

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

struct Home {
    dir: tempfile::TempDir,
}

impl Home {
    fn new() -> Home {
        Home {
            dir: tempfile::Builder::new()
                .prefix("iwb-")
                .tempdir_in("/tmp")
                .unwrap(),
        }
    }
    fn path(&self) -> &Path {
        self.dir.path()
    }
    fn run_dir(&self) -> PathBuf {
        iwork_ipc::endpoint::unix_run_dir(|_| None, Some(self.path().to_path_buf())).unwrap()
    }
    fn data(&self) -> DataDir {
        DataDir::from_root(self.run_dir().parent().unwrap().to_path_buf())
    }
}

/// 决定后台数据目录与 run 目录的变量：拉起后台前都要清掉，否则后台照外面的值找目录，基准却在临时 HOME 下等
/// （Linux 上 XDG_RUNTIME_DIR 优先于 HOME；GitHub 的 ubuntu runner 设了它）。
const DIR_VARS: [&str; 4] = [
    "XDG_RUNTIME_DIR",
    "XDG_DATA_HOME",
    "XDG_CONFIG_HOME",
    "IWORK_DATA_DIR",
];

/// 后台子进程的 stderr 去哪：设了 `IWORK_BENCH_CHILD_STDERR_DIR`（基准冒烟设的）就各写一个文件进去，冒烟跑完在里面
/// 找 panic 行——后台里可重启的任务 panic 后由监督器恢复、连接照样成功，只有 stderr 上那一行能看出来；没设就丢掉，
/// 平时跑基准不多出文件。
fn child_stderr() -> Stdio {
    use std::sync::atomic::{AtomicUsize, Ordering};
    static N: AtomicUsize = AtomicUsize::new(0);
    let Ok(dir) = std::env::var("IWORK_BENCH_CHILD_STDERR_DIR") else {
        return Stdio::null();
    };
    let path = Path::new(&dir).join(format!(
        "daemon-{}.stderr",
        N.fetch_add(1, Ordering::SeqCst)
    ));
    let file = std::fs::File::create(&path)
        .unwrap_or_else(|e| panic!("建不了后台 stderr 文件 {}：{e}", path.display()));
    Stdio::from(file)
}

/// 和产品的 `DetachedSpawn` 一样脱离（新会话、工作目录 /、另起线程收尸），只是 stderr 交给 `child_stderr`。
/// 产品的拉起方式把标准流都接到 /dev/null，基准换这个实现，不改产品。
struct BenchDetachedSpawn;

impl iwork_client_core::launcher::Spawn for BenchDetachedSpawn {
    fn spawn(&self, exe: &Path, args: &[&str]) -> std::io::Result<()> {
        use std::os::unix::process::CommandExt;
        let mut cmd = Command::new(exe);
        cmd.args(args)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(child_stderr())
            .current_dir("/");
        // SAFETY：pre_exec 里只调 async-signal-safe 的 setsid
        unsafe {
            cmd.pre_exec(|| {
                if libc::setsid() == -1 {
                    return Err(std::io::Error::last_os_error());
                }
                Ok(())
            });
        }
        let mut child = cmd.spawn()?;
        std::thread::spawn(move || {
            let _ = child.wait();
        });
        Ok(())
    }
}

fn spawn(home: &Home) -> Child {
    let mut c = Command::new(DAEMON);
    c.arg("--dev-trust")
        .env("HOME", home.path())
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(child_stderr());
    for k in DIR_VARS {
        c.env_remove(k);
    }
    c.spawn().unwrap()
}

fn stop(mut child: Child) {
    // SAFETY：只结束本基准起的后台
    unsafe {
        libc::kill(child.id() as i32, libc::SIGTERM);
    }
    let _ = child.wait();
}

fn client_cfg() -> ClientConfig {
    let mut cfg = ClientConfig::new(pb::ClientKind::DevTool, "bench");
    cfg.trust = TrustPolicy::DevTrust;
    cfg
}

async fn try_connect(home: &Home, own: &SelfIdentity) -> bool {
    let Ok(ep) = LocalEndpoint::in_run_dir(&home.run_dir()) else {
        return false;
    };
    Client::connect(&LocalConnector, ep.endpoint(), own, client_cfg())
        .await
        .is_ok()
}

/// 往库里写 `n` 条事件（每次提交 500 条）。
fn seed(home: &Home, n: usize) {
    if n == 0 {
        return;
    }
    let data = home.data();
    data.ensure_layout().unwrap();
    let store = open_store(&data);
    let rt = tokio::runtime::Runtime::new().unwrap();
    rt.block_on(async {
        let mut done = 0;
        let mut cmd = 1u128;
        while done < n {
            let k = (n - done).min(500);
            store.commit(request(cmd, k)).await.unwrap();
            cmd += 1;
            done += k;
        }
    });
    store.close().unwrap();
}

fn request(cmd: u128, events: usize) -> CommitRequest {
    request_of(
        cmd,
        (0..events)
            .map(|i| {
                GlobalSettingsChanged {
                    playful_ui: Some(i.is_multiple_of(2)),
                    autostart: None,
                    projects_root: None,
                }
                .into()
            })
            .collect(),
    )
}

fn request_of(cmd: u128, events: Vec<DomainEvent>) -> CommitRequest {
    CommitRequest {
        command_id: CommandId::from_raw(RawUlid::from_parts(1_794_268_800_000, cmd)),
        actor: Actor("bench".into()),
        events,
        outbox: vec![],
    }
}

/// 观察者那一组提交的事件。
#[derive(Debug, Clone, Copy)]
enum Load {
    /// 谁都不叫醒
    Settings,
    /// 叫醒回合宿主与引擎（`conversation_deleting`）
    Wakes,
}

const BENCH_PROJECT: u128 = 7;
const BENCH_CONV: u128 = 10;

fn open_store(data: &DataDir) -> SqliteStore<ProtoEventCodec> {
    let codec = Arc::new(ProtoEventCodec);
    SqliteStore::open(
        data,
        codec.clone(),
        Arc::new(SqliteProjector::new(codec)),
        Arc::new(iwork_daemon::time::SystemClock::new()),
        StoreConfig {
            fullfsync: Some(false),
            ..StoreConfig::default()
        },
    )
    .unwrap()
}

fn cold_start(rt: &tokio::runtime::Runtime, own: &SelfIdentity, events: usize, reps: usize) {
    let mut ms = Vec::new();
    for _ in 0..reps {
        let home = Home::new();
        seed(&home, events);
        let t = Instant::now();
        let child = spawn(&home);
        rt.block_on(async {
            while !try_connect(&home, own).await {
                tokio::time::sleep(Duration::from_millis(2)).await;
                assert!(t.elapsed() < Duration::from_secs(120), "后台没起来");
            }
        });
        ms.push(t.elapsed().as_secs_f64() * 1000.0);
        stop(child);
    }
    emit(
        "daemon_cold_start",
        serde_json::json!({ "events": events, "median_ms": median(ms.clone()), "samples_ms": ms }),
    );
}

fn reconnect_running(rt: &tokio::runtime::Runtime, own: &SelfIdentity) {
    let home = Home::new();
    let child = spawn(&home);
    let ms = rt.block_on(async {
        while !try_connect(&home, own).await {
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        let mut ms = Vec::new();
        for _ in 0..20 {
            let t = Instant::now();
            assert!(try_connect(&home, own).await);
            ms.push(t.elapsed().as_secs_f64() * 1000.0);
        }
        ms
    });
    stop(child);
    emit(
        "client_reconnect_running",
        serde_json::json!({ "median_ms": median(ms.clone()), "max_ms": ms.iter().cloned().fold(0.0, f64::max) }),
    );
}

fn connect_cold(rt: &tokio::runtime::Runtime, own: &SelfIdentity) {
    let mut ms = Vec::new();
    for _ in 0..5 {
        let home = Home::new();
        std::fs::create_dir_all(home.run_dir().parent().unwrap()).unwrap();
        // 拉起的后台继承这里的环境：HOME 指到临时目录，决定目录的其余变量清掉
        // SAFETY：基准是单线程顺序跑的，改环境变量时没有别的线程在读
        unsafe {
            std::env::set_var("HOME", home.path());
            for k in DIR_VARS {
                std::env::remove_var(k);
            }
        }
        let ep = LocalEndpoint::in_run_dir(&home.run_dir()).unwrap();
        let mut l = iwork_client_core::launcher::Launcher::new(
            iwork_client_core::launcher::IpcConnect {
                endpoint: ep.endpoint().clone(),
                own: own.clone(),
                cfg: client_cfg(),
            },
            BenchDetachedSpawn,
            iwork_client_core::launcher::LaunchConfig {
                daemon_exe: PathBuf::from(DAEMON),
                budget: iwork_client_core::launcher::BUDGET,
                run_dir: Some(home.run_dir()),
                endpoint: Ok(home.run_dir().join("d.sock")),
                args: vec!["--dev-trust"],
            },
        );
        let t = Instant::now();
        rt.block_on(async {
            loop {
                match l.connect_or_launch().await {
                    Ok(_) => break,
                    Err(_) if t.elapsed() < Duration::from_secs(60) => {}
                    Err(e) => panic!("{e:?}"),
                }
            }
        });
        ms.push(t.elapsed().as_secs_f64() * 1000.0);
        let pid = iwork_ipc::rendezvous::read_lock_info(&home.run_dir())
            .unwrap()
            .pid;
        // SAFETY：只结束本基准拉起的后台
        unsafe {
            libc::kill(pid as i32, libc::SIGTERM);
        }
        std::thread::sleep(Duration::from_millis(300));
    }
    emit(
        "client_connect_cold",
        serde_json::json!({ "median_ms": median(ms.clone()), "samples_ms": ms }),
    );
}

/// 进程累计的 CPU 秒数（`ps -o time=`：[[dd-]hh:]mm:ss[.ss]）。
fn cpu_secs(pid: u32) -> f64 {
    let out = Command::new("ps")
        .args(["-o", "time=", "-p", &pid.to_string()])
        .output()
        .unwrap();
    let text = String::from_utf8_lossy(&out.stdout).trim().to_owned();
    let (days, rest) = match text.split_once('-') {
        Some((d, r)) => (d.parse::<f64>().unwrap(), r.to_owned()),
        None => (0.0, text),
    };
    let parts: Vec<f64> = rest.split(':').map(|p| p.parse().unwrap()).collect();
    let secs = parts.iter().fold(0.0, |acc, p| acc * 60.0 + p);
    days * 86400.0 + secs
}

fn idle_cpu(rt: &tokio::runtime::Runtime, own: &SelfIdentity) {
    let secs: u64 = std::env::var("IWORK_BENCH_IDLE_SECS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(60);
    let home = Home::new();
    let child = spawn(&home);
    rt.block_on(async {
        while !try_connect(&home, own).await {
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    });
    std::thread::sleep(Duration::from_secs(5));
    let before = cpu_secs(child.id());
    let t = Instant::now();
    std::thread::sleep(Duration::from_secs(secs));
    let used = cpu_secs(child.id()) - before;
    let pct = used / t.elapsed().as_secs_f64() * 100.0;
    stop(child);
    emit(
        "idle_cpu",
        serde_json::json!({ "seconds": secs, "cpu_seconds": used, "cpu_percent": pct }),
    );
}

/// 单事件提交的中位耗时（微秒）。`observer` 时装真的 `PublishObserver`，发布给一个真的 ipc `Publisher`（没有连接）。
fn commits(
    rt: &tokio::runtime::Runtime,
    own: &SelfIdentity,
    observer: bool,
    load: Load,
    n: usize,
) -> f64 {
    let home = Home::new();
    let data = home.data();
    data.ensure_layout().unwrap();
    let store = Arc::new(open_store(&data));
    rt.block_on(async {
        // 叫醒那一组要一个对话可删（重复的「进删除」投影照写）
        store
            .commit(request_of(
                u128::MAX - 1,
                vec![
                    ProjectAdded {
                        project: fixtures::project(BENCH_PROJECT),
                    }
                    .into(),
                    ConversationCreated {
                        conversation: fixtures::project_conversation(BENCH_CONV, BENCH_PROJECT),
                    }
                    .into(),
                ],
            ))
            .await
            .unwrap();
        if observer {
            let (_server, publisher) = Server::new(
                ServerConfig::new("bench"),
                own.clone(),
                iwork_daemon::handler::NotWired,
                iwork_daemon::source::StoreSource::new(
                    store.clone(),
                    iwork_daemon::source::SnapshotNotWired,
                ),
            );
            let obs = iwork_daemon::publish::PublishObserver::new(
                Arc::new(publisher),
                iwork_daemon::publish::PublishState::new(),
            );
            store.set_commit_observer(Box::new(obs)).await.unwrap();
        }
        let mut us = Vec::with_capacity(n);
        for i in 0..n {
            let req = match load {
                Load::Settings => request(i as u128 + 1, 1),
                Load::Wakes => request_of(
                    i as u128 + 1,
                    vec![
                        ConversationDeleting {
                            conversation_id: fixtures::cid(BENCH_CONV),
                        }
                        .into(),
                    ],
                ),
            };
            let t = Instant::now();
            store.commit(req).await.unwrap();
            us.push(t.elapsed().as_secs_f64() * 1e6);
        }
        median(us)
    })
}

fn write_commit_observer(rt: &tokio::runtime::Runtime, own: &SelfIdentity) {
    const ROUNDS: usize = 5;
    let cells = [
        (false, Load::Settings),
        (true, Load::Settings),
        (false, Load::Wakes),
        (true, Load::Wakes),
    ];
    let mut out: [Vec<f64>; 4] = Default::default();
    for round in 0..ROUNDS {
        // 每轮换先后，负载随时间的漂移不偏向一组
        for k in 0..cells.len() {
            let i = (k + round) % cells.len();
            let (observer, load) = cells[i];
            out[i].push(commits(rt, own, observer, load, 2000));
        }
    }
    emit(
        "write_commit_observer",
        serde_json::json!({
            "rounds": ROUNDS,
            "without_observer_median_us": median(out[0].clone()),
            "with_observer_median_us": median(out[1].clone()),
            "wakes_without_observer_median_us": median(out[2].clone()),
            "wakes_with_observer_median_us": median(out[3].clone()),
            "without_rounds_us": out[0],
            "with_rounds_us": out[1],
            "wakes_without_rounds_us": out[2],
            "wakes_with_rounds_us": out[3],
        }),
    );
}

fn main() {
    // cargo bench 会给自定义 harness 传 `--bench`；按名字过滤其余参数
    let filter: Vec<String> = std::env::args()
        .skip(1)
        .filter(|a| !a.starts_with("--"))
        .collect();
    let on = |name: &str| filter.is_empty() || filter.iter().any(|f| name.contains(f.as_str()));
    let own = SelfIdentity::detect().expect("基准进程的身份");
    let rt = tokio::runtime::Runtime::new().unwrap();
    if let Ok(l) = std::fs::read_to_string("/proc/loadavg").or_else(|_| {
        Command::new("sysctl")
            .args(["-n", "vm.loadavg"])
            .output()
            .map(|o| String::from_utf8_lossy(&o.stdout).into_owned())
    }) {
        emit("environment", serde_json::json!({ "loadavg": l.trim() }));
    }
    if on("daemon_cold_start") {
        // `IWORK_BENCH_COLD_EVENTS_MAX` 只保留不超过它的几档（CI 冒烟用；空库那档总在）
        let max: usize = std::env::var("IWORK_BENCH_COLD_EVENTS_MAX")
            .ok()
            .map_or(usize::MAX, |v| {
                v.parse().expect("IWORK_BENCH_COLD_EVENTS_MAX 要是整数")
            });
        for events in [0, 10_000, 100_000].into_iter().filter(|e| *e <= max) {
            cold_start(&rt, &own, events, 3);
        }
    }
    if on("client_reconnect_running") {
        reconnect_running(&rt, &own);
    }
    if on("client_connect_cold") {
        connect_cold(&rt, &own);
    }
    if on("write_commit_observer") {
        write_commit_observer(&rt, &own);
    }
    if on("idle_cpu") {
        idle_cpu(&rt, &own);
    }
}
