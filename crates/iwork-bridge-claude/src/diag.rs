//! 不等待的诊断输出（审查 S6-5b 2/F-3）：桥接写 stderr 一律经一个有界队列交给专门的写线程，队列满了就丢。
//!
//! stderr 是后台拿着读端的一条管道，后台不读、或者读端被别的进程拿着不读时，同步写会一直卡住；卡在收尾、panic
//! 处理或者 tokio 的线程上，收尾就到不了。只有写线程自己会被卡住。
//!
//! 不给 fd 2 设 `O_NONBLOCK`：这个标志在「打开文件描述」上，dup 出来的也是同一个描述，所有拿着它的进程（比如经
//! 继承竞态拿到它的别的子进程）都会受影响。
//!
//! 写线程没起（进程内测试里）时，写进来的内容直接丢掉。

use std::io::{self, Write};
use std::sync::OnceLock;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::mpsc::{self, SyncSender};
use std::time::{Duration, Instant};

/// 队列里最多排几条（一条是一行日志或一段 panic 信息）。
const QUEUE: usize = 256;

static TX: OnceLock<SyncSender<Vec<u8>>> = OnceLock::new();
/// 放进了队列、还没写完的条数（[`flush`] 用）。
static PENDING: AtomicUsize = AtomicUsize::new(0);

/// 起写线程。只调一次（`main` 最前面）。
pub fn start() -> io::Result<()> {
    let (tx, rx) = mpsc::sync_channel::<Vec<u8>>(QUEUE);
    TX.set(tx).map_err(|_| io::Error::other("写线程已经起过"))?;
    std::thread::Builder::new()
        .name("iwork-stderr".into())
        .spawn(move || {
            let mut err = io::stderr();
            for bytes in rx {
                let _ = err.write_all(&bytes);
                PENDING.fetch_sub(1, Ordering::SeqCst);
            }
        })
        .map(drop)
}

/// 写一段。不等待：队列满了、写线程没起，就丢掉。返回放进去没有。
pub fn write(bytes: &[u8]) -> bool {
    let Some(tx) = TX.get() else {
        return false;
    };
    PENDING.fetch_add(1, Ordering::SeqCst);
    if tx.try_send(bytes.to_vec()).is_ok() {
        true
    } else {
        PENDING.fetch_sub(1, Ordering::SeqCst);
        false
    }
}

/// 写一行（自动加换行）。
pub fn line(text: &str) -> bool {
    write(format!("{text}\n").as_bytes())
}

/// 等队列写完，最多等 `limit`。只在不赶时间的退出之前用（比如拒绝启动），收尾路径上不用。
pub fn flush(limit: Duration) {
    let deadline = Instant::now() + limit;
    while PENDING.load(Ordering::SeqCst) > 0 && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(5));
    }
}

/// 给日志订阅器用的写入端：`tracing_subscriber::fmt().with_writer(|| diag::Writer)`。
pub struct Writer;

impl Write for Writer {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        write(buf);
        Ok(buf.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}
