//! 三个期限（T06 方案 4.3）：hook 的 timeout 为 h 时，后台、桥接、hook 各自最晚在第几秒收手。
//!
//! 一次调用出三个值，三方的期限出自同一个 h：后台的值进 `initialize` 的 `permissionWaitLimitSecs`，
//! hook 的值进环境变量 `IWORK_HOOK_DEADLINE_SECS`，桥接自己用中间那个兜底。

use std::time::Duration;

/// 取消时发了 `interrupt` 之后，等这一轮的 `result` 最多多久；到点没等到就给 CLI 发 SIGTERM（5.1，D2 选 b″）。
/// 必须小于后台的 3 秒（后台到点结束整棵进程树），SIGTERM 才有机会让 CLI 自己收拾、把会话记录写完整。
pub const CANCEL_TERM_AFTER: Duration = Duration::from_millis(1500);

/// 三方各自的期限，都从 hook 开始等算起。`0 < daemon < bridge < hook < h`。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Deadlines {
    /// 后台的回合编排：到时提交中断（主路径）
    pub daemon: Duration,
    /// 桥接：后台还没答复就兜底拒绝并结束 CLI
    pub bridge: Duration,
    /// hook：桥接还没答复就自己拒绝退出（最后一道）
    pub hook: Duration,
}

/// `daemon = h − min(120s, 0.6h)`；`bridge = h − min(45s, 0.4h)`；`hook = h − min(15s, 0.2h)`。
/// h 小的时候按比例留余量，大的时候余量封顶；顺序在任何 h > 0 下都成立。
pub fn deadlines(h: Duration) -> Deadlines {
    let before =
        |cap_secs: u64, share: f64| h - Duration::from_secs(cap_secs).min(h.mul_f64(share));
    Deadlines {
        daemon: before(120, 0.6),
        bridge: before(45, 0.4),
        hook: before(15, 0.2),
    }
}

#[cfg(test)]
mod tests {
    use proptest::prelude::*;

    use super::*;

    /// 现在的 h = 7200 秒：7080 / 7155 / 7185（方案 4.3 的表）。
    /// 取消时给 SIGTERM 的点要在后台 3 秒结束整棵树之前（5.1）。
    #[test]
    fn cancel_term_comes_before_the_daemon_kills_the_tree() {
        assert!(CANCEL_TERM_AFTER < Duration::from_secs(3));
    }

    #[test]
    fn product_timeout() {
        assert_eq!(
            deadlines(Duration::from_secs(7200)),
            Deadlines {
                daemon: Duration::from_secs(7080),
                bridge: Duration::from_secs(7155),
                hook: Duration::from_secs(7185),
            }
        );
    }

    /// 故障注入用的 10 秒（任务卡验收 2）：余量按比例，4 / 6 / 8 秒。
    #[test]
    fn short_timeout_uses_shares() {
        let d = deadlines(Duration::from_secs(10));
        assert_eq!(
            (d.daemon, d.bridge, d.hook),
            (
                Duration::from_secs(4),
                Duration::from_secs(6),
                Duration::from_secs(8)
            )
        );
    }

    proptest! {
        /// 任何 h（1 毫秒到 G3 测过的最大值 4294968 秒）：0 < daemon < bridge < hook < h。
        #[test]
        fn strictly_ordered(ms in 1u64..=4_294_968_000) {
            let h = Duration::from_millis(ms);
            let d = deadlines(h);
            prop_assert!(Duration::ZERO < d.daemon, "{d:?}");
            prop_assert!(d.daemon < d.bridge, "{d:?}");
            prop_assert!(d.bridge < d.hook, "{d:?}");
            prop_assert!(d.hook < h, "{d:?}");
        }
    }
}
