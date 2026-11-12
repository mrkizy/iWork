//! 后台服务生命周期命令的判定（T04 方案 2.8）。纯函数；执行停机归后台服务。

/// 在跑的东西：持有进程的回合数、执行中的亲手执行卡片数。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct RunningCount {
    pub turns: u32,
    pub cards: u32,
}

impl RunningCount {
    pub fn is_idle(self) -> bool {
        self.turns == 0 && self.cards == 0
    }
}

/// `ShutdownDaemon` 的受理结论。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ShutdownDecision {
    /// 受理并停机。
    Proceed,
    /// 有成员在运行且没说要取消：回 `PRECONDITION_FAILED` 带两个数，客户端弹确认后以 `cancel_running = true` 重发
    /// （设计 4「有成员在运行时先确认」）。
    NeedsConfirmation {
        running_turns: u32,
        executing_cards: u32,
    },
}

pub fn decide_shutdown(running: RunningCount, cancel_running: bool) -> ShutdownDecision {
    if cancel_running || running.is_idle() {
        ShutdownDecision::Proceed
    } else {
        ShutdownDecision::NeedsConfirmation {
            running_turns: running.turns,
            executing_cards: running.cards,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn shutdown_needs_confirmation_only_when_running_and_not_cancelling() {
        let idle = RunningCount::default();
        let busy = RunningCount { turns: 2, cards: 1 };
        let cards_only = RunningCount { turns: 0, cards: 1 };
        assert_eq!(decide_shutdown(idle, false), ShutdownDecision::Proceed);
        assert_eq!(decide_shutdown(idle, true), ShutdownDecision::Proceed);
        assert_eq!(decide_shutdown(busy, true), ShutdownDecision::Proceed);
        assert_eq!(
            decide_shutdown(busy, false),
            ShutdownDecision::NeedsConfirmation {
                running_turns: 2,
                executing_cards: 1
            }
        );
        assert_eq!(
            decide_shutdown(cards_only, false),
            ShutdownDecision::NeedsConfirmation {
                running_turns: 0,
                executing_cards: 1
            }
        );
    }
}
