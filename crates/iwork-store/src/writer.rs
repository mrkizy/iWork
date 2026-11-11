//! 单写者线程与组提交（技术设计 5.2，方案 2.4）。
//!
//! 一个 std 线程独占写连接。第一条写请求到达后再等 `group_commit_window` 收齐（最多
//! `max_batch` 条），一个 `BEGIN IMMEDIATE … COMMIT` 里每条一个 `SAVEPOINT`，一次 fsync 摊给整批；
//! 回复在 `COMMIT` 返回之后才发。`seq` 由这里显式分配（`next_seq` 是下一个可分配的号），
//! 失败的命令退回它占的号，整批失败后从持久的 `MAX(seq)` 重新确定，保证连续无洞。

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{Receiver, RecvTimeoutError, SyncSender, TrySendError};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use iwork_domain::common::{Actor, Timestamp};
use iwork_domain::id::{CommandId, Seq};
use iwork_ports::clock::Clock;
use iwork_ports::store::{
    CommitReceipt, CommitRequest, EventCodec, Projector, RebuildReport, StorageError, StoredEvent,
};
use rusqlite::{Connection, OptionalExtension, params};

use crate::reply::{self, Reply, Responder};
use crate::signals::{CommitObserver, CommittedBatch, ExitGuard, FailureSignal, StoreFailure};
use crate::sql::{ColumnText, id_col, seq_col, seq_val, sql_err};
use crate::tables::PROJECTION_TABLES;

const WRITER_GONE: &str = "写线程已退出";
const WRITER_BROKEN: &str = "写连接已失效（回滚失败），需要重启后台服务";

/// 当前事件的信封：投影器从这里取时间与主体（`Projector::project` 没有这两个参数）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Envelope {
    pub at: Timestamp,
    pub actor: Actor,
    pub command_id: CommandId,
}

/// 投影器的事务句柄：写线程独占的连接 + 当前事件的信封。不实现 `Clone`，连接不出本 crate。
pub struct WriteTx {
    pub(crate) conn: Connection,
    pub(crate) envelope: Envelope,
}

impl WriteTx {
    /// 当前事件的信封。
    pub fn envelope(&self) -> &Envelope {
        &self.envelope
    }
}

/// 批内的一条普通写操作（检查点、outbox 标记、制品登记与 pin）。在本条的 savepoint 里执行，
/// 结果先留在自己身上，整批 `COMMIT` 之后再经 `finish` 送出。
pub(crate) trait SqlOp: Send {
    fn apply(&mut self, conn: &Connection, now: Timestamp) -> Result<(), StorageError>;
    fn finish(self: Box<Self>, outcome: Result<(), StorageError>);
}

struct FnOp<T, F> {
    f: F,
    value: Option<T>,
    reply: Responder<T>,
}

impl<T, F> SqlOp for FnOp<T, F>
where
    T: Send,
    F: FnMut(&Connection, Timestamp) -> Result<T, StorageError> + Send,
{
    fn apply(&mut self, conn: &Connection, now: Timestamp) -> Result<(), StorageError> {
        self.value = Some((self.f)(conn, now)?);
        Ok(())
    }

    fn finish(self: Box<Self>, outcome: Result<(), StorageError>) {
        let this = *self;
        this.reply.send(outcome.and_then(|()| {
            this.value.ok_or(StorageError::Io {
                message: "写操作没有产出结果".into(),
            })
        }));
    }
}

/// 独占一批、自己管事务的操作（GC、启动恢复、checkpoint）。闭包执行完不直接回复，而是交回一个
/// 「送出结果」的函数：写线程先确认事务已结束（没结束就回滚，回滚不了就标失效），再把收尾的结论
/// 一并送出。这样闭包遗留的事务既不会污染下一批，也不会让调用方拿到一个其实已被回滚的 `Ok`。
pub(crate) type ExclusiveFn = Box<dyn FnOnce(&Connection, Timestamp) -> Deliver + Send>;

/// 送出独占操作的结果；参数是写线程收尾时发现的问题（没有就是 `None`）。
pub(crate) type Deliver = Box<dyn FnOnce(Option<StorageError>) + Send>;

