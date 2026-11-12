//! 任务 `publish-check`（致命）：ipc 上报的两种后台故障分开处理（方案 2.10、S6）。
//!
//! - `PublishStalled`（交接等不到发布水位）：收到时读库里的 head，2s 后它仍超过已发布的水位，说明发布链路断了，
//!   在线客户端都会漏事件 → 任务结束，监督器整体退出（20），重启后客户端重连续传。两者相等是那条连接上的瞬时情况，
//!   只把 `ipc_faults` 加一。
//! - `HandoverTimeout` 等别的后台故障：多半是负载高，重启治不了还会打断回合，只记数。

use std::future::Future;
use std::time::Duration;

use iwork_ipc::server::FailCause;
use iwork_ports::store::StorageError;
use tokio::sync::broadcast::{self, error::RecvError};

use crate::logging::AuditSignal;

/// 收到 `PublishStalled` 后等多久再核对。
pub const SETTLE: Duration = Duration::from_secs(2);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Check {
    /// 有事件提交了却没发布。
    Stalled {
        head: u64,
        published: u64,
    },
    Transient,
}

/// 纯函数。`published` 是发布水位：观察者交给发布方的最高 seq 与 serve 起来时的基线中较大的那个。
pub fn check(head: u64, published: u64) -> Check {
    if head > published {
        Check::Stalled { head, published }
    } else {
        Check::Transient
    }
}

/// 库里的 head。
pub trait HeadSource: Send + Sync {
    fn head(&self) -> impl Future<Output = Result<u64, StorageError>> + Send;
}

/// 任务体：发布链路确认断了才返回。
pub async fn run(
    mut signals: broadcast::Receiver<AuditSignal>,
    head: impl HeadSource,
    published: impl Fn() -> u64 + Send,
    count_fault: impl Fn() + Send,
    settle: Duration,
) {
    loop {
        let cause = match signals.recv().await {
            Ok(AuditSignal::ServerFault { cause }) => cause,
            Ok(AuditSignal::PeerRejectedByServer) => continue,
            Err(RecvError::Lagged(n)) => {
                tracing::warn!(missed = n, "审计旁路积压，丢了一些信号");
                continue;
            }
            Err(RecvError::Closed) => return std::future::pending().await,
        };
        if cause != FailCause::PublishStalled.as_str() {
            tracing::warn!(%cause, "ipc 报告后台故障，只记数");
            count_fault();
            continue;
        }
        // 先读 head、等 settle、再读水位：那一刻已提交的事件，过了 settle 还没发布才算断了。
        // 反过来（先等、后读 head 再读水位）时，两次读之间刚提交的一条会被误判
        let head = match head.head().await {
            Ok(h) => h,
            Err(e) => {
                tracing::warn!(error = %e, "核对发布时读 head 失败，只记数");
                count_fault();
                continue;
            }
        };
        tokio::time::sleep(settle).await;
        match check(head, published()) {
            Check::Stalled { head, published } => {
                tracing::error!(
                    head,
                    published,
                    "发布链路断了：有事件提交了却没发布，整体退出"
                );
                return;
            }
            Check::Transient => {
                tracing::warn!(head, "交接等不到发布水位，核对时已追上，只记数");
                count_fault();
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};

    struct Head(Arc<AtomicU64>);
    impl HeadSource for Head {
        async fn head(&self) -> Result<u64, StorageError> {
            Ok(self.0.load(Ordering::SeqCst))
        }
    }

    fn signal(cause: FailCause) -> AuditSignal {
        AuditSignal::ServerFault {
            cause: cause.as_str().into(),
        }
    }

    /// S6：head 超过已发布 → 任务结束（监督器据此整体退出 20）。
    #[tokio::test(start_paused = true)]
    async fn publish_stall_confirmed_exits() {
        let (tx, rx) = broadcast::channel(8);
        let faults = Arc::new(AtomicU32::new(0));
        let f = faults.clone();
        let task = tokio::spawn(run(
            rx,
            Head(Arc::new(AtomicU64::new(10))),
            || 7,
            move || {
                f.fetch_add(1, Ordering::SeqCst);
            },
            SETTLE,
        ));
        tx.send(signal(FailCause::PublishStalled)).unwrap();
        tokio::time::timeout(Duration::from_secs(5), task)
            .await
            .expect("确认断了应结束")
            .unwrap();
        assert_eq!(faults.load(Ordering::SeqCst), 0);
    }

    /// 1/F-6：收到信号时读的 head 在 settle 期间追上了（之后又有新提交让 head 变大）→ 不算断。
    #[tokio::test(start_paused = true)]
    async fn head_is_read_before_settle() {
        let (tx, rx) = broadcast::channel(8);
        let head = Arc::new(AtomicU64::new(10));
        let published = Arc::new(AtomicU64::new(9));
        let (h2, p2) = (head.clone(), published.clone());
        let task = tokio::spawn(run(
            rx,
            Head(head.clone()),
            move || p2.load(Ordering::SeqCst),
            || {},
            SETTLE,
        ));
        tx.send(signal(FailCause::PublishStalled)).unwrap();
        tokio::time::sleep(Duration::from_millis(500)).await;
        // settle 期间：10 发布了，又提交了 11（还没发布）
        published.store(10, Ordering::SeqCst);
        h2.store(11, Ordering::SeqCst);
        tokio::time::sleep(Duration::from_secs(5)).await;
        assert!(
            !task.is_finished(),
            "收到信号那一刻的 head 已发布，不该判断了"
        );
        task.abort();
    }

    /// S6：head 与已发布相等 → 只加计数，不退出；交接超时同样只记数。
    #[tokio::test(start_paused = true)]
    async fn publish_stall_transient_only_counts() {
        let (tx, rx) = broadcast::channel(8);
        let faults = Arc::new(AtomicU32::new(0));
        let f = faults.clone();
        let head = Arc::new(AtomicU64::new(10));
        let task = tokio::spawn(run(
            rx,
            Head(head.clone()),
            || 10,
            move || {
                f.fetch_add(1, Ordering::SeqCst);
            },
            SETTLE,
        ));
        tx.send(signal(FailCause::PublishStalled)).unwrap();
        tx.send(signal(FailCause::HandoverTimeout)).unwrap();
        tx.send(AuditSignal::PeerRejectedByServer).unwrap();
        tokio::time::sleep(Duration::from_secs(10)).await;
        assert!(!task.is_finished(), "瞬时情况不退出");
        assert_eq!(faults.load(Ordering::SeqCst), 2);
        task.abort();
    }

    #[test]
    fn check_compares_head_with_watermark() {
        assert_eq!(check(5, 5), Check::Transient);
        assert_eq!(check(4, 5), Check::Transient);
        assert_eq!(
            check(6, 5),
            Check::Stalled {
                head: 6,
                published: 5
            }
        );
    }
}
