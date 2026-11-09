//! 只读连接池（方案 2.4）：N 个线程各持一条只读连接，共享一个有界任务队列。
//!
//! 读事务都是一条语句的隐式事务（或一次查询里几条语句），结束即释放，不长期挡住 WAL checkpoint。
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
            std::thread::Builder::new()
                .name(format!("iwork-store-reader-{i}"))
                .spawn(move || serve(&conn, &rx))
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
        let job: Job = Box::new(move |conn| reply_tx.send(f(conn)));
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

fn serve(conn: &Connection, rx: &Mutex<Receiver<Job>>) {
    loop {
        // 只在取任务时持锁，任务本身不持锁执行
        let job = {
            let guard = rx.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
            guard.recv()
        };
        match job {
            Ok(job) => job(conn),
            Err(_) => return,
        }
    }
}
