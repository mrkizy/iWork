//! 引擎测试用的端口假实现：内存存储、计数时钟与 ID、可编排的审查运行时、可控定时器。

use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use iwork_domain::common::Timestamp;
use iwork_domain::event::DomainEvent;
use iwork_domain::id::{MessageId, RawUlid, Seq};
use iwork_ports::agent::{
    AgentError, AgentPort, AgentSession, AgentUpdate, ContentBlock, PermissionHandler,
    RuntimeCapabilities, SessionSpec, TurnOutcome, UpdateStream,
};
use iwork_ports::clock::{Clock, IdGenerator, MonotonicInstant, Timer};
use iwork_ports::store::{
    CommitReceipt, CommitRequest, DeliveryState, EventFilter, OutboxId, OutboxItem, RebuildReport,
    StorageError, StoragePort, StoredEvent,
};

#[derive(Default, Clone)]
pub struct MemStore {
    pub events: Arc<Mutex<Vec<StoredEvent>>>,
    pub fail_next: Arc<AtomicBool>,
    pub receipt_gate: Arc<Mutex<Option<Arc<tokio::sync::Notify>>>>,
}

impl MemStore {
    pub fn all(&self) -> Vec<DomainEvent> {
        self.events
            .lock()
            .unwrap()
            .iter()
            .map(|e| e.event.clone())
            .collect()
    }
    pub fn stored(&self) -> Vec<StoredEvent> {
        self.events.lock().unwrap().clone()
    }
}

impl StoragePort for MemStore {
    async fn commit(&self, req: CommitRequest) -> Result<CommitReceipt, StorageError> {
        if self.fail_next.swap(false, Ordering::SeqCst) {
            return Err(StorageError::Busy);
        }
        let receipt = {
            let mut events = self.events.lock().unwrap();
            let first = Seq(events.len() as u64 + 1);
            for e in req.events {
                let seq = Seq(events.len() as u64 + 1);
                events.push(StoredEvent {
                    seq,
                    at: Timestamp(0),
                    actor: req.actor.clone(),
                    event: e,
                });
            }
            CommitReceipt {
                first_seq: first,
                last_seq: Seq(events.len() as u64),
                replayed: false,
            }
        };
        let gate = self.receipt_gate.lock().unwrap().take();
        if let Some(gate) = gate {
            gate.notified().await;
        }
        Ok(receipt)
    }
    async fn head_seq(&self) -> Result<Seq, StorageError> {
        Ok(Seq(self.events.lock().unwrap().len() as u64))
    }
    async fn read_events(
        &self,
        _f: EventFilter,
        _from: Seq,
        _limit: usize,
    ) -> Result<Vec<StoredEvent>, StorageError> {
        Ok(self.stored())
    }
    async fn checkpoint_message(
        &self,
        _id: MessageId,
        _o: u64,
        _t: &str,
    ) -> Result<(), StorageError> {
        Ok(())
    }
    async fn outbox_pending(&self, _limit: usize) -> Result<Vec<OutboxItem>, StorageError> {
        Ok(Vec::new())
    }
    async fn outbox_mark(&self, _id: OutboxId, _s: DeliveryState) -> Result<(), StorageError> {
        Ok(())
    }
    async fn rebuild_projections(&self) -> Result<RebuildReport, StorageError> {
        unimplemented!()
    }
}

#[derive(Default)]
pub struct TickClock(AtomicU64);

impl Clock for TickClock {
    fn now(&self) -> Timestamp {
        Timestamp(self.0.fetch_add(1, Ordering::SeqCst) as i64)
    }
    fn monotonic(&self) -> MonotonicInstant {
        MonotonicInstant(self.0.fetch_add(1_000, Ordering::SeqCst))
    }
}

#[derive(Default)]
pub struct Ids(AtomicU64);

impl IdGenerator for Ids {
    fn next(&self) -> RawUlid {
        RawUlid::from_u128(u128::from(self.0.fetch_add(1, Ordering::SeqCst)) + 0x9000)
    }
}

