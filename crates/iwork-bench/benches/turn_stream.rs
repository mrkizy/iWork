//! 回合编排的流式热路径（T06 方案第 14 节）：一个进程内的假会话连发 10 万条正文片段，量 `run_turn`
//! 每处理一条的耗时。不起进程、不经 ACP，只量编排循环本身（多路输入调度、交接表、检查点、推送）。
//!
//! 跑法：`cargo bench -p iwork-bench --bench turn_stream`，每组一行 JSON 到 stdout。
//! `IWORK_BENCH_TURN_UPDATES` 改片段数（默认 100000），`IWORK_BENCH_TURN_REPS` 改重复次数（默认 7，报中位数）。

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use iwork_core::turn::{SessionLine, StubDecider, TurnPlan, TurnPorts};
use iwork_domain::common::{ModelId, ModelSelection, Timestamp, ToolGroups};
use iwork_domain::id::{AssistantId, ConversationId, MessageId, RawUlid, RuntimeId, Seq, TurnId};
use iwork_domain::session::{AllowWrite, NetworkPolicy, SessionConstraints, SessionToolSet};
use iwork_domain::turn::{Turn, TurnKind, TurnState};
use iwork_ports::agent::{
    AgentError, AgentPort, AgentSession, AgentUpdate, ContentBlock, PermissionHandler,
    RuntimeCapabilities, SandboxStatus, SessionSpec, TurnOutcome, UpdateStream,
};
use iwork_ports::clock::{Clock, IdGenerator, MonotonicInstant, Timer};
use iwork_ports::live::{LivePublisher, MessageDelta};
use iwork_ports::store::{
    CommitReceipt, CommitRequest, DeliveryState, EventFilter, OutboxId, OutboxItem, RebuildReport,
    StorageError, StoragePort, StoredEvent,
};
use tokio::sync::mpsc;

/// 假会话：prompt 时把 `n` 条正文片段和结束一次性放进队列。
struct Flood(u64);
struct FloodSession(u64);
struct Updates(mpsc::UnboundedReceiver<AgentUpdate>);

impl AgentPort for Flood {
    type Session = FloodSession;
    async fn capabilities(&self) -> Result<RuntimeCapabilities, AgentError> {
        unimplemented!()
    }
    async fn open_session<H: PermissionHandler + 'static>(
        &self,
        _spec: SessionSpec,
        _on_permission: H,
    ) -> Result<FloodSession, AgentError> {
        Ok(FloodSession(self.0))
    }
}

impl AgentSession for FloodSession {
    type Updates = Updates;
    fn runtime_session_id(&self) -> Option<&str> {
        Some("bench")
    }
    async fn prompt(&self, _blocks: Vec<ContentBlock>) -> Result<Updates, AgentError> {
        let (tx, rx) = mpsc::unbounded_channel();
        for i in 0..self.0 {
            let _ = tx.send(AgentUpdate::Text {
                offset: i,
                text: "x".into(),
            });
        }
        let _ = tx.send(AgentUpdate::Finished(TurnOutcome::Completed));
        Ok(Updates(rx))
    }
    fn permission_wait_limit(&self) -> Option<Duration> {
        None
    }
    fn sandbox(&self) -> SandboxStatus {
        SandboxStatus::Enforced
    }
    async fn cancel(&self) -> Result<(), AgentError> {
        Ok(())
    }
    async fn terminate(&self) -> Result<(), AgentError> {
        Ok(())
    }
    async fn close(self) -> Result<(), AgentError> {
        Ok(())
    }
}

impl UpdateStream for Updates {
    async fn next(&mut self) -> Option<AgentUpdate> {
        self.0.recv().await
    }
}

struct Store;

impl StoragePort for Store {
    async fn commit(&self, req: CommitRequest) -> Result<CommitReceipt, StorageError> {
        std::hint::black_box(&req);
        Ok(CommitReceipt {
            first_seq: Seq(1),
            last_seq: Seq(1),
            replayed: false,
        })
    }
    async fn head_seq(&self) -> Result<Seq, StorageError> {
        unimplemented!()
    }
    async fn read_events(
        &self,
        _f: EventFilter,
        _from: Seq,
        _limit: usize,
    ) -> Result<Vec<StoredEvent>, StorageError> {
        unimplemented!()
    }
    async fn checkpoint_message(
        &self,
        _id: MessageId,
        _offset: u64,
        text: &str,
    ) -> Result<(), StorageError> {
        std::hint::black_box(text);
        Ok(())
    }
    async fn outbox_pending(&self, _limit: usize) -> Result<Vec<OutboxItem>, StorageError> {
        unimplemented!()
    }
    async fn outbox_mark(&self, _id: OutboxId, _s: DeliveryState) -> Result<(), StorageError> {
        unimplemented!()
    }
    async fn rebuild_projections(&self) -> Result<RebuildReport, StorageError> {
        unimplemented!()
    }
}

