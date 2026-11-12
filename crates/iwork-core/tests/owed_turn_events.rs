//! `owed_turn_events`（T10a 方案 D5、X2、E2 ③）：没有编排在跑的回合，宿主替它提交的收尾事件。
//! 期望表在这里独立写出，逐个回合状态 × 两种触发穷举。

use iwork_core::turn::{OwedBy, OwedError, owed_turn_events};
use iwork_domain::common::Timestamp;
use iwork_domain::event::{DomainEvent, MessageCompleted, TurnFinished, TurnStateChanged};
use iwork_domain::id::{MessageId, RawUlid};
use iwork_domain::machine::Finite;
use iwork_domain::message::{Author, Message, MessageBody, MessageStatus};
use iwork_domain::turn::{InterruptReason, Turn, TurnState};

mod support;
use support::fold_fx;

fn turn_in(state: TurnState) -> Turn {
    let mut t = fold_fx::turn(1, 1, 1);
    t.state = state;
    t
}

fn streaming(turn: &Turn, n: u128) -> Message {
    Message {
        id: MessageId::from_raw(RawUlid::from_u128(0x9900 + n)),
        conversation_id: turn.conversation_id,
        author: Author::Assistant {
            id: turn.assistant_id,
            name_snapshot: "小助手".into(),
        },
        context_project_id: None,
        status: MessageStatus::Streaming,
        body: MessageBody::Inline(format!("检查点{n}")),
        attachments: Vec::new(),
        created_at: Timestamp(0),
        turn_id: Some(turn.id),
    }
}

fn broken(cancel_requested: bool) -> OwedBy {
    OwedBy::BrokenRun {
        cancel_requested,
        reason: "写不进存储".into(),
    }
}

/// 事件里的状态路径：起点加每一步的终点；没有状态变化时为空。
fn path(events: &[DomainEvent]) -> Vec<TurnState> {
    let mut out = Vec::new();
    for e in events {
        if let DomainEvent::TurnStateChanged(TurnStateChanged { from, to, .. }) = e {
            if out.is_empty() {
                out.push(*from);
            }
            assert_eq!(out.last(), Some(from), "每一步的起点是上一步的终点");
            out.push(*to);
        }
    }
    out
}

const RUN: TurnState = TurnState::Running;
const WAIT: TurnState = TurnState::WaitingApproval;
const CANCELLING: TurnState = TurnState::Cancelling;
const CANCELLED: TurnState = TurnState::Cancelled;
const FAILED: TurnState = TurnState::Failed;

/// 期望（独立写出）：`None` 表示应当报错；`Some(路径)`，空路径表示什么都不欠。
fn expected(state: TurnState, by: &OwedBy) -> Option<Vec<TurnState>> {
    use TurnState::*;
    match (by, state) {
        (_, Completed | Cancelled | Failed) => Some(vec![]),
        (OwedBy::BrokenRun { .. }, Cancelling) => Some(vec![CANCELLING, CANCELLED]),
        (
            OwedBy::BrokenRun {
                cancel_requested: true,
                ..
            },
            Running | WaitingApproval,
        ) => Some(vec![state, CANCELLING, CANCELLED]),
        (
            OwedBy::BrokenRun {
                cancel_requested: false,
                ..
            },
            Running | WaitingApproval,
        ) => Some(vec![state, FAILED]),
        (OwedBy::BrokenRun { .. }, Queued | Interrupted { .. }) => Some(vec![]),
        (OwedBy::Closing, Queued | Interrupted { .. }) => Some(vec![state, CANCELLED]),
        (OwedBy::Closing, Running | WaitingApproval | Cancelling) => None,
    }
}

#[test]
fn owed_events_follow_the_table_for_every_state() {
    let bys = [broken(true), broken(false), OwedBy::Closing];
    let mut nonempty = 0;
    for state in TurnState::all() {
        for by in &bys {
            let turn = turn_in(state);
            let msg = streaming(&turn, 1);
            let got = owed_turn_events(&turn, std::slice::from_ref(&msg), by);
            match expected(state, by) {
                None => assert!(
                    matches!(got, Err(OwedError::Unexpected { .. })),
                    "{state:?} {by:?}: {got:?}"
                ),
                Some(want) if want.is_empty() => {
                    assert_eq!(got.unwrap(), vec![], "{state:?} {by:?}")
                }
                Some(want) => {
                    nonempty += 1;
                    let events = got.unwrap_or_else(|e| panic!("{state:?} {by:?}: {e}"));
                    assert_eq!(path(&events), want, "{state:?} {by:?}");
                    let end = *want.last().unwrap();
                    let msg_status = if end == FAILED {
                        MessageStatus::Failed
                    } else {
                        MessageStatus::Interrupted
                    };
                    // 次序：状态变化 → 消息收尾 → 回合结束
                    let tail = &events[want.len() - 1..];
                    assert_eq!(
                        tail,
                        [
                            MessageCompleted {
                                conversation_id: msg.conversation_id,
                                message_id: msg.id,
                                body: msg.body.clone(),
                                status: msg_status,
                            }
                            .into(),
                            TurnFinished {
                                turn_id: turn.id,
                                state: end,
                                usage: Default::default(),
                                celebrate: false,
                                change_summary: None,
                            }
                            .into(),
                        ],
                        "{state:?} {by:?}"
                    );
                }
            }
        }
    }
    // 对照：表确实比到了东西
    assert!(nonempty >= 8, "{nonempty}");
}

#[test]
fn failure_reason_is_recorded_only_when_failing() {
    let turn = turn_in(RUN);
    let events = owed_turn_events(&turn, &[], &broken(false)).unwrap();
    let DomainEvent::TurnStateChanged(changed) = &events[0] else {
        panic!("{events:?}")
    };
    assert_eq!(changed.reason.as_deref(), Some("写不进存储"));
    let events = owed_turn_events(&turn, &[], &broken(true)).unwrap();
    for e in &events {
        if let DomainEvent::TurnStateChanged(c) = e {
            assert_eq!(c.reason, None);
        }
    }
}

#[test]
fn interrupted_by_wait_limit_closes_like_crash() {
    let turn = turn_in(TurnState::Interrupted {
        reason: InterruptReason::ApprovalWaitLimit,
    });
    let events = owed_turn_events(&turn, &[], &OwedBy::Closing).unwrap();
    assert_eq!(path(&events), [turn.state, CANCELLED]);
}

#[test]
fn foreign_or_finished_message_is_refused() {
    let turn = turn_in(WAIT);
    let mut other = streaming(&turn, 2);
    other.turn_id = Some(fold_fx::tid(99));
    assert!(matches!(
        owed_turn_events(&turn, &[other], &broken(false)),
        Err(OwedError::Message { .. })
    ));
    let mut done = streaming(&turn, 3);
    done.status = MessageStatus::Completed;
    assert!(matches!(
        owed_turn_events(&turn, &[done], &broken(false)),
        Err(OwedError::Message { .. })
    ));
}
