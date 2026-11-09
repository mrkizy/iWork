//! 不依赖任何运行时的一次性应答（方案 2.4、ADR-T02-a）。
//!
//! 写线程与读线程池把结果经 [`Responder`] 送回；调用方拿到的 [`Reply`] 既是 `Future`（端口是
//! `impl Future + Send`），也能在普通线程里阻塞等待（制品存储的 I/O 线程要等写线程登记完再改名）。
//! 发送端没送值就被 drop（线程退出或 panic）时，接收端得到点名的 `Io` 错误，不会永远挂着。

use std::future::Future;
use std::pin::Pin;
use std::sync::{Arc, Condvar, Mutex};
use std::task::{Context, Poll, Waker};

use iwork_ports::store::StorageError;

type Outcome<T> = Result<T, StorageError>;

struct Slot<T> {
    value: Option<Outcome<T>>,
    waker: Option<Waker>,
}

struct Shared<T> {
    slot: Mutex<Slot<T>>,
    ready: Condvar,
}

/// 发送端。只能送一次；没送就 drop 时送出 `Io{dropped}`。
pub(crate) struct Responder<T> {
    shared: Option<Arc<Shared<T>>>,
    dropped: &'static str,
}

/// 接收端。
pub(crate) struct Reply<T> {
    shared: Arc<Shared<T>>,
}

/// 新建一对。`dropped` 是发送端没送值就消失时接收端看到的说明，如「写线程已退出」。
pub(crate) fn channel<T>(dropped: &'static str) -> (Responder<T>, Reply<T>) {
    let shared = Arc::new(Shared {
        slot: Mutex::new(Slot {
            value: None,
            waker: None,
        }),
        ready: Condvar::new(),
    });
    (
        Responder {
            shared: Some(shared.clone()),
            dropped,
        },
        Reply { shared },
    )
}

impl<T> Responder<T> {
    pub(crate) fn send(mut self, outcome: Outcome<T>) {
        if let Some(shared) = self.shared.take() {
            deliver(&shared, outcome);
        }
    }
}

impl<T> Drop for Responder<T> {
    fn drop(&mut self) {
        if let Some(shared) = self.shared.take() {
            deliver(
                &shared,
                Err(StorageError::Io {
                    message: self.dropped.to_owned(),
                }),
            );
        }
    }
}

fn deliver<T>(shared: &Shared<T>, outcome: Outcome<T>) {
    let waker = {
        let mut slot = lock(&shared.slot);
        slot.value = Some(outcome);
        slot.waker.take()
    };
    shared.ready.notify_all();
    if let Some(w) = waker {
        w.wake();
    }
}

/// 锁中毒只可能来自持锁时 panic；槽里的数据仍然完整（只有赋值），照常取用。
fn lock<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
}

impl<T> Reply<T> {
    /// 已经有结果的应答（如队列满时直接回 `Busy`）。
    pub(crate) fn ready(outcome: Outcome<T>) -> Self {
        let (tx, rx) = channel("");
        tx.send(outcome);
        rx
    }

    /// 在当前线程阻塞等结果。不要在异步任务里调用。
    pub(crate) fn wait(self) -> Outcome<T> {
        let mut slot = lock(&self.shared.slot);
        loop {
            if let Some(v) = slot.value.take() {
                return v;
            }
            slot = self
                .shared
                .ready
                .wait(slot)
                .unwrap_or_else(std::sync::PoisonError::into_inner);
        }
    }
}

impl<T> Future for Reply<T> {
    type Output = Outcome<T>;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let mut slot = lock(&self.shared.slot);
        match slot.value.take() {
            Some(v) => Poll::Ready(v),
            None => {
                slot.waker = Some(cx.waker().clone());
                Poll::Pending
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reply_resolves_once() {
        let (tx, rx) = channel::<u32>("gone");
        let t = std::thread::spawn(move || tx.send(Ok(7)));
        assert_eq!(pollster::block_on(rx), Ok(7));
        t.join().unwrap();

        let (tx, rx) = channel::<u32>("gone");
        tx.send(Ok(8));
        assert_eq!(rx.wait(), Ok(8), "阻塞等待也拿得到");
    }

    #[test]
    fn reply_errors_when_sender_dropped() {
        let (tx, rx) = channel::<u32>("写线程已退出");
        std::thread::spawn(move || drop(tx)).join().unwrap();
        assert_eq!(
            pollster::block_on(rx),
            Err(StorageError::Io {
                message: "写线程已退出".into()
            })
        );
    }

    #[test]
    fn ready_reply_is_immediately_available() {
        let rx = Reply::<u32>::ready(Err(StorageError::Busy));
        assert_eq!(rx.wait(), Err(StorageError::Busy));
    }
}
