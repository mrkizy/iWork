//! hook 自己的时限（T06 方案 4.3 的第三个期限）：从进程启动算起，到时还没给出结论就自己拒绝。
//!
//! 一个看门狗线程兜住整个进程：读 stdin、连接、写、读卡在哪一步都一样。经过时间取单调时钟与
//! 墙钟的较大者，把系统睡眠也算进去（宁早勿晚：早到只是多一次拒绝，晚到会让 Claude Code 的
//! hook 自然超时，而 hook 超时即放行）。看门狗最多每秒醒一次，醒来重新比一次墙钟：睡眠期间
//! 单调时钟可能不走，单靠它睡满会晚。

use std::time::{Duration, Instant, SystemTime};

/// 看门狗两次检查之间最长睡多久。
pub const POLL: Duration = Duration::from_secs(1);

/// 进程启动那一刻的两种时钟读数。
#[derive(Debug, Clone, Copy)]
pub struct Started {
    mono: Instant,
    wall: SystemTime,
}

impl Started {
    pub fn now() -> Self {
        Self {
            mono: Instant::now(),
            wall: SystemTime::now(),
        }
    }

    /// 经过的时间：两种时钟取较大者。墙钟被往回拨时按 0 算，单调时钟兜底。
    pub fn elapsed(&self) -> Duration {
        let wall = SystemTime::now()
            .duration_since(self.wall)
            .unwrap_or(Duration::ZERO);
        elapsed_max(self.mono.elapsed(), wall)
    }
}

/// 两种时钟的经过时间取较大者（纯函数，单测用）。
pub fn elapsed_max(mono: Duration, wall: Duration) -> Duration {
    mono.max(wall)
}

/// 下一次检查前睡多久：剩余时间与 [`POLL`] 取小；已经到时返回 `None`。
pub fn next_sleep(elapsed: Duration, deadline: Duration) -> Option<Duration> {
    deadline
        .checked_sub(elapsed)
        .filter(|left| !left.is_zero())
        .map(|left| left.min(POLL))
}

/// 起看门狗：到时调用 `on_expire`（它应当输出拒绝并结束进程）。
pub fn arm(started: Started, deadline: Duration, on_expire: impl FnOnce() + Send + 'static) {
    std::thread::spawn(move || {
        while let Some(nap) = next_sleep(started.elapsed(), deadline) {
            std::thread::sleep(nap);
        }
        on_expire();
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    const S: fn(u64) -> Duration = Duration::from_secs;

    #[test]
    fn sleep_counts_whichever_clock_is_ahead() {
        // 系统睡了 10 分钟：单调时钟只走了 5 秒，墙钟走了 605 秒
        assert_eq!(elapsed_max(S(5), S(605)), S(605));
        // 墙钟被往回拨（按 0 算）：单调时钟兜底
        assert_eq!(elapsed_max(S(30), S(0)), S(30));
    }

    #[test]
    fn next_sleep_never_oversleeps_the_deadline() {
        assert_eq!(next_sleep(S(0), S(7185)), Some(POLL));
        assert_eq!(
            next_sleep(Duration::from_millis(7_184_500), S(7185)),
            Some(Duration::from_millis(500))
        );
        assert_eq!(next_sleep(S(7185), S(7185)), None);
        assert_eq!(next_sleep(S(9000), S(7185)), None, "睡过头了也立刻到时");
    }

    #[test]
    fn watchdog_fires_after_deadline() {
        let (tx, rx) = std::sync::mpsc::channel();
        let started = Started::now();
        arm(started, Duration::from_millis(200), move || {
            tx.send(started.elapsed()).unwrap();
        });
        let fired_at = rx.recv_timeout(S(5)).expect("5 秒内到时");
        assert!(fired_at >= Duration::from_millis(200), "{fired_at:?}");
    }
}
