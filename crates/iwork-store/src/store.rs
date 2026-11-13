//! `SqliteStore`：打开流程、`StoragePort` 实现、维护与关闭（方案 2.2–2.4、2.8、2.9）。

use std::future::Future;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::Duration;

use iwork_domain::common::{Actor, Timestamp};
use iwork_domain::id::{CommandId, MessageId, Seq};
use iwork_ports::clock::Clock;
use iwork_ports::store::{
    CommitReceipt, CommitRequest, DeliveryState, EventCodec, EventFilter, OutboxId, OutboxItem,
    OutboxSelector, OutboxTarget, Projector, RebuildReport, RequeueReport, StorageError,
    StorageError as E, StorageError::Io, StoragePort, StoredEvent,
};
use rusqlite::{Connection, params, params_from_iter};

use crate::data_dir::DataDir;
use crate::migrate::{self, Migration, OpenHooks};
use crate::read_model::{ReadDefaults, SqliteReadModel};
use crate::reader::ReaderPool;
use crate::signals::{CommitObserver, Failed, FailureSignal};
use crate::sql::{ColumnText, enum_col, seq_col, seq_val, sql_err, u64_val};
use crate::writer::{self, WriteOp, WriteTx, WriterConfig, WriterHandle};

/// 存储的可调参数。默认值见 [`StoreConfig::default`]。
#[derive(Debug, Clone, Copy)]
pub struct StoreConfig {
    /// 组提交窗口：第一条写请求到达后再等这么久收齐一批（设计 5.2：2ms）
    pub group_commit_window: Duration,
    /// 一批最多几条
    pub max_batch: usize,
    /// 只读连接数
    pub readers: usize,
    /// 写队列容量；满了 `commit` 返回 `Busy`
    pub write_queue: usize,
    /// 读队列容量；满了查询返回 `Busy`
    pub read_queue: usize,
    /// 制品回收的宽限期（方案 2.10）
    pub gc_grace: Duration,
    /// macOS 的 `PRAGMA fullfsync`。`None` = 按平台默认（macOS 开，兑现「断电不丢已确认事件」，
    /// 方案 Q3 默认）；其他平台没有这个 PRAGMA，设了也不生效
    pub fullfsync: Option<bool>,
}

impl Default for StoreConfig {
    fn default() -> Self {
        Self {
            group_commit_window: Duration::from_millis(2),
            max_batch: 256,
            readers: 2,
            write_queue: 1024,
            read_queue: 256,
            gc_grace: Duration::from_secs(24 * 3600),
            fullfsync: None,
        }
    }
}

impl StoreConfig {
    /// 这次实际是否开 `fullfsync`。
    pub fn fullfsync_effective(&self) -> bool {
        cfg!(target_os = "macos") && self.fullfsync.unwrap_or(true)
    }
}

/// 测试用的打开选项：注入迁移登记表与文件操作钩子。
#[doc(hidden)]
#[derive(Debug, Clone, Copy)]
pub struct OpenOptions {
    pub migrations: &'static [Migration],
    pub hooks: OpenHooks,
}

impl Default for OpenOptions {
    fn default() -> Self {
        Self {
            migrations: migrate::MIGRATIONS,
            hooks: OpenHooks::default(),
        }
    }
}

/// SQLite 事件存储。可在线程间共享（`Send + Sync`）；克隆它用 `Arc`。
pub struct SqliteStore<C: EventCodec> {
    pub(crate) codec: Arc<C>,
    pub(crate) dir: DataDir,
    pub(crate) config: StoreConfig,
    pub(crate) writer: WriterHandle,
    pub(crate) readers: Arc<ReaderPool>,
    pub(crate) delivery_started: Arc<AtomicBool>,
    failure: Arc<FailureSignal>,
    join: Mutex<Option<JoinHandle<()>>>,
    #[cfg(any(test, feature = "test-hooks"))]
    pub(crate) hooks: Arc<crate::test_hooks::TestHooks>,
}