pub(crate) enum WriteOp {
    Commit(CommitRequest, Responder<CommitReceipt>),
    Sql(Box<dyn SqlOp>),
    Rebuild(Responder<RebuildReport>),
    Exclusive(ExclusiveFn),
    /// 装提交观察者；只能装一次
    SetObserver(Box<dyn CommitObserver>, Responder<()>),
    Shutdown,
    #[cfg(test)]
    Fault(Fault),
}

impl WriteOp {
    fn is_exclusive(&self) -> bool {
        match self {
            Self::Commit(..) | Self::Sql(_) => false,
            Self::Rebuild(_) | Self::Exclusive(_) | Self::SetObserver(..) | Self::Shutdown => true,
            #[cfg(test)]
            Self::Fault(_) => true,
        }
    }
}

/// 测试注入的故障。
#[cfg(test)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Fault {
    /// 下一次 COMMIT 失败；`tx_still_open` 为真时事务保留（如 SQLITE_BUSY），否则模拟 SQLite 已自动回滚
    CommitFails { tx_still_open: bool },
    /// 下一次整批回滚的 ROLLBACK 失败
    RollbackFails,
    /// 跳过 `skip` 次事件插入后，下一次插入时用 progress_handler 中断语句
    /// （SQLITE_INTERRUPT：显式事务里的 DML 被中断，SQLite 自动回滚整个事务）
    InterruptEventInsert { skip: u32 },
    /// 从此刻起本连接上的 COMMIT 与 ROLLBACK 一律被 authorizer 拒绝（BEGIN、SAVEPOINT 照常）
    DenyTxEnd,
    /// 写线程立刻 panic（模拟写线程意外退出）
    Panic,
}

/// 写线程与外界共享的计数（测试与基准读）。
#[derive(Debug, Default)]
pub(crate) struct WriterStats {
    pub(crate) commits: AtomicU64,
}

/// 写线程的参数。
#[derive(Debug, Clone, Copy)]
pub(crate) struct WriterConfig {
    pub(crate) window: Duration,
    pub(crate) max_batch: usize,
    pub(crate) queue: usize,
}

/// 给别的模块（store、制品存储）用的发送端。可复制；写线程只有一个。
#[derive(Clone)]
pub(crate) struct WriterHandle {
    tx: SyncSender<WriteOp>,
    pub(crate) stats: Arc<WriterStats>,
}

impl WriterHandle {
    /// 入队；队列满返回 `Busy`（可重试），写线程没了返回 `Io`。
    pub(crate) fn submit(&self, op: WriteOp) -> Result<(), StorageError> {
        match self.tx.try_send(op) {
            Ok(()) => Ok(()),
            Err(TrySendError::Full(_)) => Err(StorageError::Busy),
            Err(TrySendError::Disconnected(_)) => Err(StorageError::Io {
                message: WRITER_GONE.into(),
            }),
        }
    }

    /// 入队一条要等结果的操作；入队失败时直接返回已带错误的应答。
    pub(crate) fn request<T>(&self, build: impl FnOnce(Responder<T>) -> WriteOp) -> Reply<T> {
        let (tx, rx) = reply::channel(WRITER_GONE);
        match self.submit(build(tx)) {
            Ok(()) => rx,
            Err(e) => Reply::ready(Err(e)),
        }
    }

    /// 批内普通写操作：`f` 在本条的 savepoint 里执行。
    pub(crate) fn sql<T, F>(&self, f: F) -> Reply<T>
    where
        T: Send + 'static,
        F: FnMut(&Connection, Timestamp) -> Result<T, StorageError> + Send + 'static,
    {
        self.request(|reply| {
            WriteOp::Sql(Box::new(FnOp {
                f,
                value: None,
                reply,
            }))
        })
    }

    /// 独占操作：`f` 自己开事务、自己收尾，结果经它捕获的发送端送出。
    pub(crate) fn exclusive<T, F>(&self, f: F) -> Reply<T>
    where
        T: Send + 'static,
        F: FnOnce(&Connection, Timestamp) -> Result<T, StorageError> + Send + 'static,
    {
        self.request(|reply: Responder<T>| {
            WriteOp::Exclusive(Box::new(move |conn, now| {
                let result = f(conn, now);
                Box::new(move |cleanup: Option<StorageError>| {
                    reply.send(match (cleanup, result) {
                        (None, r) => r,
                        (Some(e), Ok(_)) => Err(e),
                        (Some(e), Err(orig)) => Err(StorageError::Io {
                            message: format!("{orig}；{e}"),
                        }),
                    })
                })
            }))
        })
    }

