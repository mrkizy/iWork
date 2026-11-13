//! 崩溃恢复（T04 方案 2.5；设计 4；需求 12；设计 7.2a 做法 A）：把上次运行没收尾的东西变成事件。
//!
//! 新的后台服务管不着上一个后台的子进程，所以凡是持有进程的回合、还在流式的消息、执行到一半的卡片，
//! 对它来说都是「丢了进程」：一律按各自转换表里的 `ProcessLost`（消息是 `Interrupted`）走一步。
//! 只写「它停了」的事实：不重发 prompt、不重跑卡片命令、不替用户点任何按钮；待审批不动
//! （`ProcessLost` 不作废审批，[`TurnTrigger::void_reason`] 是 `None`）。续接由用户点「继续」。
//!
//! 纯函数：不读时钟、不做 I/O；同一份输入得到同一份输出。

use std::collections::BTreeMap;

use iwork_domain::event::{
    DomainEvent, MessageCompleted, TurnFinished, TurnStateChanged, UserActionStateChanged,
};
use iwork_domain::id::{MessageId, TurnId, UserActionId};
use iwork_domain::message::{self, Message, MessageStatus, MessageTrigger};
use iwork_domain::turn::{self, TurnIllegal, TurnState, TurnTrigger};
use iwork_domain::user_action::{self, UserActionIllegal, UserActionTrigger};
use iwork_ports::read_model::Unsettled;

/// 恢复要提交的事件，与被标成「已中断」的回合（进 `RecoveryCompleted`）。
#[derive(Debug, Clone, PartialEq)]
pub struct RecoveryPlan {
    pub events: Vec<DomainEvent>,
    pub interrupted: Vec<TurnId>,
}

/// 输入里有不该出现的对象：查询的过滤与转换表对不上，是程序缺陷，不兜底。
#[derive(Debug, Clone, PartialEq, thiserror::Error)]
pub enum RecoveryError {
    #[error("回合 {turn_id} 处于 {state:?}，不能按「进程丢失」收尾：{source}")]
    Turn {
        turn_id: TurnId,
        state: TurnState,
        #[source]
        source: TurnIllegal,
    },
    #[error("消息 {message_id} 处于 {status:?}，不能按「中断」收尾")]
    Message {
        message_id: MessageId,
        status: MessageStatus,
    },
    #[error("卡片 {card_id} 不能按「进程丢失」收尾：{source}")]
    Card {
        card_id: UserActionId,
        #[source]
        source: UserActionIllegal,
    },
}

/// 把没收尾的东西变成事件。
///
/// 每个回合依次是：`TurnStateChanged`（按 `ProcessLost` 查表）→ 它名下流式消息的 `MessageCompleted`
/// （状态 `Interrupted`，正文是检查点）→ 到了终态（取消中 → 已取消）再加 `TurnFinished`。
/// 和回合编排正常收尾的顺序一致。不属于输入里任何回合的流式消息排在后面，之后是卡片。
pub fn plan(unsettled: &Unsettled) -> Result<RecoveryPlan, RecoveryError> {
    let mut messages_by_turn: BTreeMap<Option<TurnId>, Vec<&Message>> = BTreeMap::new();
    for m in &unsettled.streaming {
        messages_by_turn.entry(m.turn_id).or_default().push(m);
    }

    let mut events = Vec::new();
    let mut interrupted = Vec::new();
    for t in &unsettled.turns {
        let to = turn::next(t.state, TurnTrigger::ProcessLost).map_err(|source| {
            RecoveryError::Turn {
                turn_id: t.id,
                state: t.state,
                source,
            }
        })?;
        events.push(
            TurnStateChanged {
                turn_id: t.id,
                conversation_id: Some(t.conversation_id),
                from: t.state,
                to,
                reason: None,
            }
            .into(),
        );
        for m in messages_by_turn.remove(&Some(t.id)).unwrap_or_default() {
            events.push(interrupt_message(m)?);
        }
        if to.is_terminal() {
            events.push(
                TurnFinished {
                    turn_id: t.id,
                    conversation_id: Some(t.conversation_id),
                    state: to,
                    usage: unsettled
                        .recorded_usage
                        .get(&t.id)
                        .copied()
                        .unwrap_or_default(),
                    celebrate: false,
                    change_summary: None,
                }
                .into(),
            );
        }
        if matches!(to, TurnState::Interrupted { .. }) {
            interrupted.push(t.id);
        }
    }
    for m in messages_by_turn.into_values().flatten() {
        events.push(interrupt_message(m)?);
    }
    for c in &unsettled.cards {
        let state =
            user_action::next(&c.state, UserActionTrigger::ProcessLost).map_err(|source| {
                RecoveryError::Card {
                    card_id: c.id,
                    source,
                }
            })?;
        events.push(
            UserActionStateChanged {
                card_id: c.id,
                conversation_id: Some(c.conversation_id),
                state,
                output_artifact: c.output_artifact,
            }
            .into(),
        );
    }
    Ok(RecoveryPlan {
        events,
        interrupted,
    })
}

fn interrupt_message(m: &Message) -> Result<DomainEvent, RecoveryError> {
    let status = message::next(m.status, MessageTrigger::Interrupted).map_err(|_| {
        RecoveryError::Message {
            message_id: m.id,
            status: m.status,
        }
    })?;
    Ok(MessageCompleted {
        conversation_id: m.conversation_id,
        message_id: m.id,
        body: m.body.clone(),
        status,
    }
    .into())
}
