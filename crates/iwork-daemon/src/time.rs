//! 时钟、定时器与 ID 生成器的实现（T04 方案 2.13）。
//!
//! 规则：凡是量「过了多久」的地方只用单调钟（[`SystemClock::monotonic`]、[`TokioTimer`]、tokio 的定时器）。
//! macOS 的 `Instant` 是 `CLOCK_UPTIME_RAW`、Linux 是 `CLOCK_MONOTONIC`，系统睡眠期间都不走（N2：macOS 实测成立，
//! Linux 待实测，风险 L9），所以睡醒后不用重算任何计时器。

use std::sync::Mutex;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use iwork_domain::common::Timestamp;
use iwork_domain::id::RawUlid;
use iwork_ports::clock::{Clock, IdGenerator, MonotonicInstant, Timer};

/// 墙钟 + 单调钟。单调读数的起点是本实例创建的时刻。
#[derive(Debug)]
pub struct SystemClock {
    origin: Instant,
}

impl SystemClock {
    pub fn new() -> Self {
        Self {
            origin: Instant::now(),
        }
    }
}

impl Default for SystemClock {
    fn default() -> Self {
        Self::new()
    }
}

impl Clock for SystemClock {
    fn now(&self) -> Timestamp {
        let ms = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_or(0, |d| i64::try_from(d.as_millis()).unwrap_or(i64::MAX));
        Timestamp(ms)
    }

    fn monotonic(&self) -> MonotonicInstant {
        let ns = self.origin.elapsed().as_nanos();
        MonotonicInstant(u64::try_from(ns).unwrap_or(u64::MAX))
    }
}

/// `tokio::time::sleep`：tokio 的定时器基于 `Instant`，同样不计睡眠。
#[derive(Debug, Clone, Copy, Default)]
pub struct TokioTimer;

impl Timer for TokioTimer {
    fn sleep(&self, duration: Duration) -> impl std::future::Future<Output = ()> + Send {
        tokio::time::sleep(duration)
    }
}

/// 单调的 ULID 生成器（契约见 `IdGenerator`；参照实现是 `iwork-ports/tests/fakes.rs`）。
/// 同一毫秒内递增随机部分；随机部分用尽进到下一毫秒；墙钟回拨时沿用上一次的毫秒数继续递增。
pub struct MonotonicIds<K: Clock> {
    clock: K,
    last: Mutex<Option<RawUlid>>,
}

impl<K: Clock> MonotonicIds<K> {
    pub fn new(clock: K) -> Self {
        Self {
            clock,
            last: Mutex::new(None),
        }
    }
}

impl<K: Clock> IdGenerator for MonotonicIds<K> {
    fn next(&self) -> RawUlid {
        let now = u64::try_from(self.clock.now().0).unwrap_or(0);
        let mut last = self.last.lock().unwrap_or_else(|e| e.into_inner());
        let id = match *last {
            Some(prev) if now <= prev.timestamp_ms() => prev
                .increment()
                .unwrap_or_else(|| RawUlid::from_parts(prev.timestamp_ms() + 1, fresh_random())),
            _ => RawUlid::from_parts(now, fresh_random()),
        };
        *last = Some(id);
        id
    }
}

/// 新毫秒的随机部分（80 位）。最高位留 0，给同一毫秒内的递增留足余量。
fn fresh_random() -> u128 {
    ulid::Ulid::generate().random() >> 1
}

/// 两次读钟之间墙钟比单调钟多走的部分超过这个值，就认为中间睡过（方案 2.13）。
pub const SLEEP_THRESHOLD: Duration = Duration::from_secs(60);

/// 一次读钟：墙钟毫秒与单调钟读数。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ClockReading {
    pub wall: Timestamp,
    pub mono: MonotonicInstant,
}

impl ClockReading {
    pub fn take(clock: &impl Clock) -> Self {
        Self {
            wall: clock.now(),
            mono: clock.monotonic(),
        }
    }
}

