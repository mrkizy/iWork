//! 后台服务生命周期命令的受理（T04 方案 2.8、2.12；T10a 方案 E1）：完全退出的判定（纯函数，执行停机归后台服务）；
//! 登录自启的受理（只提交意图，让系统跟上归后台服务的对齐任务）。

/// 退出前要问的数（T10a 方案 E1 的 N）：读库得来。卡片那个数由 T10e 接上，接上之前是 0。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct QuitCounts {
    /// 占着运行时进程的回合
    pub running_turns: u32,
    /// 排着的回合：退出会把它们取消，所以也要问（P2 a）
    pub queued_turns: u32,
    /// 在途的亲手执行卡片
    pub executing_cards: u32,
}

impl QuitCounts {
    pub fn is_zero(self) -> bool {
        self.running_turns == 0 && self.queued_turns == 0 && self.executing_cards == 0
    }
}

/// `try_quit` 一口气做判定要的东西（E1）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct QuitInputs {
    /// 客户端已确认（`ShutdownDaemon.cancel_running`）：不读库，直接退
    pub confirmed: bool,
    /// N：读出来的数；读不出来（存储卡住、2 秒到了）为 `None`，算「不知道」，不算 0
    pub counts: Option<QuitCounts>,
    /// L：内存里已经过了派发边界、还没空下来的会话线数，加上卡片宿主那边已经越界的项数
    pub live: u32,
}

/// `ShutdownDaemon` 的受理结论。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ShutdownDecision {
    /// 受理并停机。
    Proceed,
    /// 没确认，并且有东西在跑或排着（或数目不知道）：回 `PRECONDITION_FAILED` 带数目，客户端弹确认后以
    /// `cancel_running = true` 重发（设计 4「有成员在运行时先确认」）。
    NeedsConfirmation {
        counts: QuitCounts,
        /// N 读不出来：`counts` 里的数不可信
        counts_unknown: bool,
    },
}

/// E1：没确认且（N 不为零、N 不知道、L 不为零）→ 要确认；否则退。纯函数。
pub fn decide_shutdown(i: QuitInputs) -> ShutdownDecision {
    if i.confirmed {
        return ShutdownDecision::Proceed;
    }
    match i.counts {
        Some(c) if c.is_zero() && i.live == 0 => ShutdownDecision::Proceed,
        Some(c) => ShutdownDecision::NeedsConfirmation {
            counts: QuitCounts {
                // 读库之后才过了边界的也算进去：确认框里的数不比此刻在跑的少
                running_turns: c.running_turns.max(i.live),
                ..c
            },
            counts_unknown: false,
        },
        None => ShutdownDecision::NeedsConfirmation {
            counts: QuitCounts {
                running_turns: i.live,
                ..QuitCounts::default()
            },
            counts_unknown: true,
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn n(running: u32, queued: u32, cards: u32) -> Option<QuitCounts> {
        Some(QuitCounts {
            running_turns: running,
            queued_turns: queued,
            executing_cards: cards,
        })
    }

    /// E1 的表逐格：确认了一律退；没确认时 N、L 有一个不为零或 N 不知道就问。期望值按表手写。
    #[test]
    fn shutdown_decision_table() {
        let q = |confirmed, counts, live| {
            decide_shutdown(QuitInputs {
                confirmed,
                counts,
                live,
            })
        };
        assert_eq!(q(true, None, 3), ShutdownDecision::Proceed);
        assert_eq!(q(true, n(2, 1, 1), 0), ShutdownDecision::Proceed);
        assert_eq!(q(false, n(0, 0, 0), 0), ShutdownDecision::Proceed);
        // 只有排着的也要问（P2 a）
        assert_eq!(
            q(false, n(0, 2, 0), 0),
            ShutdownDecision::NeedsConfirmation {
                counts: QuitCounts {
                    running_turns: 0,
                    queued_turns: 2,
                    executing_cards: 0
                },
                counts_unknown: false
            }
        );
        // 读库时还是 0、之后过了边界一个（G3）：照样问
        assert!(
            matches!(q(false, n(0, 0, 0), 1), ShutdownDecision::NeedsConfirmation { counts, .. } if counts.running_turns == 1)
        );
        // 读不出来不当成 0
        assert_eq!(
            q(false, None, 0),
            ShutdownDecision::NeedsConfirmation {
                counts: QuitCounts::default(),
                counts_unknown: true
            }
        );
        assert!(matches!(
            q(false, n(0, 0, 1), 0),
            ShutdownDecision::NeedsConfirmation { .. }
        ));
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
