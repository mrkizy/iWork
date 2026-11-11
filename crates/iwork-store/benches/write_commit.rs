//! 写入基准初版（卡验收 6，方案 7.1；T11 收编进 iwork-bench）。
//!
//! 每条命令是典型三件套：用户消息 + 助手消息 + 一次回合状态变化，外加一条 outbox。
//! 三个场景：`single`（1 个生产者串行 2000 条，看单命令延迟）、`concurrent_16`（16 线程各 500 条，
//! 看组提交下的吞吐）、`approvals`（1 个生产者串行 2000 条，每条是「建审批 + 批准」，量提交时校验：
//! 状态前提读那一行、归属存续多读回合、对话、助手三行，T02 方案 2.6）。macOS 上 `fullfsync` 开、关各一组，其他平台一组（`"fullfsync": "n/a"`）。
//! 每组一行 JSON 到 stdout，人读的表到 stderr。
//!
//! 运行：`cargo bench -p iwork-store --bench write_commit`
//! 装上提交观察者再量（T04 方案第 6 节「存储写入」）：`IWORK_BENCH_OBSERVER=1 cargo bench -p iwork-store --bench write_commit`。
//! 观察者做的事和后台服务的一样重：每条事件转成 proto、算路由键，再丢进一个有界缓冲。

#[path = "../tests/support/fixtures.rs"]
mod fixtures;

use std::sync::Arc;
use std::time::{Duration, Instant};

use fixtures::*;
use iwork_domain::approval::{ApprovalState, Decider};
use iwork_domain::event::{
    ApprovalDecided, ApprovalRequested, AssistantCreated, ConversationCreated, DomainEvent,
    MessageAppended, ProjectAdded, TurnQueued, TurnStateChanged,
};
use iwork_domain::turn::TurnState;
use iwork_ports::store::StoragePort;
use iwork_store::{CommitObserver, CommittedBatch, OpenOptions, StoreConfig};

/// 和后台服务的观察者同样的工作量：转 proto + 算路由键。结果放进有界缓冲，满了就清（只为不让优化器删掉）。
struct DaemonLikeObserver {
    sink: Vec<(iwork_proto::pb::DomainEvent, usize)>,
}

impl CommitObserver for DaemonLikeObserver {
    fn committed(&mut self, batch: CommittedBatch<'_>) {
        for e in batch.events {
            let pb = iwork_app::convert::event_to_proto(&e.event).expect("转 proto");
            let routes = iwork_app::subscription::routes_for(&e.event).len();
            if self.sink.len() >= 4096 {
                self.sink.clear();
            }
            self.sink.push((pb, routes));
        }
    }
}

fn observer_on() -> bool {
    std::env::var("IWORK_BENCH_OBSERVER").is_ok_and(|v| v == "1")
}

struct Scenario {
    name: &'static str,
    threads: u128,
    per_thread: u128,
    events: fn(u128, bool) -> Vec<DomainEvent>,
}

const SCENARIOS: [Scenario; 3] = [
    Scenario {
        name: "single",
        threads: 1,
        per_thread: 2000,
        events: typical,
    },
    Scenario {
        name: "concurrent_16",
        threads: 16,
        per_thread: 500,
        events: typical,
    },
    Scenario {
        name: "approvals",
        threads: 1,
        per_thread: 2000,
        events: approvals,
    },
];

/// 建一条审批并当场批准：两条受管事件，各过一次状态前提与归属存续。
fn approvals(n: u128, _flip: bool) -> Vec<DomainEvent> {
    vec![
        ApprovalRequested {
            approval: approval(n, 20, 10),
        }
        .into(),
        ApprovalDecided {
            approval_id: apid(n),
            state: ApprovalState::Approved {
                decider: Decider::User,
            },
            remember: None,
        }
        .into(),
    ]
}

