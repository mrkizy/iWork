//! 一步一步走的假存储（T07 方案 6.10.13「假存储」）：测试决定每一笔什么时候入队、什么时候处理、
//! 回执什么时候送到。不依赖异步运行时，配合测试里手写的单线程执行器，每个种子完全可重放。
//!
//! 纪律和生产写线程一样：**只按入队顺序处理，`step()` 永远处理队头**，没有「先处理后入队的那一笔」的接口。
//! 要两个写者按指定先后落盘，改的是入队的先后：`gate_next_commit()` 让下一个到达的 `commit` 停在入队门口
//! （还没入队），测试让别的写者先入队，再开门。
//!
//! 提交时用参照投影做同一组状态前提与归属校验（调 domain 的同一个函数），不通过整笔回滚、报 `Projection`。
//! 观察者钩子照 T04 的契约在 `step()` 里、回执可送达之前同步调用；钩子执行期间对存储的任何读写都 panic
//! （模拟「写线程上不许 I/O」）。

#![allow(dead_code, reason = "各测试各用一部分")]

use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::future::poll_fn;
use std::sync::{Arc, Mutex};
use std::task::Poll;

use iwork_domain::common::Timestamp;
use iwork_domain::event::DomainEvent;
use iwork_domain::id::{MessageId, Seq};
use iwork_ports::store::{
    CommitReceipt, CommitRequest, DeliveryState, EventFilter, OutboxId, OutboxItem, RebuildReport,
    StorageError, StoragePort, StoredEvent,
};

use super::reference_projection::{ReferenceProjection, apply_all};

/// 观察者：一笔处理完、回执送达之前调用。
pub type Observer = Box<dyn FnMut(&[StoredEvent]) + Send>;

#[derive(Default)]
struct Inner {
    log: Vec<StoredEvent>,
    projection: ReferenceProjection,
    /// 入队了、还没处理的：（票号，请求）
    queue: VecDeque<(u64, CommitRequest)>,
    /// 处理完、回执还没送到的
    processed: BTreeMap<u64, Result<CommitReceipt, StorageError>>,
    /// 回执已送到、等调用方取的
    delivered: BTreeMap<u64, Result<CommitReceipt, StorageError>>,
    next_ticket: u64,
    /// 下一个到达的 commit 停在入队门口
    gate_armed: bool,
    /// 停在门口的：门号 → 是否已开
    gates: BTreeMap<u64, bool>,
    next_gate: u64,
    fail_next: bool,
    fail_read: bool,
    in_observer: bool,
    commits: u64,
    empty_commits: u64,
}

#[derive(Clone, Default)]
pub struct StepStore {
    inner: Arc<Mutex<Inner>>,
    observer: Arc<Mutex<Option<Observer>>>,
}

impl StepStore {
    fn lock(&self) -> std::sync::MutexGuard<'_, Inner> {
        let inner = self.inner.lock().unwrap();
        assert!(!inner.in_observer, "观察者里不许碰存储（写线程上不许 I/O）");
        inner
    }

    /// 处理队头一笔（过提交时校验 → 追加日志 → 备好回执）；队空返回 `None`。返回票号。
    pub fn step(&self) -> Option<u64> {
        let (ticket, batch) = {
            let mut i = self.lock();
            let (ticket, req) = i.queue.pop_front()?;
            i.commits += 1;
            if req.events.is_empty() {
                i.empty_commits += 1;
            }
            let result = if std::mem::take(&mut i.fail_next) {
                Err(StorageError::Busy)
            } else {
                let first = Seq(i.log.len() as u64 + 1);
                let i = &mut *i;
                match apply_all(&mut i.projection, &req.events) {
                    Err(message) => Err(StorageError::Projection {
                        seq: first,
                        message,
                    }),
                    Ok(()) => {
                        for e in req.events {
                            let seq = Seq(i.log.len() as u64 + 1);
                            i.log.push(StoredEvent {
                                seq,
                                at: Timestamp(0),
                                actor: req.actor.clone(),
                                event: e,
                            });
                        }
                        Ok(CommitReceipt {
                            first_seq: first,
                            last_seq: Seq(i.log.len() as u64),
                            replayed: false,
                        })
                    }
                }
            };
            let batch = match &result {
                Ok(r) => i.log[(r.first_seq.0 as usize - 1)..].to_vec(),
                Err(_) => Vec::new(),
            };
            i.processed.insert(ticket, result);
            (ticket, batch)
        };
        if !batch.is_empty()
            && let Some(observer) = self.observer.lock().unwrap().as_mut()
        {
            self.inner.lock().unwrap().in_observer = true;
            observer(&batch);
            self.inner.lock().unwrap().in_observer = false;
        }
        Some(ticket)
    }

    /// 把这一笔的回执送到。
    pub fn deliver(&self, ticket: u64) -> bool {
        let mut i = self.lock();
        match i.processed.remove(&ticket) {
            Some(r) => {
                i.delivered.insert(ticket, r);
                true
            }
            None => false,
        }
    }

    /// 处理完、回执没送到的票号。
    pub fn undelivered(&self) -> Vec<u64> {
        self.lock().processed.keys().copied().collect()
    }

    /// 队列里每一笔的事件种类（看某个决定的提交是否已经入队）。
    pub fn queued_kinds(&self) -> Vec<Vec<&'static str>> {
        self.lock()
            .queue
            .iter()
            .map(|(_, r)| r.events.iter().map(DomainEvent::kind).collect())
            .collect()
    }

    pub fn queued(&self) -> usize {
        self.lock().queue.len()
    }

    /// 下一个到达的 `commit` 停在入队门口。
    pub fn gate_next_commit(&self) {
        self.lock().gate_armed = true;
    }

    /// 停在门口、还没开的门。
    pub fn closed_gates(&self) -> Vec<u64> {
        self.lock()
            .gates
            .iter()
            .filter(|(_, open)| !**open)
            .map(|(g, _)| *g)
            .collect()
    }

    pub fn open_gate(&self, gate: u64) {
        if let Some(open) = self.lock().gates.get_mut(&gate) {
            *open = true;
        }
    }

    pub fn fail_next(&self) {
        self.lock().fail_next = true;
    }

    /// 清掉还没触发的注入故障（收尾用）。
    pub fn clear_faults(&self) {
        let mut i = self.lock();
        i.fail_next = false;
        i.fail_read = false;
        i.gate_armed = false;
    }

    pub fn fail_read(&self) {
        self.lock().fail_read = true;
    }

    pub fn set_observer(&self, observer: Observer) {
        *self.observer.lock().unwrap() = Some(observer);
    }

    pub fn log(&self) -> Vec<StoredEvent> {
        self.lock().log.clone()
    }

    pub fn projection(&self) -> ReferenceProjection {
        self.lock().projection.clone()
    }

    pub fn commits(&self) -> u64 {
        self.lock().commits
    }

    /// 布景：不排队，直接处理；被拒时 panic。
    pub fn seed(&self, events: Vec<DomainEvent>) {
        assert!(self.lock().queue.is_empty(), "布景时队列里还有没处理的提交");
        let ticket = self.enqueue(CommitRequest {
            command_id: iwork_domain::id::CommandId::from_raw(
                iwork_domain::id::RawUlid::from_u128(1),
            ),
            actor: iwork_domain::common::Actor("test".into()),
            events,
            outbox: Vec::new(),
        });
        assert_eq!(self.step(), Some(ticket));
        let r = self.lock().processed.remove(&ticket).unwrap();
        r.expect("布景事件应当被接受");
    }

    fn enqueue(&self, req: CommitRequest) -> u64 {
        let mut i = self.lock();
        let ticket = i.next_ticket;
        i.next_ticket += 1;
        i.queue.push_back((ticket, req));
        ticket
    }
}

