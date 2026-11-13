//! 引擎测试用的端口假实现：内存存储、计数时钟与 ID、可编排的审查运行时、可控定时器。

use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use super::reference_projection::{ReferenceProjection, apply_all};
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

/// 内存存储，照生产写线程的纪律：入队（同步、不可取消）→ 按入队顺序处理 → 回执。处理在独立任务里跑，
/// 调用方的 future 被丢弃，已入队的那笔照样落盘。提交时用参照投影做同一组状态前提与归属校验
/// （T02 方案 2.6），不通过整笔回滚、报 `Projection`；读事件按流过滤。
///
/// 三个门：`enqueue_gate` 让下一个 commit 停在入队门口（还没入队）；`before_commit` 让它入队后停在处理之前
/// （排在它后面的都得等）；`receipt_gate` 让它处理完、回执还没送到。
#[derive(Default, Clone)]
pub struct MemStore {
    pub enqueue_gate: Arc<Mutex<Option<Arc<tokio::sync::Notify>>>>,
    /// 有一个 commit 停在入队门口
    pub at_enqueue_gate: Arc<AtomicBool>,
    /// 到过入队门口的次数
    pub gate_arrivals: Arc<AtomicU64>,
    queue: Arc<Mutex<Queue>>,
    queue_moved: Arc<tokio::sync::Notify>,
    pub events: Arc<Mutex<Vec<StoredEvent>>>,
    pub projection: Arc<Mutex<ReferenceProjection>>,
    pub fail_next: Arc<AtomicBool>,
    /// 为真期间每一笔提交都回 Busy（持续阻住，不是只回一次）
    pub fail_all: Arc<AtomicBool>,
    /// 下一次读事件报 `Io`
    pub fail_read: Arc<AtomicBool>,
    /// 处理过的提交笔数（含空提交、含被拒的）
    pub commits: Arc<AtomicU64>,
    /// 处理到第几笔（从 1 数，含空提交）时返回 Busy；0 表示不注入
    pub fail_commit_at: Arc<AtomicU64>,
    /// 被提交时校验拒掉（Projection）的笔数
    pub rejections: Arc<AtomicU64>,
    /// 处理过的空提交（屏障）笔数
    pub empty_commits: Arc<AtomicU64>,
    pub commit_entered: Arc<AtomicBool>,
    pub before_commit: Arc<Mutex<Option<Arc<tokio::sync::Notify>>>>,
    pub receipt_gate: Arc<Mutex<Option<Arc<tokio::sync::Notify>>>>,
}

/// 票号：入队时领一张，处理按票号顺序。
#[derive(Default)]
struct Queue {
    next: u64,
    serving: u64,
}

impl MemStore {
    /// 已入队、还没处理完的笔数。
    pub fn queued(&self) -> u64 {
        let q = self.queue.lock().unwrap();
        q.next - q.serving
    }

    /// 处理一笔（不看门）：提交时校验、追加日志、回执。
    fn process(&self, req: CommitRequest) -> Result<CommitReceipt, StorageError> {
        let mut events = self.events.lock().unwrap();
        self.commits.fetch_add(1, Ordering::SeqCst);
        if req.events.is_empty() {
            self.empty_commits.fetch_add(1, Ordering::SeqCst);
        }
        if self.fail_all.load(Ordering::SeqCst)
            || self.fail_next.swap(false, Ordering::SeqCst)
            || self.fail_commit_at.load(Ordering::SeqCst) == self.commits.load(Ordering::SeqCst)
        {
            return Err(StorageError::Busy);
        }
        let first = Seq(events.len() as u64 + 1);
        apply_all(&mut self.projection.lock().unwrap(), &req.events).map_err(|message| {
            self.rejections.fetch_add(1, Ordering::SeqCst);
            StorageError::Projection {
                seq: first,
                message,
            }
        })?;
        for e in req.events {
            let seq = Seq(events.len() as u64 + 1);
            events.push(StoredEvent {
                seq,
                at: Timestamp(0),
                actor: req.actor.clone(),
                event: e,
            });
        }
        Ok(CommitReceipt {
            first_seq: first,
            last_seq: Seq(events.len() as u64),
            replayed: false,
        })
    }

