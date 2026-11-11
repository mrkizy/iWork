//! 写线程对外的两个信号（T04 方案 2.6、2.10）：提交观察者与存储失效。
//!
//! - [`CommitObserver`]：写线程一批 `COMMIT` 成功之后、任何一条回执送出之前，按 seq 顺序把新事件交给它。
//!   写线程是唯一知道提交顺序的地方（单写者），发布顺序就从这里取，别处不再排。
//! - [`StoreFailure`] / [`SqliteStore::failed`](crate::SqliteStore::failed)：写连接失效、写线程或读线程意外退出时
//!   给出一个能等的结构化信号。失效后存储的行为不变（写请求照旧立即返回 `Io`），只是多了这个信号。

use std::future::Future;
use std::pin::Pin;
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll, Waker};

use iwork_ports::store::StoredEvent;

/// 一批提交里新写入的东西。
#[derive(Debug, Clone, Copy)]
pub struct CommittedBatch<'a> {
    /// 本批新事件，seq 递增、连续。幂等重发命中旧结果的命令、整条失败回滚的命令都不在里面。
    pub events: &'a [StoredEvent],
    /// 本批新写入的 outbox 条数。
    pub outbox_added: usize,
}

/// 提交观察者。
pub trait CommitObserver: Send + 'static {
    /// 在写线程上调用：一批 `COMMIT` 成功之后、任何一条回执送出之前。**不许阻塞、不许做 I/O**：
    /// 它占着写线程，慢一点所有提交都跟着慢。panic 会让写线程退出，存储随之失效（[`StoreFailure::WriterGone`]）。
    /// 重建投影、检查点、outbox 标记等不产生新事件的写操作不回调。
    fn committed(&mut self, batch: CommittedBatch<'_>);
}

/// 存储为什么不可用了。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StoreFailure {
    /// 回滚失败，写连接不可信。
    WriterBroken,
    /// 写线程没收到关闭请求就退出了（panic）。
    WriterGone,
    /// 有读线程退出了（panic，或卡在事务里的连接换不出新的）。
    ReaderGone,
}

/// 失效信号：只记第一次失效，之后的不覆盖。正常关闭从不设置它。
#[derive(Default)]
pub(crate) struct FailureSignal {
    state: Mutex<FailureState>,
}

#[derive(Default)]
struct FailureState {
    failure: Option<StoreFailure>,
    wakers: Vec<Waker>,
}

impl FailureSignal {
    pub(crate) fn fire(&self, failure: StoreFailure) {
        let wakers = {
            let mut s = self
                .state
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            if s.failure.is_some() {
                return;
            }
            s.failure = Some(failure);
            std::mem::take(&mut s.wakers)
        };
        tracing::error!(?failure, "存储失效");
        wakers.into_iter().for_each(Waker::wake);
    }

    pub(crate) fn wait(self: &Arc<Self>) -> Failed {
        Failed {
            signal: self.clone(),
        }
    }
}

/// 存储失效时完成的 future。可以有多个等待者；正常关闭时永远不完成。
pub struct Failed {
    signal: Arc<FailureSignal>,
}

impl Future for Failed {
    type Output = StoreFailure;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<StoreFailure> {
        let mut s = self
            .signal
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        match s.failure {
            Some(f) => Poll::Ready(f),
            None => {
                if !s.wakers.iter().any(|w| w.will_wake(cx.waker())) {
                    s.wakers.push(cx.waker().clone());
                }
                Poll::Pending
            }
        }
    }
}

/// 线程的退出守卫：线程以 panic 或「不该退出的路」离开时发失效信号；正常退出前调 [`ExitGuard::disarm`]。
pub(crate) struct ExitGuard {
    signal: Arc<FailureSignal>,
    failure: StoreFailure,
    armed: bool,
}

impl ExitGuard {
    pub(crate) fn new(signal: Arc<FailureSignal>, failure: StoreFailure) -> Self {
        Self {
            signal,
            failure,
            armed: true,
        }
    }

    pub(crate) fn disarm(mut self) {
        self.armed = false;
    }
}

impl Drop for ExitGuard {
    fn drop(&mut self) {
        if self.armed {
            self.signal.fire(self.failure);
        }
    }
}