    /// 阻塞入队（关闭时用，不能因为队列满就丢掉关闭请求）。
    pub(crate) fn send_blocking(&self, op: WriteOp) -> Result<(), StorageError> {
        self.tx.send(op).map_err(|_| StorageError::Io {
            message: WRITER_GONE.into(),
        })
    }
}

/// 起写线程。`conn` 已设好 PRAGMA、跑完迁移。
pub(crate) fn spawn<C, P, K>(
    conn: Connection,
    codec: Arc<C>,
    projector: Arc<P>,
    clock: Arc<K>,
    config: WriterConfig,
    failure: Arc<FailureSignal>,
) -> Result<(WriterHandle, JoinHandle<()>), StorageError>
where
    C: EventCodec + 'static,
    P: Projector<Tx = WriteTx> + 'static,
    K: Clock + 'static,
{
    let next_seq = durable_next_seq(&conn)?;
    let (tx, rx) = std::sync::mpsc::sync_channel(config.queue);
    let stats = Arc::new(WriterStats::default());
    let mut writer = Writer {
        tx: WriteTx {
            conn,
            envelope: Envelope {
                at: Timestamp(0),
                actor: Actor(String::new()),
                command_id: CommandId::from_raw(iwork_domain::id::RawUlid::from_u128(0)),
            },
        },
        codec,
        projector,
        clock,
        config,
        next_seq,
        broken: false,
        observer: None,
        failure: failure.clone(),
        stats: stats.clone(),
        #[cfg(test)]
        faults: Vec::new(),
    };
    let join = std::thread::Builder::new()
        .name("iwork-store-writer".into())
        .spawn(move || {
            // panic 离开时守卫发 WriterGone；收到关闭请求或所有发送端都没了是正常退出
            let guard = ExitGuard::new(failure, StoreFailure::WriterGone);
            writer.run(rx);
            guard.disarm();
        })
        .map_err(|e| StorageError::Io {
            message: format!("起写线程失败：{e}"),
        })?;
    Ok((WriterHandle { tx, stats }, join))
}

/// 下一个可分配的号：`COALESCE(MAX(seq), 0) + 1`。只在事务外（或回滚确认之后）读。
fn durable_next_seq(conn: &Connection) -> Result<u64, StorageError> {
    let next: i64 = conn
        .query_row("SELECT COALESCE(MAX(seq), 0) + 1 FROM events", [], |r| {
            r.get(0)
        })
        .map_err(sql_err)?;
    Ok(seq_col("events", "seq", next)?.0)
}

struct Writer<C, P, K> {
    tx: WriteTx,
    codec: Arc<C>,
    projector: Arc<P>,
    clock: Arc<K>,
    config: WriterConfig,
    next_seq: u64,
    /// 回滚失败后连接不可信：之后的写请求一律立即失败，由 T04 整体重启（设计 10.4）。只经 `mark_broken` 置位
    broken: bool,
    observer: Option<Box<dyn CommitObserver>>,
    failure: Arc<FailureSignal>,
    stats: Arc<WriterStats>,
    #[cfg(test)]
    faults: Vec<Fault>,
}

/// 批内一条：普通写操作或命令，连同它暂存的结果。
enum BatchItem {
    Commit {
        req: CommitRequest,
        reply: Responder<CommitReceipt>,
        result: Option<Result<CommitReceipt, StorageError>>,
    },
    Sql {
        op: Box<dyn SqlOp>,
        result: Option<Result<(), StorageError>>,
    },
}

impl BatchItem {
    fn finish(self, override_err: Option<&StorageError>) {
        match self {
            Self::Commit { reply, result, .. } => reply.send(match override_err {
                Some(e) => Err(e.clone()),
                None => result.unwrap_or_else(|| Err(not_applied())),
            }),
            Self::Sql { op, result } => op.finish(match override_err {
                Some(e) => Err(e.clone()),
                None => result.unwrap_or_else(|| Err(not_applied())),
            }),
        }
    }
}

