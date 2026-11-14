//! 识别对端（两端共用；审查 2026-11-14-fix-app-dialer-flake 1/F-1）。
//!
//! - [`identify_by`]：带截止时间识别。超时时先 [`Connection::abort`] 真的关掉通信，再交给调用方按失败处理（不放行）。
//!   只等不关是不够的：阻塞识别取消不掉，它手里还有一份复制的句柄，对端会一直等下去。
//! - [`run_blocking`]：本机传输（unix、Windows）的阻塞部分。算映像哈希、验签要读整个程序文件，放到阻塞线程池；
//!   同时在途的阻塞识别不超过 [`SLOTS`] 个。

use std::io;

use tokio::sync::Semaphore;
use tokio::time::{Instant, error::Elapsed, timeout_at};

use super::Connection;
use crate::peer::{PeerError, PeerIdentity};

/// 同时在途的阻塞识别上限（每个进程）。平时一次识别几十毫秒，同时握手的连接两三条（桌面客户端重连、开发工具），
/// 4 个够用、还有余量。卡住的识别取消不掉，每个占一条阻塞线程（unix 上还占一个复制的 fd）；不设上限的话持续卡住
/// 时每次超时都多叠一个，直到 tokio 阻塞池的 512 条线程，进程退出也被它们拖住。名额满了的识别在截止时间内等，等到
/// 截止就按超时处理。
pub const SLOTS: usize = 4;

static IN_FLIGHT: Semaphore = Semaphore::const_new(SLOTS);

/// 在截止时间内识别对端。超时（含等名额超时）时先关掉通信，返回 `Err`；调用方按识别失败处理，不放行。
pub(crate) async fn identify_by<C: Connection>(
    conn: &C,
    deadline: Instant,
) -> Result<Result<PeerIdentity, PeerError>, Elapsed> {
    let r = timeout_at(deadline, conn.peer()).await;
    if r.is_err() {
        conn.abort();
    }
    r
}

/// 在阻塞线程池上做一次识别。名额在这里拿、随闭包移进阻塞线程，识别真的结束时才归还：等待它的一方超时离开不会
/// 提前把名额还回去。
pub(crate) async fn run_blocking<F>(identify: F) -> Result<PeerIdentity, PeerError>
where
    F: FnOnce() -> Result<PeerIdentity, PeerError> + Send + 'static,
{
    let permit = IN_FLIGHT
        .acquire()
        .await
        .map_err(|e| PeerError::Credentials(io::Error::other(e)))?;
    tokio::task::spawn_blocking(move || {
        let _permit = permit;
        #[cfg(feature = "test-hooks")]
        let _running = hooks::Running::start();
        identify()
    })
    .await
    .map_err(|e| PeerError::Credentials(io::Error::other(e)))?
}

/// 测试用的控制点（`test-hooks` feature；发布构建里没有）：让阻塞识别卡一会儿、数在途的阻塞识别。
#[cfg(feature = "test-hooks")]
pub mod hooks {
    use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
    use std::time::Duration;

    pub use super::SLOTS;

    /// 此刻空着的名额。
    pub fn available() -> usize {
        super::IN_FLIGHT.available_permits()
    }

    static STALL_MS: AtomicU64 = AtomicU64::new(0);
    static RUNNING: AtomicUsize = AtomicUsize::new(0);
    static PEAK: AtomicUsize = AtomicUsize::new(0);

    /// 之后开始的每一次阻塞识别先睡这么久（`std::thread::sleep`，占住阻塞线程，取消不掉）。
    pub fn set_stall(d: Duration) {
        STALL_MS.store(d.as_millis() as u64, Ordering::SeqCst);
    }

    /// 此刻还在跑的阻塞识别个数。
    pub fn running() -> usize {
        RUNNING.load(Ordering::SeqCst)
    }

    /// 上次 [`reset_peak`] 以来同时在跑的最大个数。
    pub fn peak() -> usize {
        PEAK.load(Ordering::SeqCst)
    }

    pub fn reset_peak() {
        PEAK.store(RUNNING.load(Ordering::SeqCst), Ordering::SeqCst);
    }

    /// 一次阻塞识别从开始到结束（随闭包里的值一起丢掉）。
    pub(crate) struct Running;

    impl Running {
        pub(crate) fn start() -> Running {
            let now = RUNNING.fetch_add(1, Ordering::SeqCst) + 1;
            PEAK.fetch_max(now, Ordering::SeqCst);
            let stall = STALL_MS.load(Ordering::SeqCst);
            if stall > 0 {
                std::thread::sleep(Duration::from_millis(stall));
            }
            Running
        }
    }

    impl Drop for Running {
        fn drop(&mut self) {
            RUNNING.fetch_sub(1, Ordering::SeqCst);
        }
    }
}
