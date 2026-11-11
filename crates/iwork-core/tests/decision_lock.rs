use std::future::Future;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::task::{Context, Poll, Wake, Waker};

use iwork_core::approval::engine::lock::AsyncLock;

#[derive(Default)]
struct Wakes(AtomicUsize);
impl Wake for Wakes {
    fn wake(self: Arc<Self>) {
        self.0.fetch_add(1, Ordering::SeqCst);
    }
}

#[test]
fn cancelled_notified_waiter_hands_off_wakeup() {
    let lock = AsyncLock::new(42);
    let held = pollster::block_on(lock.lock());
    let a = Arc::new(Wakes::default());
    let b = Arc::new(Wakes::default());
    let wa = Waker::from(a.clone());
    let wb = Waker::from(b.clone());
    let mut first = Box::pin(lock.lock());
    let mut second = Box::pin(lock.lock());
    assert!(
        first
            .as_mut()
            .poll(&mut Context::from_waker(&wa))
            .is_pending()
    );
    assert!(
        second
            .as_mut()
            .poll(&mut Context::from_waker(&wb))
            .is_pending()
    );
    drop(held);
    assert_eq!(a.0.load(Ordering::SeqCst), 1);
    assert_eq!(b.0.load(Ordering::SeqCst), 0);
    drop(first);
    assert_eq!(b.0.load(Ordering::SeqCst), 1);
    match second.as_mut().poll(&mut Context::from_waker(&wb)) {
        Poll::Ready(value) => assert_eq!(*value, 42),
        Poll::Pending => panic!("取消后必须唤醒下一位"),
    };
}