    async fn process_in_turn(
        &self,
        ticket: u64,
        req: CommitRequest,
    ) -> Result<CommitReceipt, StorageError> {
        loop {
            // 先登记再检查，检查和等待之间的叫醒不会丢
            let moved = self.queue_moved.notified();
            tokio::pin!(moved);
            moved.as_mut().enable();
            if self.queue.lock().unwrap().serving == ticket {
                break;
            }
            moved.await;
        }
        let before = self.before_commit.lock().unwrap().take();
        if let Some(before) = before {
            self.commit_entered.store(true, Ordering::SeqCst);
            before.notified().await;
        }
        let result = self.process(req);
        self.queue.lock().unwrap().serving += 1;
        self.queue_moved.notify_waiters();
        result
    }

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
    /// 不经引擎直接写一笔（扮演编排、T10 或测试布景），同步处理；队列里还有没处理的提交、或被拒时 panic。
    pub fn seed(&self, events: Vec<DomainEvent>) {
        assert_eq!(self.queued(), 0, "布景时队列里还有没处理的提交");
        let mut q = self.queue.lock().unwrap();
        self.process(CommitRequest {
            command_id: iwork_domain::id::CommandId::from_raw(RawUlid::from_u128(0)),
            actor: iwork_domain::common::Actor("test".into()),
            events,
            outbox: Vec::new(),
        })
        .expect("布景事件应当被接受");
        q.next += 1;
        q.serving += 1;
    }
}

fn selected(filter: &EventFilter, event: &DomainEvent) -> bool {
    match filter {
        EventFilter::All => true,
        EventFilter::Streams(selectors) => {
            let (kind, id) = event.stream();
            selectors
                .iter()
                .any(|s| s.kind == kind && (s.id.is_none() || s.id == id))
        }
    }
}

