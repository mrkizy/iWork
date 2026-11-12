//! 任务 `sleep-watch`（可重启）：每 30s 读一次墙钟与单调钟，墙钟多走了 60s 以上就认为中间睡过（方案 2.13）。
//! 只记日志并通知「醒来了」（组 3 的安装检查据此立刻跑一次）。它不改任何计时器：量时长只用不计睡眠的单调钟。

use std::sync::Arc;
use std::time::Duration;

use iwork_ports::clock::Clock;
use tokio::sync::Notify;

use crate::time::{ClockReading, slept};

pub const INTERVAL: Duration = Duration::from_secs(30);

pub async fn run(clock: Arc<impl Clock + 'static>, woke: Arc<Notify>, interval: Duration) {
    let mut prev = ClockReading::take(clock.as_ref());
    loop {
        tokio::time::sleep(interval).await;
        let now = ClockReading::take(clock.as_ref());
        if let Some(gap) = slept(prev, now) {
            tracing::info!(slept_secs = gap.as_secs(), "检测到系统睡眠后唤醒");
            woke.notify_one();
        }
        prev = now;
    }
}