impl<C: EventCodec + 'static> SqliteStore<C> {
    /// 打开（必要时新建、迁移）数据库并起写线程与读线程池（方案 2.2）。
    ///
    /// 拒绝启动的情况：协议主版本不符（`ProtocolMajorMismatch`，已备份）；schema 比程序新、
    /// 不是 iWork 的库、迁移失败（都是 `Io`，消息里写明版本号与备份路径）。
    pub fn open<P, K>(
        dir: &DataDir,
        codec: Arc<C>,
        projector: Arc<P>,
        clock: Arc<K>,
        config: StoreConfig,
    ) -> Result<Self, StorageError>
    where
        P: Projector<Tx = WriteTx> + 'static,
        K: Clock + 'static,
    {
        Self::open_with(dir, codec, projector, clock, config, OpenOptions::default())
    }

    #[doc(hidden)]
    pub fn open_with<P, K>(
        dir: &DataDir,
        codec: Arc<C>,
        projector: Arc<P>,
        clock: Arc<K>,
        config: StoreConfig,
        options: OpenOptions,
    ) -> Result<Self, StorageError>
    where
        P: Projector<Tx = WriteTx> + 'static,
        K: Clock + 'static,
    {
        dir.ensure_layout().map_err(|e| Io {
            message: e.to_string(),
        })?;
        let program_major = codec.protocol_major();
        let probe = migrate::probe(
            dir,
            program_major,
            clock.now(),
            options.migrations,
            &options.hooks,
        )?;
        let conn = open_writer(dir, &config)?;
        migrate::migrate(
            &conn,
            dir,
            probe,
            program_major,
            clock.now(),
            options.migrations,
        )?;
        let failure = Arc::new(FailureSignal::default());
        let (writer, join) = writer::spawn(
            conn,
            codec.clone(),
            projector,
            clock,
            WriterConfig {
                window: config.group_commit_window,
                max_batch: config.max_batch.max(1),
                queue: config.write_queue.max(1),
            },
            failure.clone(),
        )?;
        let readers = ReaderPool::open(
            &dir.db_path(),
            config.readers,
            config.read_queue.max(1),
            &failure,
        )?;
        Ok(Self {
            codec,
            dir: dir.clone(),
            config,
            writer,
            readers: Arc::new(readers),
            delivery_started: Arc::new(AtomicBool::new(false)),
            failure,
            join: Mutex::new(Some(join)),
            #[cfg(any(test, feature = "test-hooks"))]
            hooks: Arc::default(),
        })
    }

    /// 读模型：与本存储共用读线程池与 codec。默认值只在读侧补（方案 2.6、2.7）。
    pub fn read_model(&self, defaults: ReadDefaults) -> SqliteReadModel<C> {
        SqliteReadModel::new(self.readers.clone(), self.codec.clone(), defaults)
    }

    /// T04 起 outbox 投递循环前调。之后 `rebuild_projections` 与制品的 `recover` 一律拒绝（方案 2.9）。
    pub fn mark_delivery_started(&self) {
        self.delivery_started.store(true, Ordering::SeqCst);
    }

    /// 装提交观察者（T04 方案 2.6）：经写队列送到写线程，返回时已生效，之后的每一批成功提交都会回调。
    /// 只能装一次，第二次返回 `Io`。
    pub fn set_commit_observer(
        &self,
        observer: Box<dyn CommitObserver>,
    ) -> impl Future<Output = Result<(), StorageError>> + Send + use<C> {
        self.writer
            .request(|reply| WriteOp::SetObserver(observer, reply))
    }

    /// 存储变得不可用时完成（写连接失效、写线程或读线程意外退出）；正常 `close` 不触发，future 永远不完成。
    /// 可以有多个等待者，失效之后再调立即完成。
    pub fn failed(&self) -> Failed {
        self.failure.wait()
    }

    #[cfg(test)]
    pub(crate) fn failure_waiting(&self) -> usize {
        self.failure.waiting()
    }

    /// `wal_checkpoint(TRUNCATE)`：把 WAL 合回主库并截短（T04 空闲时调，方案 S11）。
    pub fn maintenance(&self) -> impl Future<Output = Result<(), StorageError>> + Send + use<C> {
        self.writer.exclusive(|conn, _now| {
            let busy: i64 = conn
                .query_row("PRAGMA wal_checkpoint(TRUNCATE)", [], |r| r.get(0))
                .map_err(sql_err)?;
            if busy != 0 {
                return Err(E::Busy);
            }
            Ok(())
        })
    }

    /// 等写队列里已有的请求处理完、停写线程。之后的写请求得到 `Io`。T04 完全退出时调；
    /// 不调而直接 drop 也会做同样的事，只是拿不到错误。
    pub fn close(self) -> Result<(), StorageError> {
        self.shutdown()
    }

    fn shutdown(&self) -> Result<(), StorageError> {
        let join = self
            .join
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .take();
        let Some(join) = join else { return Ok(()) };
        // 写线程已经没了（panic）时发送失败，照样 join 取回结果
        let _ = self.writer.send_blocking(WriteOp::Shutdown);
        join.join().map_err(|_| Io {
            message: "写线程异常退出".into(),
        })
    }

    /// 写线程已提交的批数（组提交测试与基准用）。
    #[doc(hidden)]
    pub fn committed_batches(&self) -> u64 {
        self.writer
            .stats
            .commits
            .load(std::sync::atomic::Ordering::Relaxed)
    }

    /// 数据目录。
    pub fn data_dir(&self) -> &DataDir {
        &self.dir
    }

    #[cfg(test)]
    pub(crate) fn inject_fault(&self, fault: writer::Fault) {
        self.writer.send_blocking(WriteOp::Fault(fault)).unwrap();
    }
}