fn typical(n: u128, flip: bool) -> Vec<DomainEvent> {
    let (from, to) = if flip {
        (TurnState::Running, TurnState::WaitingApproval)
    } else {
        (TurnState::WaitingApproval, TurnState::Running)
    };
    vec![
        MessageAppended {
            message: user_message(n * 2, 10, "请帮我看看这个测试为什么失败"),
        }
        .into(),
        MessageAppended {
            message: assistant_message(n * 2 + 1, 10, 20),
        }
        .into(),
        TurnStateChanged {
            turn_id: tid(20),
            from,
            to,
            reason: None,
        }
        .into(),
    ]
}

fn percentile(sorted: &[Duration], p: f64) -> u128 {
    let idx = ((sorted.len() as f64 - 1.0) * p).round() as usize;
    sorted[idx].as_micros()
}

fn run(scenario: &Scenario, fullfsync: Option<bool>, label: &str) {
    let env = Env::new();
    let config = StoreConfig {
        fullfsync,
        ..StoreConfig::default()
    };
    let store = Arc::new(
        env.open_with(config, OpenOptions::default())
            .expect("打开存储"),
    );
    block(store.commit(cmd(
        1,
        vec![
            ProjectAdded { project: project(1) }.into(),
            // 回合的助手：审批写入前查归属存续（前后两个版本都带这一条，结果可比）
            AssistantCreated {
                assistant: assistant(1, false),
            }
            .into(),
            ConversationCreated { conversation: project_conversation(10, 1) }.into(),
            TurnQueued { turn: turn(20, 10) }.into(),
        ],
    )))
    .unwrap();
    if observer_on() {
        block(store.set_commit_observer(Box::new(DaemonLikeObserver { sink: Vec::new() })))
            .expect("装观察者");
    }

    let started = Instant::now();
    let handles: Vec<_> = (0..scenario.threads)
        .map(|t| {
            let store = store.clone();
            let per = scenario.per_thread;
            let events = scenario.events;
            std::thread::spawn(move || {
                let mut lat = Vec::with_capacity(per as usize);
                for i in 0..per {
                    let n = 1_000 + t * 1_000_000 + i;
                    let mut req = cmd(n, events(n, i % 2 == 0));
                    req.outbox = vec![outbox("runtime", b"prompt")];
                    let t0 = Instant::now();
                    block(store.commit(req)).expect("提交");
                    lat.push(t0.elapsed());
                }
                lat
            })
        })
        .collect();
    let mut lat: Vec<Duration> = handles
        .into_iter()
        .flat_map(|h| h.join().unwrap())
        .collect();
    let wall = started.elapsed();
    lat.sort_unstable();
    let commands = lat.len();
    let per_sec = commands as f64 / wall.as_secs_f64();
    let (p50, p95, p99) = (
        percentile(&lat, 0.50),
        percentile(&lat, 0.95),
        percentile(&lat, 0.99),
    );
    println!(
        "{{\"bench\":\"store_write\",\"scenario\":\"{}\",\"commands\":{commands},\"p50_us\":{p50},\"p95_us\":{p95},\
         \"p99_us\":{p99},\"commands_per_sec\":{per_sec:.1},\"batches\":{},\"fullfsync\":\"{label}\",\"observer\":{},\
         \"sqlite_version\":\"{}\",\"platform\":\"{}-{}\"}}",
        scenario.name,
        store.committed_batches(),
        observer_on(),
        rusqlite::version(),
        std::env::consts::OS,
        std::env::consts::ARCH,
    );
    eprintln!(
        "{:<14} fullfsync={:<4} observer={:<5} 命令 {commands:>5}  P50 {p50:>6}µs  P95 {p95:>6}µs  P99 {p99:>6}µs  {per_sec:>8.1} 条/秒  批数 {}",
        scenario.name,
        label,
        observer_on(),
        store.committed_batches()
    );
}

fn main() {
    // `cargo test --benches` 会带 `--bench` 以外的参数跑一遍；只在 `cargo bench` 时量
    if !std::env::args().any(|a| a == "--bench") {
        return;
    }
    let settings: &[(Option<bool>, &str)] = if cfg!(target_os = "macos") {
        &[(Some(true), "on"), (Some(false), "off")]
    } else {
        &[(None, "n/a")]
    };
    for scenario in &SCENARIOS {
        for (fullfsync, label) in settings {
            run(scenario, *fullfsync, label);
        }
    }
}
