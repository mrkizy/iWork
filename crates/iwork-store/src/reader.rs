//! 只读连接池（方案 2.4）：N 个线程各持一条只读连接，共享一个有界任务队列。
//!
//! 一次端口调用（含 `ReadModelPort` 里由几条 SELECT 拼成的复合查询）在**一个**显式读事务里做完，
//! 返回值对应同一个提交时刻；出错也保证结束事务，结束不了就换一条新连接，不让一个卡在事务里的连接
//! 长期挡住 WAL checkpoint（方案 2.4，第 1 轮代码审查 1/F-1）。
//! 队列满返回 `Busy`（可重试），不无限堆积（方案 S9）。

use std::path::Path;
use std::sync::mpsc::{Receiver, SyncSender, TrySendError};
use std::sync::{Arc, Mutex};

use iwork_ports::store::StorageError;
use rusqlite::{Connection, OpenFlags};

use crate::reply::{self, Reply};
use crate::sql::sql_err;

const READER_GONE: &str = "读线程已退出";

type Job = Box<dyn FnOnce(&Connection) + Send>;

/// 只读连接池。drop 时队列断开，线程各自退出。
pub(crate) struct ReaderPool {
    tx: SyncSender<Job>,
}

impl ReaderPool {
    pub(crate) fn open(db: &Path, readers: usize, queue: usize) -> Result<Self, StorageError> {
        let (tx, rx) = std::sync::mpsc::sync_channel::<Job>(queue);
        let rx = Arc::new(Mutex::new(rx));
        for i in 0..readers.max(1) {
            let conn = open_reader(db)?;
            let rx = rx.clone();
            let db = db.to_path_buf();
            std::thread::Builder::new()
                .name(format!("iwork-store-reader-{i}"))
                .spawn(move || serve(conn, &db, &rx))
                .map_err(|e| StorageError::Io {
                    message: format!("起读线程失败：{e}"),
                })?;
        }
        Ok(Self { tx })
    }

    /// 在某条只读连接上跑 `f`。队列满时立即返回 `Busy`。
    pub(crate) fn run<T, F>(&self, f: F) -> Reply<T>
    where
        T: Send + 'static,
        F: FnOnce(&Connection) -> Result<T, StorageError> + Send + 'static,
    {
        let (reply_tx, reply_rx) = reply::channel(READER_GONE);
        let job: Job = Box::new(move |conn| reply_tx.send(in_read_tx(conn, f)));
        match self.tx.try_send(job) {
            Ok(()) => reply_rx,
            Err(TrySendError::Full(_)) => Reply::ready(Err(StorageError::Busy)),
            Err(TrySendError::Disconnected(_)) => Reply::ready(Err(StorageError::Io {
                message: READER_GONE.into(),
            })),
        }
    }
}

fn open_reader(db: &Path) -> Result<Connection, StorageError> {
    let conn = Connection::open_with_flags(
        db,
        OpenFlags::SQLITE_OPEN_READ_ONLY
            | OpenFlags::SQLITE_OPEN_NO_MUTEX
            | OpenFlags::SQLITE_OPEN_URI,
    )
    .map_err(sql_err)?;
    conn.execute_batch(
        "PRAGMA cache_size = -2000; PRAGMA busy_timeout = 5000; PRAGMA query_only = ON;",
    )
    .map_err(sql_err)?;
    Ok(conn)
}

/// 在一个读事务里跑 `f`。`BEGIN` 是延迟事务：第一条 SELECT 定下快照，之后几条语句都读同一时刻。
/// 无论 `f` 成败都结束事务；结束失败时把错误交给调用方（`serve` 随后会换连接）。
fn in_read_tx<T>(
    conn: &Connection,
    f: impl FnOnce(&Connection) -> Result<T, StorageError>,
) -> Result<T, StorageError> {
    conn.execute_batch("BEGIN").map_err(sql_err)?;
    let result = f(conn);
    let end = if conn.is_autocommit() {
        Ok(())
    } else {
        conn.execute_batch("COMMIT").map_err(sql_err)
    };
    match (result, end) {
        (Ok(v), Ok(())) => Ok(v),
        (Err(e), _) => Err(e),
        (Ok(_), Err(e)) => Err(StorageError::Io {
            message: format!("读事务结束失败：{e}"),
        }),
    }
}

fn serve(mut conn: Connection, db: &Path, rx: &Mutex<Receiver<Job>>) {
    loop {
        // 只在取任务时持锁，任务本身不持锁执行
        let job = {
            let guard = rx.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
            guard.recv()
        };
        let Ok(job) = job else { return };
        job(&conn);
        if !conn.is_autocommit() && conn.execute_batch("ROLLBACK").is_err() {
            // 结束不了的读事务会一直挡住 checkpoint：丢掉这条连接，换一条新的
            tracing::error!("读连接卡在事务里，换一条新连接");
            match open_reader(db) {
                Ok(fresh) => conn = fresh,
                Err(e) => {
                    tracing::error!(error = %e, "重开读连接失败，读线程退出");
                    return;
                }
            }
        }
    }
}