impl<C: EventCodec> Drop for SqliteStore<C> {
    fn drop(&mut self) {
        let join = self
            .join
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .take();
        if let Some(join) = join {
            let _ = self.writer.send_blocking(WriteOp::Shutdown);
            if join.join().is_err() {
                tracing::error!("写线程异常退出");
            }
        }
    }
}

/// 把 `target` 以 `prefixes` 里任一前缀开头、停在 `Dispatched` 的条目改成 `to`。前缀为空时什么都不改。
fn set_dispatched_by_prefix(
    conn: &Connection,
    now: Timestamp,
    prefixes: &[String],
    to: DeliveryState,
) -> Result<u64, StorageError> {
    if prefixes.is_empty() {
        return Ok(0);
    }
    let cond = vec![PREFIX_MATCH; prefixes.len()].join(" OR ");
    let sql = format!("UPDATE outbox SET state = ?, updated_at = ? WHERE state = ? AND ({cond})");
    let mut args: Vec<rusqlite::types::Value> = vec![
        to.to_text().to_owned().into(),
        now.0.into(),
        DeliveryState::Dispatched.to_text().to_owned().into(),
    ];
    args.extend(prefixes.iter().cloned().map(Into::into));
    let n = conn
        .execute(&sql, params_from_iter(args))
        .map_err(sql_err)?;
    Ok(n as u64)
}

/// 写连接与 PRAGMA（方案 2.3）。
fn open_writer(dir: &DataDir, config: &StoreConfig) -> Result<Connection, StorageError> {
    let conn = Connection::open(dir.db_path()).map_err(sql_err)?;
    let mode: String = conn
        .query_row("PRAGMA journal_mode = WAL", [], |r| r.get(0))
        .map_err(sql_err)?;
    if !mode.eq_ignore_ascii_case("wal") {
        return Err(Io {
            message: format!("数据库无法切到 WAL 模式（得到 {mode}）"),
        });
    }
    conn.execute_batch(
        "PRAGMA synchronous = FULL;
         PRAGMA foreign_keys = OFF;
         PRAGMA busy_timeout = 5000;
         PRAGMA temp_store = MEMORY;
         PRAGMA cache_size = -8000;
         PRAGMA journal_size_limit = 67108864;
         PRAGMA wal_autocheckpoint = 1000;",
    )
    .map_err(sql_err)?;
    if cfg!(target_os = "macos") {
        let on = if config.fullfsync_effective() {
            "ON"
        } else {
            "OFF"
        };
        conn.execute_batch(&format!("PRAGMA fullfsync = {on};"))
            .map_err(sql_err)?;
    }
    Ok(conn)
}

