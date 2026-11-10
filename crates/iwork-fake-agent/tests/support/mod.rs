//! 集成测试的公共部件：假运行时的启动配置、内存存储、时钟与 ID、增量收集、事件快照。

#![allow(dead_code)] // 各测试文件只用到其中一部分

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Instant;

use iwork_agent_acp::RuntimeLaunch;
use iwork_domain::common::{Actor, ModelId, ModelSelection, Timestamp, ToolGroup};
use iwork_domain::event::DomainEvent;
use iwork_domain::id::{AssistantId, ConversationId, MessageId, RawUlid, RuntimeId, Seq, TurnId};
use iwork_domain::message::MessageBody;
use iwork_domain::session::{AllowWrite, NetworkPolicy, SessionConstraints, SessionToolSet};
use iwork_domain::turn::{Turn, TurnKind, TurnState};
use iwork_ports::agent::{ContentBlock, SessionSpec};
use iwork_ports::clock::{Clock, IdGenerator, MonotonicInstant, Timer};
use iwork_ports::live::{LivePublisher, MessageDelta};
use iwork_ports::store::{
    CommitReceipt, CommitRequest, DeliveryState, EventFilter, OutboxId, OutboxItem, RebuildReport,
    StorageError, StoragePort, StoredEvent,
};
use serde_json::Value;

pub const FAKE_AGENT: &str = env!("CARGO_BIN_EXE_iwork-fake-agent");

/// 一份脚本及其记录文件，放在临时目录里，随它一起删掉。
pub struct Fake {
    pub dir: tempfile::TempDir,
    pub launch: RuntimeLaunch,
    pub record: PathBuf,
}

impl Fake {
    pub fn new(script: &Value) -> Self {
        let dir = tempfile::tempdir().expect("建临时目录");
        let script_path = dir.path().join("script.json");
        std::fs::write(&script_path, script.to_string()).expect("写脚本");
        let record = dir.path().join("record.jsonl");
        let launch = RuntimeLaunch {
            program: PathBuf::from(FAKE_AGENT),
            args: vec![
                "--script".into(),
                script_path.into_os_string(),
                "--record".into(),
                record.clone().into_os_string(),
            ],
            env: Vec::new(),
        };
        Self {
            dir,
            launch,
            record,
        }
    }

    /// 记录文件里的全部条目。
    pub fn records(&self) -> Vec<Value> {
        read_records(&self.record)
    }

    /// 记录里某个方法收到的全部参数。
    pub fn params_of(&self, method: &str) -> Vec<Value> {
        self.records()
            .into_iter()
            .filter(|r| r["method"] == method)
            .map(|r| r["params"].clone())
            .collect()
    }
}

pub fn read_records(path: &Path) -> Vec<Value> {
    match std::fs::read_to_string(path) {
        Ok(text) => text
            .lines()
            .map(|l| serde_json::from_str(l).expect("记录是 JSON"))
            .collect(),
        Err(_) => Vec::new(),
    }
}

pub fn spec(cwd: &Path) -> SessionSpec {
    SessionSpec {
        cwd: cwd.to_path_buf(),
        model: ModelSelection {
            model_id: ModelId("fake-model".into()),
            effort: None,
        },
        system_prompt: "你是测试助手".into(),
        mcp: Vec::new(),
        constraints: SessionConstraints {
            deny_read_write: vec![PathBuf::from("/secret")],
            read_only: Vec::new(),
            allow_write: AllowWrite::Whitelist(vec![cwd.to_path_buf()]),
            excluded_commands: Vec::new(),
            allow_unsandboxed_retry: true,
            network: NetworkPolicy::Open,
        },
        tool_set: SessionToolSet {
            enabled_groups: BTreeSet::from([ToolGroup::ReadAndSearch, ToolGroup::RunCommands]),
            include_always_on: true,
        },
        resume: None,
        kind: TurnKind::Assistant,
    }
}

pub fn text(s: &str) -> Vec<ContentBlock> {
    vec![ContentBlock::Text(s.into())]
}

// ---------- 时钟与 ID ----------

/// 真实单调时钟：检查点间隔按真实时间算。
pub struct RealClock {
    start: Instant,
}

impl Default for RealClock {
    fn default() -> Self {
        Self {
            start: Instant::now(),
        }
    }
}

impl Clock for RealClock {
    fn now(&self) -> Timestamp {
        Timestamp(1_700_000_000_000 + self.start.elapsed().as_millis() as i64)
    }
    fn monotonic(&self) -> MonotonicInstant {
        MonotonicInstant(self.start.elapsed().as_nanos() as u64)
    }
}

/// tokio 定时器。
#[derive(Default)]
pub struct TokioTimer;

impl Timer for TokioTimer {
    async fn sleep(&self, duration: std::time::Duration) {
        tokio::time::sleep(duration).await;
    }
}

/// 递增 ID：测试里只需要唯一。
#[derive(Default)]
pub struct CountingIds(AtomicU64);