fn not_applied() -> StorageError {
    StorageError::Io {
        message: "写操作没有执行".into(),
    }
}

impl<C, P, K> Writer<C, P, K>
where
    C: EventCodec,
    P: Projector<Tx = WriteTx>,
    K: Clock,
{
    fn run(&mut self, rx: Receiver<WriteOp>) {
        loop {
            let Ok(first) = rx.recv() else { return };
            if first.is_exclusive() {
                if !self.run_exclusive(first) {
                    return;
                }
                continue;
            }
            let mut batch = vec![first];
            let mut held_back = None;
            let deadline = Instant::now() + self.config.window;
            while batch.len() < self.config.max_batch {
                let wait = deadline.saturating_duration_since(Instant::now());
                match rx.recv_timeout(wait) {
                    Ok(op) if op.is_exclusive() => {
                        held_back = Some(op);
                        break;
                    }
                    Ok(op) => batch.push(op),
                    Err(RecvTimeoutError::Timeout | RecvTimeoutError::Disconnected) => break,
                }
            }
            self.run_batch(batch);
            if let Some(op) = held_back
                && !self.run_exclusive(op)
            {
                return;
            }
        }
    }

    /// 处理一条独占操作；返回 `false` 表示该退出了。
    fn run_exclusive(&mut self, op: WriteOp) -> bool {
        match op {
            WriteOp::Shutdown => return false,
            WriteOp::Rebuild(reply) => {
                let r = if self.broken {
                    Err(broken())
                } else {
                    self.rebuild()
                };
                reply.send(r);
            }
            WriteOp::SetObserver(observer, reply) => {
                reply.send(if self.observer.is_some() {
                    Err(StorageError::Io {
                        message: "提交观察者只能装一次".into(),
                    })
                } else {
                    self.observer = Some(observer);
                    Ok(())
                });
            }
            WriteOp::Exclusive(f) => {
                if self.broken {
                    // f 捕获的发送端随 f 一起 drop，调用方得到「写线程已退出」类错误
                    drop(f);
                } else {
                    let deliver = f(&self.tx.conn, self.clock.now());
                    let cleanup = match self.end_leftover_tx("独占操作") {
                        Ok(false) => None,
                        Ok(true) => Some(StorageError::Io {
                            message: "独占操作返回时事务仍未结束，已回滚，本次操作未生效".into(),
                        }),
                        Err(e) => Some(e),
                    };
                    deliver(cleanup);
                }
            }
            #[cfg(test)]
            WriteOp::Fault(Fault::DenyTxEnd) => {
                use rusqlite::hooks::{AuthAction, Authorization, TransactionOperation as Op};
                self.tx
                    .conn
                    .authorizer(Some(|ctx: rusqlite::hooks::AuthContext<'_>| {
                        match ctx.action {
                            AuthAction::Transaction {
                                // rusqlite 把 COMMIT 归为 Unknown
                                operation: Op::Unknown | Op::Rollback,
                            } => Authorization::Deny,
                            _ => Authorization::Allow,
                        }
                    }))
                    .expect("装 authorizer");
            }
            #[cfg(test)]
            WriteOp::Fault(Fault::Panic) => panic!("注入的写线程 panic"),
            #[cfg(test)]
            WriteOp::Fault(f) => self.faults.push(f),
            WriteOp::Commit(..) | WriteOp::Sql(_) => unreachable!("不是独占操作"),
        }
        true
    }

    fn run_batch(&mut self, ops: Vec<WriteOp>) {
        let mut items: Vec<BatchItem> = ops
            .into_iter()
            .map(|op| match op {
                WriteOp::Commit(req, reply) => BatchItem::Commit {
                    req,
                    reply,
                    result: None,
                },
                WriteOp::Sql(op) => BatchItem::Sql { op, result: None },
                _ => unreachable!("独占操作不进批"),
            })
            .collect();
        if self.broken {
            let e = broken();
            items.into_iter().for_each(|i| i.finish(Some(&e)));
            return;
        }
        if let Err(e) = self.exec("BEGIN IMMEDIATE") {
            // 连接若意外还在事务里，回滚掉或标失效，不让下一批接着撞同一个错误
            let e = self.end_leftover_tx("开批").err().unwrap_or(e);
            items.into_iter().for_each(|i| i.finish(Some(&e)));
            return;
        }

        let now = self.clock.now();
        let mut lost: Option<StorageError> = None;
        for item in &mut items {
            if let Err(e) = self.apply_in_savepoint(item, now) {
                lost = Some(e);
                break;
            }
        }
        if lost.is_none()
            && let Err(e) = self.commit_tx()
        {
            lost = Some(e);
        }
        match lost {
            None => {
                self.stats.commits.fetch_add(1, Ordering::Relaxed);
                // 先交给观察者、再送回执：调用方拿到回执时，它的事件一定已经交出去了
                self.notify_observer(&mut items, now);
                items.into_iter().for_each(|i| i.finish(None));
            }
            Some(cause) => {
                let err = self.abort_batch(&cause);
                items.into_iter().for_each(|i| i.finish(Some(&err)));
            }
        }
    }

    /// 把这批里真正写进去的事件按 seq 顺序交给观察者。没装观察者时什么都不做（也不搬事件）。
    fn notify_observer(&mut self, items: &mut [BatchItem], now: Timestamp) {
        let Some(observer) = self.observer.as_mut() else {
            return;
        };
        let mut events = Vec::new();
        let mut outbox_added = 0;
        for item in items.iter_mut() {
            let BatchItem::Commit {
                req,
                result: Some(Ok(receipt)),
                ..
            } = item
            else {
                continue;
            };
            if receipt.replayed {
                continue;
            }
            outbox_added += req.outbox.len();
            // 回执已经定了，请求里的事件不再用：直接搬走，不多拷贝
            let first = receipt.first_seq.0;
            for (i, event) in std::mem::take(&mut req.events).into_iter().enumerate() {
                events.push(StoredEvent {
                    seq: Seq(first + i as u64),
                    at: now,
                    actor: req.actor.clone(),
                    event,
                });
            }
        }
        if !events.is_empty() || outbox_added > 0 {
            observer.committed(CommittedBatch {
                events: &events,
                outbox_added,
            });
        }
    }

    /// 回滚失败后：连接不可信，之后的写请求一律立即失败，并发失效信号。
    fn mark_broken(&mut self) {
        self.broken = true;
        self.failure.fire(StoreFailure::WriterBroken);
    }

    /// 在本条的 savepoint 里执行。条目自己的失败记在条目上（返回 `Ok`）；返回 `Err` 表示事务已经
    /// 没了或 savepoint 操作失败，整批要作废。
    fn apply_in_savepoint(
        &mut self,
        item: &mut BatchItem,
        now: Timestamp,
    ) -> Result<(), StorageError> {
        self.exec("SAVEPOINT op")?;
        let seq_before = self.next_seq;
        let outcome = match item {
            BatchItem::Commit { req, result, .. } => {
                let r = self.apply_commit(req, now);
                let ok = r.is_ok();
                *result = Some(r.clone());
                if ok { Ok(()) } else { r.map(|_| ()) }
            }
            BatchItem::Sql { op, result } => {
                let r = op.apply(&self.tx.conn, now);
                *result = Some(r.clone());
                r
            }
        };
        match outcome {
            Ok(()) => self.exec("RELEASE op"),
            Err(e) => {
                self.next_seq = seq_before;
                if self.tx.conn.is_autocommit() {
                    // SQLite 已把整个事务自动回滚：前面 RELEASE 过的条目也没了
                    return Err(e);
                }
                self.exec("ROLLBACK TO op")?;
                self.exec("RELEASE op")
            }
        }
    }

    fn apply_commit(
        &mut self,
        req: &CommitRequest,
        now: Timestamp,
    ) -> Result<CommitReceipt, StorageError> {
        let command_id = req.command_id.to_string();
        let previous: Option<(i64, i64)> = self
            .tx
            .conn
            .query_row(
                "SELECT first_seq, last_seq FROM commands WHERE command_id = ?1",
                [&command_id],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .optional()
            .map_err(sql_err)?;
        if let Some((first, last)) = previous {
            return Ok(CommitReceipt {
                first_seq: seq_col("commands", "first_seq", first)?,
                last_seq: seq_col("commands", "last_seq", last)?,
                replayed: true,
            });
        }

        let head = self.next_seq - 1;
        self.tx.envelope = Envelope {
            at: now,
            actor: req.actor.clone(),
            command_id: req.command_id,
        };
        for event in &req.events {
            let seq = Seq(self.next_seq);
            let encoded = self.codec.encode(event)?;
            let (stream_kind, stream_id) = event.stream();
            #[cfg(test)]
            let armed = self.arm_interrupt_fault();
            let inserted = self.tx.conn.execute(
                "INSERT INTO events(seq, stream_kind, stream_id, type, payload, command_id, actor, at) \
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
                params![
                    seq_val("事件 seq", seq)?,
                    stream_kind.to_text(),
                    stream_id.map(|s| s.0),
                    encoded.kind,
                    encoded.payload,
                    command_id,
                    req.actor.0,
                    now.0
                ],
            );
            #[cfg(test)]
            if armed {
                self.disarm_interrupt_fault();
            }
            inserted.map_err(sql_err)?;
            self.projector.project(&mut self.tx, seq, event)?;
            self.next_seq += 1;
        }
        for item in &req.outbox {
            self.tx
                .conn
                .execute(
                    "INSERT INTO outbox(command_id, target, payload, state, attempts, created_at, updated_at) \
                     VALUES (?1, ?2, ?3, 'queued', 0, ?4, ?4)",
                    params![command_id, item.target.0, item.payload, now.0],
                )
                .map_err(sql_err)?;
        }
        let (first, last) = if req.events.is_empty() {
            (head, head)
        } else {
            (head + 1, self.next_seq - 1)
        };
        self.tx
            .conn
            .execute(
                "INSERT INTO commands(command_id, first_seq, last_seq, at) VALUES (?1, ?2, ?3, ?4)",
                params![
                    command_id,
                    seq_val("first_seq", Seq(first))?,
                    seq_val("last_seq", Seq(last))?,
                    now.0
                ],
            )
            .map_err(sql_err)?;
        Ok(CommitReceipt {
            first_seq: Seq(first),
            last_seq: Seq(last),
            replayed: false,
        })
    }

    fn commit_tx(&mut self) -> Result<(), StorageError> {
        #[cfg(test)]
        if let Some(i) = self
            .faults
            .iter()
            .position(|f| matches!(f, Fault::CommitFails { .. }))
        {
            let Fault::CommitFails { tx_still_open } = self.faults.remove(i) else {
                unreachable!()
            };
            if !tx_still_open {
                self.exec("ROLLBACK")?;
            }
            return Err(StorageError::Io {
                message: "注入的 COMMIT 失败".into(),
            });
        }
        self.exec("COMMIT")
    }

    /// 整批失败收尾（方案 2.4）：确认回滚 → 才重读持久 head。回滚本身失败则连接不可信。
    fn abort_batch(&mut self, cause: &StorageError) -> StorageError {
        if !self.tx.conn.is_autocommit()
            && let Err(e) = self.rollback()
        {
            tracing::error!(error = %e, "整批回滚失败，写连接标为失效");
            self.mark_broken();
            return broken();
        }
        match durable_next_seq(&self.tx.conn) {
            Ok(n) => self.next_seq = n,
            Err(e) => {
                tracing::error!(error = %e, "回滚后读不出持久 head，写连接标为失效");
                self.mark_broken();
                return broken();
            }
        }
        tracing::warn!(error = %cause, "一批写操作整体回滚");
        StorageError::Io {
            message: format!("本批写入整体回滚，可重试：{cause}"),
        }
    }

    fn rollback(&mut self) -> Result<(), StorageError> {
        #[cfg(test)]
        if let Some(i) = self.faults.iter().position(|f| *f == Fault::RollbackFails) {
            self.faults.remove(i);
            return Err(StorageError::Io {
                message: "注入的 ROLLBACK 失败".into(),
            });
        }
        self.exec("ROLLBACK")
    }

    /// 重建投影（方案 2.9）：一个事务里清登记表的投影表，按 seq 重放全部事件。
    fn rebuild(&mut self) -> Result<RebuildReport, StorageError> {
        self.exec("BEGIN IMMEDIATE")?;
        let result = self
            .replay_all()
            .and_then(|report| self.exec("COMMIT").map(|()| report));
        match result {
            Ok(report) => Ok(report),
            Err(e) => match self.end_leftover_tx("重建") {
                Ok(_) => Err(e),
                Err(broken) => Err(StorageError::Io {
                    message: format!("{e}；{broken}"),
                }),
            },
        }
    }

    /// 确保写连接不在事务里：在就回滚。返回 `Ok(true)` 表示确实有遗留事务、已回滚；
    /// 回滚失败时写连接标为失效（之后的写请求一律立即失败，由 T04 整体重启），返回失效错误。
    fn end_leftover_tx(&mut self, what: &str) -> Result<bool, StorageError> {
        if self.tx.conn.is_autocommit() {
            return Ok(false);
        }
        match self.rollback() {
            Ok(()) => {
                tracing::warn!(what, "写连接上有遗留事务，已回滚");
                Ok(true)
            }
            Err(e) => {
                tracing::error!(what, error = %e, "遗留事务回滚失败，写连接标为失效");
                self.mark_broken();
                Err(broken())
            }
        }
    }

    fn replay_all(&mut self) -> Result<RebuildReport, StorageError> {
        const CHUNK: i64 = 1000;
        for table in PROJECTION_TABLES {
            self.exec(&format!("DELETE FROM {}", table.name))?;
        }
        let mut count = 0u64;
        let mut last = 0i64;
        loop {
            // 分块读进内存：projector 要拿 &mut WriteTx，不能同时借着连接上的游标
            let rows: Vec<(i64, String, Vec<u8>, String, i64, String)> = {
                let mut stmt = self
                    .tx
                    .conn
                    .prepare_cached(
                        "SELECT seq, type, payload, actor, at, command_id FROM events \
                         WHERE seq > ?1 ORDER BY seq LIMIT ?2",
                    )
                    .map_err(sql_err)?;
                stmt.query_map(params![last, CHUNK], |r| {
                    Ok((
                        r.get(0)?,
                        r.get(1)?,
                        r.get(2)?,
                        r.get(3)?,
                        r.get(4)?,
                        r.get(5)?,
                    ))
                })
                .map_err(sql_err)?
                .collect::<Result<_, _>>()
                .map_err(sql_err)?
            };
            if rows.is_empty() {
                break;
            }
            for (seq, kind, payload, actor, at, command_id) in rows {
                let event = self.codec.decode(&kind, &payload).inspect_err(|e| {
                    tracing::error!(seq, kind = %kind, error = %e, "重建时事件解码失败");
                })?;
                self.tx.envelope = Envelope {
                    at: Timestamp(at),
                    actor: Actor(actor),
                    command_id: id_col("events", "command_id", &command_id)?,
                };
                self.projector
                    .project(&mut self.tx, seq_col("events", "seq", seq)?, &event)?;
                count += 1;
                last = seq;
            }
        }
        Ok(RebuildReport {
            events: count,
            last_seq: seq_col("events", "seq", last)?,
        })
    }

    fn exec(&self, sql: &str) -> Result<(), StorageError> {
        self.tx.conn.execute_batch(sql).map_err(sql_err)
    }

    #[cfg(test)]
    fn arm_interrupt_fault(&mut self) -> bool {
        let Some(i) = self
            .faults
            .iter()
            .position(|f| matches!(f, Fault::InterruptEventInsert { .. }))
        else {
            return false;
        };
        let Fault::InterruptEventInsert { skip } = self.faults[i] else {
            unreachable!()
        };
        if skip > 0 {
            self.faults[i] = Fault::InterruptEventInsert { skip: skip - 1 };
            return false;
        }
        self.faults.remove(i);
        self.tx
            .conn
            .progress_handler(1, Some(|| true))
            .expect("装 progress_handler");
        true
    }

    #[cfg(test)]
    fn disarm_interrupt_fault(&mut self) {
        self.tx
            .conn
            .progress_handler(0, None::<fn() -> bool>)
            .expect("卸 progress_handler");
    }
}

fn broken() -> StorageError {
    StorageError::Io {
        message: WRITER_BROKEN.into(),
    }
}