impl<C: EventCodec + 'static> StoragePort for SqliteStore<C> {
    fn commit(
        &self,
        req: CommitRequest,
    ) -> impl Future<Output = Result<CommitReceipt, StorageError>> + Send {
        self.writer.request(|reply| WriteOp::Commit(req, reply))
    }

    fn command_receipt(
        &self,
        command_id: CommandId,
    ) -> impl Future<Output = Result<Option<CommitReceipt>, StorageError>> + Send {
        // 走写线程（排在先入队的提交后面），不走读连接：方案 A2
        self.writer.sql_then(
            move |conn, _now| writer::recorded_receipt(conn, command_id),
            || {
                #[cfg(any(test, feature = "test-hooks"))]
                self.hooks.receipt_enqueued(command_id);
            },
        )
    }

    fn head_seq(&self) -> impl Future<Output = Result<Seq, StorageError>> + Send {
        self.readers.run(|conn| {
            let head: i64 = conn
                .query_row("SELECT COALESCE(MAX(seq), 0) FROM events", [], |r| r.get(0))
                .map_err(sql_err)?;
            seq_col("events", "seq", head)
        })
    }

    fn read_events(
        &self,
        filter: EventFilter,
        from: Seq,
        limit: usize,
    ) -> impl Future<Output = Result<Vec<StoredEvent>, StorageError>> + Send {
        let codec = self.codec.clone();
        self.readers
            .run(move |conn| read_events(conn, codec.as_ref(), &filter, from, limit))
    }

    fn checkpoint_message(
        &self,
        message_id: MessageId,
        offset: u64,
        text: &str,
    ) -> impl Future<Output = Result<(), StorageError>> + Send {
        let id = message_id.to_string();
        let text = text.to_owned();
        self.writer.sql(move |conn, now: Timestamp| {
            conn.execute(
                "INSERT INTO message_checkpoints(message_id, offset, text, updated_at) \
                 VALUES (?1, ?2, ?3, ?4) \
                 ON CONFLICT(message_id) DO UPDATE SET offset = excluded.offset, \
                 text = excluded.text, updated_at = excluded.updated_at",
                params![id, u64_val("offset", offset)?, text, now.0],
            )
            .map_err(sql_err)?;
            Ok(())
        })
    }

    fn outbox_pending(
        &self,
        limit: usize,
    ) -> impl Future<Output = Result<Vec<OutboxItem>, StorageError>> + Send {
        self.readers
            .run(move |conn| queued_outbox(conn, "", Vec::new(), limit))
    }

    fn outbox_pending_where(
        &self,
        selector: OutboxSelector,
        limit: usize,
    ) -> impl Future<Output = Result<Vec<OutboxItem>, StorageError>> + Send {
        self.readers.run(move |conn| {
            let (cond, args) = match &selector {
                OutboxSelector::Prefix(p) => (format!(" AND {}", PREFIX_MATCH), vec![p.clone()]),
                OutboxSelector::NoneOf(ps) if ps.is_empty() => (String::new(), Vec::new()),
                OutboxSelector::NoneOf(ps) => (
                    format!(" AND NOT ({})", vec![PREFIX_MATCH; ps.len()].join(" OR ")),
                    ps.clone(),
                ),
            };
            queued_outbox(conn, &cond, args, limit)
        })
    }

    fn outbox_requeue_dispatched(
        &self,
        redeliver_prefixes: Vec<String>,
        fail_prefixes: Vec<String>,
    ) -> impl Future<Output = Result<RequeueReport, StorageError>> + Send {
        self.writer.sql(move |conn, now: Timestamp| {
            // 一个 savepoint 里两条 UPDATE；两边都不沾的（不认识的去向）原样留在 dispatched
            let requeued =
                set_dispatched_by_prefix(conn, now, &redeliver_prefixes, DeliveryState::Queued)?;
            let failed =
                set_dispatched_by_prefix(conn, now, &fail_prefixes, DeliveryState::Failed)?;
            Ok(RequeueReport { requeued, failed })
        })
    }

    fn outbox_mark(
        &self,
        id: OutboxId,
        state: DeliveryState,
    ) -> impl Future<Output = Result<(), StorageError>> + Send {
        self.writer.sql(move |conn, now: Timestamp| {
            // 每次进入「已派发」算一次尝试（需求 12「投递与消费分开记录」）
            let bump = i64::from(state == DeliveryState::Dispatched);
            let changed = conn
                .execute(
                    "UPDATE outbox SET state = ?1, attempts = attempts + ?2, updated_at = ?3 \
                     WHERE id = ?4",
                    params![state.to_text(), bump, now.0, u64_val("outbox id", id.0)?],
                )
                .map_err(sql_err)?;
            if changed == 0 {
                return Err(E::NotFound {
                    entity: "outbox",
                    id: id.0.to_string(),
                });
            }
            Ok(())
        })
    }

    fn rebuild_projections(
        &self,
    ) -> impl Future<Output = Result<RebuildReport, StorageError>> + Send {
        if self.delivery_started.load(Ordering::SeqCst) {
            return crate::reply::Reply::ready(Err(Io {
                message: "投递已开始，重建只能在启动阶段执行".into(),
            }));
        }
        self.writer.request(WriteOp::Rebuild)
    }
}

