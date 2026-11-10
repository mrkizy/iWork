//! 每个对话的决策锁（T07 方案 6.1、ADR-T07-c）：锁内可以 `await` 一次提交。
//!
//! 核心不绑定异步运行时，这里用 std `Mutex` + `oneshot` 写一个最小的异步互斥：持有者拿走值，
//! 释放时放回并叫醒一个排队者。等待登记的析构负责取消移交：值已归还时继续叫醒下一位。
//! 检查与排队在同一把 std 锁下完成，没有 `unsafe`。

use std::collections::VecDeque;
use std::ops::{Deref, DerefMut};
use std::sync::Mutex;

use futures_channel::oneshot;

pub struct AsyncLock<T> {
    inner: Mutex<Inner<T>>,
}

struct Inner<T> {
    value: Option<T>,
    waiters: VecDeque<oneshot::Sender<()>>,
}

impl<T> AsyncLock<T> {
    pub fn new(value: T) -> Self {
        Self {
            inner: Mutex::new(Inner {
                value: Some(value),
                waiters: VecDeque::new(),
            }),
        }
    }

    fn std_lock(&self) -> std::sync::MutexGuard<'_, Inner<T>> {
        self.inner.lock().unwrap_or_else(|p| p.into_inner())
    }

    pub async fn lock(&self) -> AsyncGuard<'_, T> {
        loop {
            let wait = {
                let mut inner = self.std_lock();
                if let Some(value) = inner.value.take() {
                    return AsyncGuard {
                        lock: self,
                        value: Some(value),
                    };
                }
                let (tx, rx) = oneshot::channel();
                inner.waiters.push_back(tx);
                WaitRegistration {
                    lock: self,
                    receiver: rx,
                }
            };
            // 发送方被丢弃（不会发生：持有者释放时只会 send）也当作被叫醒，重试即可
            let mut wait = wait;
            let _ = (&mut wait.receiver).await;
        }
    }
}

struct WaitRegistration<'a, T> {
    lock: &'a AsyncLock<T>,
    receiver: oneshot::Receiver<()>,
}

fn wake_next<T>(inner: &mut Inner<T>) {
    while let Some(waiter) = inner.waiters.pop_front() {
        if waiter.send(()).is_ok() {
            break;
        }
    }
}

impl<T> Drop for WaitRegistration<'_, T> {
    fn drop(&mut self) {
        let mut inner = self.lock.std_lock();
        if inner.value.is_some() {
            wake_next(&mut inner);
        }
    }
}

pub struct AsyncGuard<'a, T> {
    lock: &'a AsyncLock<T>,
    value: Option<T>,
}

impl<T> Deref for AsyncGuard<'_, T> {
    type Target = T;
    fn deref(&self) -> &T {
        self.value.as_ref().expect("持有期间值一直在")
    }
}

impl<T> DerefMut for AsyncGuard<'_, T> {
    fn deref_mut(&mut self) -> &mut T {
        self.value.as_mut().expect("持有期间值一直在")
    }
}

impl<T> Drop for AsyncGuard<'_, T> {
    fn drop(&mut self) {
        let mut inner = self.lock.std_lock();
        inner.value = self.value.take();
        wake_next(&mut inner);
    }
}
