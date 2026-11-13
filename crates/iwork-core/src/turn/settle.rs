//! 没有编排在跑的回合，欠下的收尾事件（T10a 方案 D5、X2、E2 ③）：宿主替它提交。
//!
//! 和崩溃恢复（`crate::recovery`）同一张转换表，只是触发不同：
//! - **编排以错误收场**（D5）：回合还占着进程。`Cancelling` → `Cancelled`；`Running`、`WaitingApproval` 被要求过
//!   取消的 → 先 `Cancelling` 再 `Cancelled`，没被要求过的 → `Failed`。
//! - **对话在删、完全退出**（X2、E2 ③）：回合没过派发边界。`Queued` → `Cancelled`；`Interrupted` 照「放弃」收成
//!   `Cancelled`。
//!
//! 每个回合依次是：状态变化 → 它名下还在流式的助手消息收尾 → `TurnFinished`。和编排正常收尾的次序一致。
//! 要作废的审批不在这里算：调用方经引擎的 `commit_with(VoidTurn)` 一并提交。
//!
//! 纯函数：不读时钟、不做 I/O。

use iwork_domain::common::Usage;
use iwork_domain::event::{DomainEvent, MessageCompleted, TurnFinished, TurnStateChanged};
use iwork_domain::id::{MessageId, TurnId};
use iwork_domain::message::{self, Message, MessageStatus, MessageTrigger};
use iwork_domain::turn::{self, Turn, TurnIllegal, TurnState, TurnTrigger};

/// 为什么要替回合收尾。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum OwedBy {
    /// 本进程里这个回合的那一趟编排以错误收场（D5）。`cancel_requested`：这一趟被要求过取消（D6 的依据成立）；
    /// `reason`：记成失败时写进状态变化的原因
    BrokenRun {
        cancel_requested: bool,
        reason: String,
    },
    /// 对话在删、或完全退出，没过派发边界的回合收成取消（X2、E2 ③）
    Closing,
}

/// 输入和收尾的规矩对不上：调用方的过滤错了，是程序缺陷，不兜底。
#[derive(Debug, Clone, PartialEq, thiserror::Error)]
pub enum OwedError {
    #[error("回合 {turn_id} 处于 {state:?}，不能按「{by}」收尾")]
    Unexpected {
        turn_id: TurnId,
        state: TurnState,
        by: &'static str,
    },
    #[error(transparent)]
    Illegal(#[from] TurnIllegal),
    #[error("消息 {message_id} 不属于回合 {turn_id}，或不在流式中（{status:?}）")]
    Message {
        turn_id: TurnId,
        message_id: MessageId,
        status: MessageStatus,
    },
}

/// 回合 `turn`（库里此刻的样子）欠下的事件；什么都不欠（已到终态，或这种触发下不归宿主收）时返回空。
/// `streaming`：它名下还在流式的助手消息，正文已用检查点填好。`recorded_usage`：这个回合已记下的用量累计（读
/// 模型里的回合用量，没记过就是零），结束事件原样带上——事件里的用量是回合累计。
pub fn owed_turn_events(
    turn: &Turn,
    streaming: &[Message],
    by: &OwedBy,
    recorded_usage: Usage,
) -> Result<Vec<DomainEvent>, OwedError> {
    if turn.state.is_terminal() {
        return Ok(Vec::new());
    }
    let steps: Vec<(TurnTrigger, Option<String>)> = match (by, turn.state) {
        (OwedBy::BrokenRun { .. }, TurnState::Cancelling) => {
            vec![(TurnTrigger::ProcessEnded, None)]
        }
        (
            OwedBy::BrokenRun {
                cancel_requested: true,
                ..
            },
            TurnState::Running | TurnState::WaitingApproval,
        ) => vec![
            (TurnTrigger::Cancel, None),
            (TurnTrigger::ProcessEnded, None),
        ],
        (
            OwedBy::BrokenRun {
                cancel_requested: false,
                reason,
            },
            TurnState::Running | TurnState::WaitingApproval,
        ) => vec![(TurnTrigger::Failed, Some(reason.clone()))],
        // 第一笔没写进去：库里还排着或还中断着，宿主照 D5a 再派，这里没有欠的
        (OwedBy::BrokenRun { .. }, TurnState::Queued | TurnState::Interrupted { .. }) => {
            return Ok(Vec::new());
        }
        (OwedBy::Closing, TurnState::Queued) => vec![(TurnTrigger::Cancel, None)],
        (OwedBy::Closing, TurnState::Interrupted { .. }) => vec![(TurnTrigger::Abandon, None)],
        (OwedBy::Closing, state) => {
            // 占着进程的回合由取消信号收（D6），编排出错的走 BrokenRun（D5）
            return Err(OwedError::Unexpected {
                turn_id: turn.id,
                state,
                by: "对话在删或退出",
            });
        }
        (OwedBy::BrokenRun { .. }, state) => unreachable!("终态已在上面返回：{state:?}"),
    };

    let mut events = Vec::new();
    let mut state = turn.state;
    for (trigger, reason) in steps {
        let to = turn::next(state, trigger)?;
        events.push(
            TurnStateChanged {
                turn_id: turn.id,
                conversation_id: Some(turn.conversation_id),
                from: state,
                to,
                reason,
            }
            .into(),
        );
        state = to;
    }
    debug_assert!(state.is_terminal(), "收尾总到终态：{state:?}");
    // 和编排收尾同一个口径：失败的回合消息记失败，别的记中断
    let trigger = if state == TurnState::Failed {
        MessageTrigger::Failed
    } else {
        MessageTrigger::Interrupted
    };
    for m in streaming {
        let status = message::next(m.status, trigger)
            .ok()
            .filter(|_| m.turn_id == Some(turn.id))
            .ok_or(OwedError::Message {
                turn_id: turn.id,
                message_id: m.id,
                status: m.status,
            })?;
        events.push(
            MessageCompleted {
                conversation_id: m.conversation_id,
                message_id: m.id,
                body: m.body.clone(),
                status,
            }
            .into(),
        );
    }
    events.push(
        TurnFinished {
            turn_id: turn.id,
            conversation_id: Some(turn.conversation_id),
            state,
            usage: recorded_usage,
            celebrate: false,
            change_summary: None,
        }
        .into(),
    );
    Ok(events)
}