/// 纯函数：两次读钟之间睡了多久。墙钟走过的比单调钟多出 [`SLEEP_THRESHOLD`] 以上才算；墙钟回拨不算。
pub fn slept(prev: ClockReading, now: ClockReading) -> Option<Duration> {
    let wall_ms = now.wall.0.checked_sub(prev.wall.0)?;
    let wall = Duration::from_millis(u64::try_from(wall_ms).ok()?);
    let mono = Duration::from_nanos(now.mono.0.saturating_sub(prev.mono.0));
    let gap = wall.checked_sub(mono)?;
    (gap >= SLEEP_THRESHOLD).then_some(gap)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicI64, AtomicU64, Ordering};

    /// 可以分别拨墙钟与单调钟的假时钟。
    #[derive(Default)]
    struct FakeClock {
        wall_ms: AtomicI64,
        mono_ns: AtomicU64,
    }

    impl Clock for &FakeClock {
        fn now(&self) -> Timestamp {
            Timestamp(self.wall_ms.load(Ordering::SeqCst))
        }
        fn monotonic(&self) -> MonotonicInstant {
            MonotonicInstant(self.mono_ns.load(Ordering::SeqCst))
        }
    }

    /// 用单调钟量的超时：开始时记下单调读数，之后读数差 ≥ limit 才算到点。
    fn expired(clock: &impl Clock, started: MonotonicInstant, limit: Duration) -> bool {
        Duration::from_nanos(clock.monotonic().0 - started.0) >= limit
    }

    /// 卡验收 5：墙钟跳 30 分钟、单调钟只走 5 秒（睡眠期间不走）→ 60s 的超时不触发，`slept` 报出约 30 分钟。
    #[test]
    fn sleep_does_not_expire_timers() {
        let clock = FakeClock::default();
        clock.wall_ms.store(1_000_000, Ordering::SeqCst);
        let started = (&clock).monotonic();
        let before = ClockReading::take(&&clock);

        clock
            .wall_ms
            .fetch_add(30 * 60 * 1000 + 5_000, Ordering::SeqCst);
        clock.mono_ns.fetch_add(5_000_000_000, Ordering::SeqCst);

        assert!(!expired(&&clock, started, Duration::from_secs(60)));
        let gap = slept(before, ClockReading::take(&&clock)).expect("应检测到睡眠");
        assert_eq!(gap, Duration::from_secs(30 * 60));

        // 对照：醒来后单调钟再走 55 秒，超时到点
        clock.mono_ns.fetch_add(55_000_000_000, Ordering::SeqCst);
        assert!(expired(&&clock, started, Duration::from_secs(60)));
    }

    #[test]
    fn slept_ignores_small_gaps_and_wall_clock_going_back() {
        let r = |wall: i64, mono_s: u64| ClockReading {
            wall: Timestamp(wall),
            mono: MonotonicInstant(mono_s * 1_000_000_000),
        };
        assert_eq!(slept(r(0, 0), r(30_000, 30)), None);
        assert_eq!(slept(r(0, 0), r(89_000, 30)), None);
        assert_eq!(slept(r(0, 0), r(90_000, 30)), Some(Duration::from_secs(60)));
        assert_eq!(slept(r(100_000, 0), r(0, 30)), None);
    }

    #[test]
    fn ids_are_strictly_increasing_even_when_wall_clock_goes_back() {
        let clock = FakeClock::default();
        clock.wall_ms.store(5_000, Ordering::SeqCst);
        let ids = MonotonicIds::new(&clock);
        let a = ids.next();
        let b = ids.next();
        clock.wall_ms.store(1_000, Ordering::SeqCst);
        let c = ids.next();
        clock.wall_ms.store(9_000, Ordering::SeqCst);
        let d = ids.next();
        assert!(a < b && b < c && c < d);
        assert_eq!(c.timestamp_ms(), 5_000);
        assert_eq!(d.timestamp_ms(), 9_000);
    }

    #[test]
    fn system_clock_monotonic_advances() {
        let c = SystemClock::new();
        let a = c.monotonic();
        std::thread::sleep(Duration::from_millis(5));
        assert!(c.monotonic().0 >= a.0 + 4_000_000);
        assert!(c.now().0 > 1_700_000_000_000);
    }
}