impl IdGenerator for CountingIds {
    fn next(&self) -> RawUlid {
        RawUlid::from_u128(u128::from(self.0.fetch_add(1, Ordering::SeqCst)) + 1_000)
    }
}

pub fn queued_turn(ids: &CountingIds) -> Turn {
    Turn {
        id: TurnId::from_raw(ids.next()),
        conversation_id: ConversationId::from_raw(ids.next()),
        assistant_id: AssistantId::from_raw(ids.next()),
        assistant_version: 1,
        runtime_id: RuntimeId::from_raw(ids.next()),
        model: ModelSelection {
            model_id: ModelId("fake-model".into()),
            effort: None,
        },
        runtime_session_id: None,
        kind: TurnKind::Assistant,
        state: TurnState::Queued,
        started_at: Timestamp(1),
        ended_at: None,
    }
}

// ---------- 存储与推送 ----------

/// 内存存储：只记提交的事件与检查点。
#[derive(Default)]
pub struct MemStore {
    events: Mutex<Vec<DomainEvent>>,
    checkpoints: Mutex<Vec<(MessageId, u64, String)>>,
}

impl MemStore {
    pub fn events(&self) -> Vec<DomainEvent> {
        self.events.lock().unwrap().clone()
    }

    pub fn checkpoints(&self) -> Vec<(MessageId, u64, String)> {
        self.checkpoints.lock().unwrap().clone()
    }

    /// 事件的快照：每条一行，去掉 ID 与时间，只留判断对错要看的内容。
    pub fn snapshot(&self) -> Vec<String> {
        self.events().iter().map(describe).collect()
    }

    /// 最后一次回合状态变化后的状态。
    pub fn last_turn_state(&self) -> Option<TurnState> {
        self.events().iter().rev().find_map(|e| match e {
            DomainEvent::TurnStateChanged(c) => Some(c.to),
            _ => None,
        })
    }
}

pub fn describe(e: &DomainEvent) -> String {
    match e {
        DomainEvent::TurnStateChanged(c) => match &c.reason {
            Some(r) => format!("turn {:?} -> {:?} ({r})", c.from, c.to),
            None => format!("turn {:?} -> {:?}", c.from, c.to),
        },
        DomainEvent::MessageAppended(m) => format!("message appended {:?}", m.message.status),
        DomainEvent::MessageCompleted(m) => match &m.body {
            MessageBody::Inline(text) => format!("message {:?} {text:?}", m.status),
            MessageBody::Artifact(_) => format!("message {:?} <artifact>", m.status),
        },
        DomainEvent::ToolCallStarted(t) => {
            format!("tool started {} {}", t.tool_call_id.0, t.tool_name)
        }
        DomainEvent::ToolCallFinished(t) => {
            format!("tool finished {} ok={}", t.tool_call_id.0, t.ok)
        }
        DomainEvent::TurnFinished(t) => format!("turn finished {:?}", t.state),
        other => other.kind().to_string(),
    }
}

impl StoragePort for MemStore {
    async fn commit(&self, req: CommitRequest) -> Result<CommitReceipt, StorageError> {
        assert_eq!(req.actor, Actor("system".into()));
        let mut events = self.events.lock().unwrap();
        let first = events.len() as u64 + 1;
        events.extend(req.events);
        Ok(CommitReceipt {
            first_seq: Seq(first),
            last_seq: Seq(events.len() as u64),
            replayed: false,
        })
    }

    async fn head_seq(&self) -> Result<Seq, StorageError> {
        Ok(Seq(self.events.lock().unwrap().len() as u64))
    }

    async fn read_events(
        &self,
        _filter: EventFilter,
        _from: Seq,
        _limit: usize,
    ) -> Result<Vec<StoredEvent>, StorageError> {
        unimplemented!("回合编排不读事件")
    }

    async fn checkpoint_message(
        &self,
        message_id: MessageId,
        offset: u64,
        text: &str,
    ) -> Result<(), StorageError> {
        self.checkpoints
            .lock()
            .unwrap()
            .push((message_id, offset, text.to_owned()));
        Ok(())
    }

    async fn outbox_pending(&self, _limit: usize) -> Result<Vec<OutboxItem>, StorageError> {
        unimplemented!("回合编排不读 outbox")
    }

    async fn outbox_mark(&self, _id: OutboxId, _state: DeliveryState) -> Result<(), StorageError> {
        unimplemented!("回合编排不读 outbox")
    }

    async fn rebuild_projections(&self) -> Result<RebuildReport, StorageError> {
        unimplemented!("回合编排不重建投影")
    }
}

/// 收集推出去的正文增量。
#[derive(Default)]
pub struct Deltas(Mutex<Vec<MessageDelta>>);

impl Deltas {
    pub fn all(&self) -> Vec<MessageDelta> {
        self.0.lock().unwrap().clone()
    }
}

impl LivePublisher for Deltas {
    fn message_delta(&self, delta: MessageDelta) {
        self.0.lock().unwrap().push(delta);
    }
}