impl StoragePort for MemStore {
    async fn commit(&self, req: CommitRequest) -> Result<CommitReceipt, StorageError> {
        // 经端口提交的事件（引擎自己的，或 T10 经 commit_with 交给引擎的）一律带所属对话（G5，T10a 方案 2.7
        // 「怎么守住新构造的一律 Some」）。查的是真引擎交给存储的载荷原样，不经任何补齐；测试布景走 `seed`，不在此列
        for e in &req.events {
            assert_ne!(
                e.carried_conversation(),
                Some(None),
                "{} 没带所属对话：{e:?}",
                e.kind()
            );
        }
        let gate = self.enqueue_gate.lock().unwrap().take();
        if let Some(gate) = gate {
            self.at_enqueue_gate.store(true, Ordering::SeqCst);
            self.gate_arrivals.fetch_add(1, Ordering::SeqCst);
            gate.notified().await;
            self.at_enqueue_gate.store(false, Ordering::SeqCst);
        }
        // 入队：同步领票，之后调用方被丢弃也不影响这笔被处理
        let ticket = {
            let mut q = self.queue.lock().unwrap();
            q.next += 1;
            q.next - 1
        };
        let this = self.clone();
        let processed = tokio::spawn(async move { this.process_in_turn(ticket, req).await });
        let receipt = processed.await.expect("处理任务不 panic")?;
        let gate = self.receipt_gate.lock().unwrap().take();
        if let Some(gate) = gate {
            gate.notified().await;
        }
        Ok(receipt)
    }
    async fn command_receipt(
        &self,
        _id: iwork_domain::id::CommandId,
    ) -> Result<Option<iwork_ports::store::CommitReceipt>, iwork_ports::store::StorageError> {
        unimplemented!("这个假存储的使用者不查回执")
    }
    async fn head_seq(&self) -> Result<Seq, StorageError> {
        Ok(Seq(self.events.lock().unwrap().len() as u64))
    }
    async fn read_events(
        &self,
        filter: EventFilter,
        from: Seq,
        limit: usize,
    ) -> Result<Vec<StoredEvent>, StorageError> {
        if self.fail_read.swap(false, Ordering::SeqCst) {
            return Err(StorageError::Io {
                message: "测试注入的读失败".into(),
            });
        }
        Ok(self
            .stored()
            .into_iter()
            .filter(|e| e.seq > from && selected(&filter, &e.event))
            .take(limit)
            .collect())
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
    async fn outbox_requeue_dispatched(
        &self,
        _redeliver_prefixes: Vec<String>,
        _fail_prefixes: Vec<String>,
    ) -> Result<iwork_ports::store::RequeueReport, StorageError> {
        Ok(iwork_ports::store::RequeueReport::default())
    }
    async fn outbox_pending_where(
        &self,
        _selector: iwork_ports::store::OutboxSelector,
        _limit: usize,
    ) -> Result<Vec<OutboxItem>, StorageError> {
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

/// 定时器：可立即到期，也可由 expire 精确触发；默认保持等待。
#[derive(Default, Clone)]
pub struct FakeTimer {
    pub fire: Arc<std::sync::atomic::AtomicBool>,
    pub expired: Arc<tokio::sync::Notify>,
}

impl Timer for FakeTimer {
    async fn sleep(&self, _d: Duration) {
        if !self.fire.load(Ordering::SeqCst) {
            self.expired.notified().await;
        }
    }
}

impl FakeTimer {
    pub fn expire(&self) {
        self.fire.store(true, Ordering::SeqCst);
        self.expired.notify_one();
    }
}

/// 审查运行时的脚本：每次开会话取一条。
#[derive(Debug, Clone)]
pub enum ReviewScript {
    /// 输出这段文字后正常结束
    Says(String),
    /// 输出永不结束（配合定时器到期 = 超时）
    Hangs,
    OpenHangs,
    CloseHangs(String),
    CancelHangs,
    Updates(Vec<AgentUpdate>),
    /// 运行时进程退出
    Crashes,
    /// 等 `gate` 打开后再输出
    SaysAfter(String, Arc<tokio::sync::Notify>),
}

#[derive(Default, Clone)]
pub struct FakeReviewer {
    pub scripts: Arc<Mutex<VecDeque<ReviewScript>>>,
    pub specs: Arc<Mutex<Vec<SessionSpec>>>,
    pub inputs: Arc<Mutex<Vec<Vec<ContentBlock>>>>,
    pub phases: Arc<Mutex<Vec<&'static str>>>,
}

impl FakeReviewer {
    pub fn push(&self, s: ReviewScript) {
        self.scripts.lock().unwrap().push_back(s);
    }
}

pub struct FakeReviewSession(
    ReviewScript,
    Arc<Mutex<Vec<Vec<ContentBlock>>>>,
    Arc<Mutex<Vec<&'static str>>>,
);
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
        self.phases.lock().unwrap().push("open");
        let s = self
            .scripts
            .lock()
            .unwrap()
            .pop_front()
            .unwrap_or(ReviewScript::Crashes);
        if matches!(s, ReviewScript::OpenHangs) {
            std::future::pending::<()>().await;
        }
        Ok(FakeReviewSession(
            s,
            self.inputs.clone(),
            self.phases.clone(),
        ))
    }
}

impl AgentSession for FakeReviewSession {
    type Updates = FakeReviewUpdates;
    fn runtime_session_id(&self) -> Option<&str> {
        None
    }
    fn permission_wait_limit(&self) -> Option<Duration> {
        None
    }
    fn sandbox(&self) -> iwork_ports::agent::SandboxStatus {
        iwork_ports::agent::SandboxStatus::Enforced
    }
    async fn terminate(&self) -> Result<(), AgentError> {
        Ok(())
    }
    async fn prompt(&self, b: Vec<ContentBlock>) -> Result<FakeReviewUpdates, AgentError> {
        self.1.lock().unwrap().push(b);
        self.2.lock().unwrap().push("prompt");
        Ok(match &self.0 {
            ReviewScript::Says(t) | ReviewScript::CloseHangs(t) => FakeReviewUpdates(
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
            ReviewScript::Updates(updates) => {
                FakeReviewUpdates(updates.clone().into(), None, false)
            }
            ReviewScript::Hangs | ReviewScript::OpenHangs | ReviewScript::CancelHangs => {
                FakeReviewUpdates(VecDeque::new(), None, true)
            }
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
        self.2.lock().unwrap().push("cancel");
        if matches!(self.0, ReviewScript::CancelHangs) {
            std::future::pending::<()>().await;
        }
        Ok(())
    }
    async fn close(self) -> Result<(), AgentError> {
        self.2.lock().unwrap().push("close");
        if matches!(self.0, ReviewScript::CloseHangs(_)) {
            std::future::pending::<()>().await;
        }
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