struct RealClock(Instant);

impl Clock for RealClock {
    fn now(&self) -> Timestamp {
        Timestamp(self.0.elapsed().as_millis() as i64)
    }
    fn monotonic(&self) -> MonotonicInstant {
        MonotonicInstant(self.0.elapsed().as_nanos() as u64)
    }
}

#[derive(Default)]
struct Ids(AtomicU64);

impl IdGenerator for Ids {
    fn next(&self) -> RawUlid {
        RawUlid::from_u128(u128::from(self.0.fetch_add(1, Ordering::Relaxed)) + 1)
    }
}

struct Live;

impl LivePublisher for Live {
    fn message_delta(&self, delta: MessageDelta) {
        std::hint::black_box(delta);
    }
}

struct TokioTimer;

impl Timer for TokioTimer {
    async fn sleep(&self, duration: Duration) {
        tokio::time::sleep(duration).await;
    }
}

fn plan(n: u128) -> TurnPlan {
    let id = RawUlid::from_u128(0x1000 + n);
    let model = ModelSelection {
        model_id: ModelId("m".into()),
        effort: None,
    };
    TurnPlan {
        turn: Turn {
            id: TurnId::from_raw(id),
            conversation_id: ConversationId::from_raw(id),
            assistant_id: AssistantId::from_raw(id),
            assistant_version: 1,
            runtime_id: RuntimeId::from_raw(id),
            model: model.clone(),
            runtime_session_id: None,
            kind: TurnKind::Assistant,
            state: TurnState::Queued,
            started_at: Timestamp(0),
            ended_at: None,
        },
        assistant_name: "bench".into(),
        context_project_id: None,
        spec: SessionSpec {
            cwd: "/work".into(),
            model,
            system_prompt: String::new(),
            mcp: Vec::new(),
            constraints: SessionConstraints {
                deny_read_write: Vec::new(),
                read_only: Vec::new(),
                allow_write: AllowWrite::Anywhere,
                excluded_commands: Vec::new(),
                allow_unsandboxed_retry: false,
                network: NetworkPolicy::Open,
            },
            tool_set: SessionToolSet {
                enabled_groups: ToolGroups::new(),
                include_always_on: true,
            },
            disabled_tools: Vec::new(),
            plugin_dirs: Vec::new(),
            resume: None,
            kind: TurnKind::Assistant,
        },
        blocks: vec![ContentBlock::Text("hi".into())],
    }
}

fn env_or(key: &str, default: u64) -> u64 {
    std::env::var(key)
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(default)
}

fn main() {
    let updates = env_or("IWORK_BENCH_TURN_UPDATES", 100_000);
    let reps = env_or("IWORK_BENCH_TURN_REPS", 7).max(1);
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_time()
        .build()
        .expect("运行时");
    let mut per_update = Vec::new();
    for rep in 0..reps {
        let ns = rt.block_on(async {
            let mut line = SessionLine::new(Arc::new(StubDecider::allow_all()), None);
            let clock = RealClock(Instant::now());
            let ports = TurnPorts {
                store: &Store,
                clock: &clock,
                ids: &Ids::default(),
                live: &Live,
                timer: &TokioTimer,
            };
            let started = Instant::now();
            let state = line
                .run_turn(&Flood(updates), ports, plan(u128::from(rep)))
                .await
                .expect("回合跑完");
            assert_eq!(state, TurnState::Completed);
            started.elapsed().as_nanos() as f64 / updates as f64
        });
        per_update.push(ns);
    }
    per_update.sort_by(f64::total_cmp);
    let median = per_update[per_update.len() / 2];
    println!(
        "{}",
        serde_json::json!({
            "bench": "turn_stream",
            "updates": updates,
            "reps": reps,
            "ns_per_update_median": (median * 10.0).round() / 10.0,
            "ns_per_update_min": (per_update[0] * 10.0).round() / 10.0,
            "ns_per_update_max": (per_update[per_update.len() - 1] * 10.0).round() / 10.0,
        })
    );
}