/// `target` 以一个参数开头：参数第一次出现的位置是 1 当且仅当它是前缀（空前缀出现在 1，匹配一切）。
/// 不用 `LIKE`：前缀里的 `%`、`_` 不当通配符。语义与 [`OutboxSelector::matches`] 相同（`outbox_selector_agrees_with_sql` 对拍）。
const PREFIX_MATCH: &str = "instr(target, ?) = 1";

/// `queued` 的条目，加上额外条件 `cond`（以 ` AND` 开头，或为空）与它的参数，按 id 递增。
fn queued_outbox(
    conn: &Connection,
    cond: &str,
    cond_args: Vec<String>,
    limit: usize,
) -> Result<Vec<OutboxItem>, StorageError> {
    let sql = format!(
        "SELECT id, target, payload, state, attempts FROM outbox \
         WHERE state = ?{cond} ORDER BY id LIMIT ?"
    );
    let mut args: Vec<rusqlite::types::Value> =
        vec![DeliveryState::Queued.to_text().to_owned().into()];
    args.extend(cond_args.into_iter().map(Into::into));
    args.push(limit_val(limit).into());
    let mut stmt = conn.prepare_cached(&sql).map_err(sql_err)?;
    let rows = stmt
        .query_map(params_from_iter(args), |r| {
            Ok((
                r.get::<_, i64>(0)?,
                r.get::<_, String>(1)?,
                r.get::<_, Vec<u8>>(2)?,
                r.get::<_, String>(3)?,
                r.get::<_, i64>(4)?,
            ))
        })
        .map_err(sql_err)?;
    let mut out = Vec::new();
    for row in rows {
        let (id, target, payload, state, attempts) = row.map_err(sql_err)?;
        out.push(OutboxItem {
            id: OutboxId(id as u64),
            target: OutboxTarget(target),
            payload,
            state: enum_col("outbox", "state", &state)?,
            attempts: u32::try_from(attempts).unwrap_or(u32::MAX),
        });
    }
    Ok(out)
}

fn limit_val(limit: usize) -> i64 {
    i64::try_from(limit).unwrap_or(i64::MAX)
}

fn read_events<C: EventCodec>(
    conn: &Connection,
    codec: &C,
    filter: &EventFilter,
    from: Seq,
    limit: usize,
) -> Result<Vec<StoredEvent>, StorageError> {
    let mut sql = String::from("SELECT seq, type, payload, actor, at FROM events WHERE seq > ?");
    let mut args: Vec<rusqlite::types::Value> = vec![seq_val("read_events 的 from", from)?.into()];
    if let EventFilter::Streams(selectors) = filter {
        if selectors.is_empty() {
            return Ok(Vec::new());
        }
        let mut ors = Vec::new();
        for s in selectors {
            args.push(s.kind.to_text().to_owned().into());
            match &s.id {
                Some(id) => {
                    ors.push("(stream_kind = ? AND stream_id = ?)");
                    args.push(id.0.clone().into());
                }
                None => ors.push("(stream_kind = ?)"),
            }
        }
        sql.push_str(&format!(" AND ({})", ors.join(" OR ")));
    }
    sql.push_str(" ORDER BY seq LIMIT ?");
    args.push(limit_val(limit).into());

    // 同一个过滤器（引擎每次追平都用同一组流）拼出的 SQL 相同，按文本缓存，省掉每次请求的准备开销
    let mut stmt = conn.prepare_cached(&sql).map_err(sql_err)?;
    let rows = stmt
        .query_map(params_from_iter(args), |r| {
            Ok((
                r.get::<_, i64>(0)?,
                r.get::<_, String>(1)?,
                r.get::<_, Vec<u8>>(2)?,
                r.get::<_, String>(3)?,
                r.get::<_, i64>(4)?,
            ))
        })
        .map_err(sql_err)?;
    let mut out = Vec::new();
    for row in rows {
        let (seq, kind, payload, actor, at) = row.map_err(sql_err)?;
        out.push(StoredEvent {
            seq: seq_col("events", "seq", seq)?,
            at: Timestamp(at),
            actor: Actor(actor),
            event: codec.decode(&kind, &payload)?,
        });
    }
    Ok(out)
}