/// 定时器：`fire` 为真时立刻到期，否则永不到期（让审查结果先回来）。
#[derive(Default, Clone)]
pub struct FakeTimer {
    pub fire: Arc<std::sync::atomic::AtomicBool>,
}

impl Timer for FakeTimer {
    async fn sleep(&self, _d: Duration) {
        if !self.fire.load(Ordering::SeqCst) {
            std::future::pending::<()>().await;
        }
    }
}

/// 审查运行时的脚本：每次开会话取一条。
#[derive(Debug, Clone)]
pub enum ReviewScript {
    /// 输出这段文字后正常结束
    Says(String),
    /// 输出永不结束（配合定时器到期 = 超时）
    Hangs,
    /// 运行时进程退出
    Crashes,
    /// 等 `gate` 打开后再输出
    SaysAfter(String, Arc<tokio::sync::Notify>),
}

#[derive(Default, Clone)]
pub struct FakeReviewer {
    pub scripts: Arc<Mutex<VecDeque<ReviewScript>>>,
    pub specs: Arc<Mutex<Vec<SessionSpec>>>,
}

impl FakeReviewer {
    pub fn push(&self, s: ReviewScript) {
        self.scripts.lock().unwrap().push_back(s);
    }
}

pub struct FakeReviewSession(ReviewScript);
pub struct FakeReviewUpdates(
    VecDeque<AgentUpdate>,
    Option<Arc<tokio::sync::Notify>>,
    bool,
);

impl AgentPort for FakeReviewer {
    type Session = FakeReviewSession;
    async fn capabilities(&self) -> Result<RuntimeCapabilities, AgentError> {
        unimplemented!()
    }
    async fn open_session<H: PermissionHandler + 'static>(
        &self,
        spec: SessionSpec,
        _h: H,
    ) -> Result<FakeReviewSession, AgentError> {
        self.specs.lock().unwrap().push(spec);
        let s = self
            .scripts
            .lock()
            .unwrap()
            .pop_front()
            .unwrap_or(ReviewScript::Crashes);
        Ok(FakeReviewSession(s))
    }
}

impl AgentSession for FakeReviewSession {
    type Updates = FakeReviewUpdates;
    fn runtime_session_id(&self) -> Option<&str> {
        None
    }
    async fn prompt(&self, _b: Vec<ContentBlock>) -> Result<FakeReviewUpdates, AgentError> {
        Ok(match &self.0 {
            ReviewScript::Says(t) => FakeReviewUpdates(
                [
                    AgentUpdate::Text {
                        offset: 0,
                        text: t.clone(),
                    },
                    AgentUpdate::Finished(TurnOutcome::Completed),
                ]
                .into(),
                None,
                false,
            ),
            ReviewScript::SaysAfter(t, gate) => FakeReviewUpdates(
                [
                    AgentUpdate::Text {
                        offset: 0,
                        text: t.clone(),
                    },
                    AgentUpdate::Finished(TurnOutcome::Completed),
                ]
                .into(),
                Some(gate.clone()),
                false,
            ),
            ReviewScript::Hangs => FakeReviewUpdates(VecDeque::new(), None, true),
            ReviewScript::Crashes => FakeReviewUpdates(
                [AgentUpdate::Failed(AgentError::ProcessExited {
                    message: "boom".into(),
                })]
                .into(),
                None,
                false,
            ),
        })
    }
    async fn cancel(&self) -> Result<(), AgentError> {
        Ok(())
    }
    async fn close(self) -> Result<(), AgentError> {
        Ok(())
    }
}

impl UpdateStream for FakeReviewUpdates {
    async fn next(&mut self) -> Option<AgentUpdate> {
        if let Some(gate) = self.1.take() {
            gate.notified().await;
        }
        if self.2 {
            std::future::pending::<()>().await;
        }
        self.0.pop_front()
    }
}
