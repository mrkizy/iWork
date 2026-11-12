//! 后台服务生命周期命令的受理（T04 方案 2.8、2.12）：完全退出的判定（纯函数，执行停机归后台服务）；
//! 登录自启的受理（只提交意图，让系统跟上归后台服务的对齐任务）。

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

/// 受理 `SetAutostart` 失败。
#[derive(Debug, Clone, PartialEq, thiserror::Error)]
pub enum SetAutostartError {
    /// 这台机器做不了登录自启（`PRECONDITION_FAILED`，带原因）。没提交任何东西。
    #[error("{0}")]
    Unsupported(String),
    #[error(transparent)]
    Storage(#[from] iwork_ports::store::StorageError),
}

/// 受理 `SetAutostart{enabled}`（T04 方案 2.12）：
/// 1. `port.supported()`：不支持 → 报错，不提交（只读）；
/// 2. 用命令自己的 `command_id` 提交 `GlobalSettingsChanged{autostart: Some(enabled)}`；
/// 3. 到此为止，**不调 `port.set`**——让系统跟上已提交的设置是后台对齐任务的事。同一个 `command_id` 重发：
///    存储返回上次的回执（`replayed`），不产生事件。
pub async fn set_autostart<P, S>(
    port: &P,
    store: &S,
    command_id: iwork_domain::id::CommandId,
    actor: iwork_domain::common::Actor,
    enabled: bool,
) -> Result<iwork_ports::store::CommitReceipt, SetAutostartError>
where
    P: iwork_ports::autostart::AutostartPort,
    S: iwork_ports::store::StoragePort,
{
    port.supported()
        .map_err(|e| SetAutostartError::Unsupported(e.to_string()))?;
    let receipt = store
        .commit(iwork_ports::store::CommitRequest {
            command_id,
            actor,
            events: vec![
                iwork_domain::event::GlobalSettingsChanged {
                    playful_ui: None,
                    autostart: Some(enabled),
                    projects_root: None,
                }
                .into(),
            ],
            outbox: vec![],
        })
        .await?;
    Ok(receipt)
}