impl StoragePort for StepStore {
    async fn commit(&self, req: CommitRequest) -> Result<CommitReceipt, StorageError> {
        let gate = {
            let mut i = self.lock();
            if std::mem::take(&mut i.gate_armed) {
                let g = i.next_gate;
                i.next_gate += 1;
                i.gates.insert(g, false);
                Some(g)
            } else {
                None
            }
        };
        if let Some(g) = gate {
            // 停在入队门口：还没入队，这时被丢弃就什么都没发生
            poll_fn(|_| {
                if self.lock().gates[&g] {
                    Poll::Ready(())
                } else {
                    Poll::Pending
                }
            })
            .await;
        }
        let ticket = self.enqueue(req);
        poll_fn(|_| match self.lock().delivered.remove(&ticket) {
            Some(r) => Poll::Ready(r),
            None => Poll::Pending,
        })
        .await
    }

    async fn head_seq(&self) -> Result<Seq, StorageError> {
        Ok(Seq(self.lock().log.len() as u64))
    }

    async fn read_events(
        &self,
        filter: EventFilter,
        from: Seq,
        limit: usize,
    ) -> Result<Vec<StoredEvent>, StorageError> {
        let mut i = self.lock();
        if std::mem::take(&mut i.fail_read) {
            return Err(StorageError::Io {
                message: "测试注入的读失败".into(),
            });
        }
        Ok(i.log
            .iter()
            .filter(|e| e.seq > from && selected(&filter, &e.event))
            .take(limit)
            .cloned()
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

/// 测试里手写的单线程执行器：每个任务一个 future，用 noop waker 轮询，谁推进一步由调度表决定。
pub struct Tasks<T> {
    tasks: Vec<Option<std::pin::Pin<Box<dyn std::future::Future<Output = T> + Send>>>>,
    pub done: BTreeMap<usize, T>,
    pub dropped: BTreeSet<usize>,
}

impl<T> Default for Tasks<T> {
    fn default() -> Self {
        Self {
            tasks: Vec::new(),
            done: BTreeMap::new(),
            dropped: BTreeSet::new(),
        }
    }
}

impl<T> Tasks<T> {
    pub fn spawn(&mut self, f: impl std::future::Future<Output = T> + Send + 'static) -> usize {
        self.tasks.push(Some(Box::pin(f)));
        self.tasks.len() - 1
    }

    /// 还在跑的任务。
    pub fn running(&self) -> Vec<usize> {
        (0..self.tasks.len())
            .filter(|i| self.tasks[*i].is_some())
            .collect()
    }

    /// 推进任务一步；完成了就收结果。
    pub fn poll(&mut self, i: usize) {
        let Some(fut) = self.tasks.get_mut(i).and_then(Option::as_mut) else {
            return;
        };
        let mut cx = std::task::Context::from_waker(std::task::Waker::noop());
        if let Poll::Ready(out) = fut.as_mut().poll(&mut cx) {
            self.tasks[i] = None;
            self.done.insert(i, out);
        }
    }

    /// 丢弃一个还在跑的任务（它占着的令牌、等待登记随之释放）。
    pub fn drop_task(&mut self, i: usize) {
        if self.tasks.get_mut(i).and_then(Option::take).is_some() {
            self.dropped.insert(i);
        }
    }

    pub fn poll_all(&mut self) {
        for i in self.running() {
            self.poll(i);
        }
    }
}
