//! 流式推送热路径的基准（技术设计 10.1「流式输出延迟」的后台一侧；T11 收编进 iwork-bench）。
//!
//! 场景：N 个假成员（4 / 16），各自一个运行时进程，以 100 片段/秒输出带发出时刻的正文。
//! 量的是「假运行时写出一片 → 回合编排调用 LivePublisher」这一段：ACP 编解码、管道、
//! UpdateMapper、回合编排。客户端那一段（IPC、帧合并）不在内，等 T03/T04 接上后由 T11 补全。
//!
//! 跑法（release，不进 CI 默认测试）：
//! `cargo test -p iwork-fake-agent --release --test stream_bench -- --ignored --nocapture`
//! 可用环境变量 `IWORK_BENCH_SECS` 改每个场景的时长（默认 10 秒）。

mod support;

#[path = "../../iwork-core/tests/support/history.rs"]
#[allow(dead_code, reason = "这里只用默认值")]
mod history;

use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use history::FakeHistory;
use iwork_agent_acp::AcpRuntime;
use iwork_core::turn::{SessionLine, StubDecider, TurnEntry, TurnPlan, TurnPorts};
use iwork_domain::turn::TurnState;
use iwork_ports::live::{LivePublisher, MessageDelta};
use serde_json::json;
use support::*;

const RATE_PER_SEC: u64 = 100;

/// 收到一片就按片里的发出时刻算延迟。
#[derive(Default)]
struct Latencies(Mutex<Vec<u64>>);

impl LivePublisher for Latencies {
    fn message_delta(&self, delta: MessageDelta) {
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos() as u64;
        let sent: u64 = delta
            .text
            .split('|')
            .next()
            .and_then(|s| s.parse().ok())
            .expect("片里带发出时刻");
        self.0.lock().unwrap().push(now.saturating_sub(sent));
    }
}

struct Report {
    members: usize,
    deltas: usize,
    secs: f64,
    p50: Duration,
    p95: Duration,
    p99: Duration,
    max: Duration,
    rss_start: u64,
    rss_peak: u64,
}

fn rss_of_self(system: &mut sysinfo::System) -> u64 {
    let pid = sysinfo::Pid::from_u32(std::process::id());
    system.refresh_processes(sysinfo::ProcessesToUpdate::Some(&[pid]), true);
    system.process(pid).map(|p| p.memory()).unwrap_or(0)
}

async fn run_scenario(members: usize, secs: u64) -> Report {
    let count = RATE_PER_SEC * secs;
    let script = json!({ "turns": [{ "steps": [
        { "type": "repeat_text", "chunk": "片段", "count": count, "rate_per_sec": RATE_PER_SEC, "stamp": true }
    ]}]});
    let live = Arc::new(Latencies::default());
    let mut system = sysinfo::System::new().expect("sysinfo 可用");
    let rss_start = rss_of_self(&mut system);
    let peak = Arc::new(Mutex::new(rss_start));
    let sampler = {
        let peak = peak.clone();
        tokio::spawn(async move {
            loop {
                tokio::time::sleep(Duration::from_millis(200)).await;
                let rss = rss_of_self(&mut system);
                let mut p = peak.lock().unwrap();
                *p = (*p).max(rss);
            }
        })
    };

    let started = Instant::now();
    let mut tasks = Vec::new();
    for _ in 0..members {
        let fake = Fake::new(&script);
        let live = live.clone();
        tasks.push(tokio::spawn(async move {
            let store = MemStore::default();
            let clock = RealClock::default();
            let ids = CountingIds::default();
            let timer = TokioTimer;
            let history = FakeHistory::default();
            let ports = TurnPorts {
                store: &store,
                clock: &clock,
                ids: &ids,
                live: &*live,
                timer: &timer,
                history: &history,
            };
            let plan = TurnPlan {
                changes: None,
                entry: TurnEntry::New,
                turn: queued_turn(&ids),
                assistant_name: "压测".into(),
                context_project_id: None,
                spec: spec(fake.dir.path()),
                blocks: text("开始"),
            };
            let mut line = SessionLine::new(Arc::new(StubDecider::allow_all()), None);
            let state = line
                .run_turn(&AcpRuntime::new(fake.launch.clone()), ports, plan)
                .await
                .unwrap();
            assert_eq!(state, TurnState::Completed);
        }));
    }
    for t in tasks {
        t.await.unwrap();
    }
    let secs_taken = started.elapsed().as_secs_f64();
    sampler.abort();

    let mut lat = live.0.lock().unwrap().clone();
    assert_eq!(lat.len(), members * count as usize, "每一片都要到");
    lat.sort_unstable();
    let pick = |q: f64| Duration::from_nanos(lat[((lat.len() - 1) as f64 * q) as usize]);
    Report {
        members,
        deltas: lat.len(),
        secs: secs_taken,
        p50: pick(0.50),
        p95: pick(0.95),
        p99: pick(0.99),
        max: pick(1.0),
        rss_start,
        rss_peak: *peak.lock().unwrap(),
    }
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "基准：手动或由 T11 的基准任务运行"]
async fn stream_latency_baseline() {
    let secs: u64 = std::env::var("IWORK_BENCH_SECS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(10);
    for members in [4, 16] {
        let r = run_scenario(members, secs).await;
        println!(
            "stream_bench members={} rate={}/s/member duration={:.1}s deltas={} throughput={:.0}/s \
             latency p50={:?} p95={:?} p99={:?} max={:?} rss_start={}KiB rss_peak={}KiB",
            r.members,
            RATE_PER_SEC,
            r.secs,
            r.deltas,
            r.deltas as f64 / r.secs,
            r.p50,
            r.p95,
            r.p99,
            r.max,
            r.rss_start / 1024,
            r.rss_peak / 1024,
        );
    }
}
