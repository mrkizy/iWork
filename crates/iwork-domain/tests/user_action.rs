//! 亲手执行卡片状态机（方案 4.3「亲手执行卡片 UserActionState」）。

mod support;

use iwork_domain::action::CanonicalPath;
use iwork_domain::approval::VoidReason;
use iwork_domain::common::Sha256;
use iwork_domain::id::{ConversationId, RawUlid, TurnId, UserActionId};
use iwork_domain::machine::{Finite, IllegalTransition};
use iwork_domain::user_action::{
    self, ExitStatus, PROCESS_LOST_ERROR, Proposal, USER_ACTION_MACHINE, USER_ACTION_TRANSITIONS,
    UserActionCard, UserActionState, UserActionStateKind, UserActionTrigger, UserActionTriggerKind,
};
use proptest::prelude::*;
use support::{check_forward, check_reverse, verify_machine};

use UserActionState as S;
use UserActionStateKind as K;
use UserActionTrigger as T;
use UserActionTriggerKind as TK;

fn card_in(state: UserActionState) -> UserActionCard {
    UserActionCard {
        id: UserActionId::from_raw(RawUlid::from_u128(1)),
        turn_id: TurnId::from_raw(RawUlid::from_u128(2)),
        conversation_id: ConversationId::from_raw(RawUlid::from_u128(3)),
        proposal: Proposal::Command {
            command: "echo hi".into(),
            cwd: CanonicalPath("/tmp".into()),
        },
        reason: "需要写凭据目录".into(),
        proposal_hash: Sha256([0; 32]),
        state,
        output_artifact: None,
    }
}

fn run(start: UserActionState, triggers: Vec<UserActionTrigger>) -> Vec<UserActionState> {
    let mut card = card_in(start);
    triggers
        .into_iter()
        .enumerate()
        .map(|(i, t)| {
            card.apply(t).unwrap_or_else(|e| panic!("第 {i} 步：{e}"));
            card.state.clone()
        })
        .collect()
}

#[test]
fn user_action_table_forward() {
    assert_eq!(USER_ACTION_TRANSITIONS.len(), 14, "对照：表行数");
    assert_eq!(
        check_forward(user_action::next_kind, USER_ACTION_TRANSITIONS),
        vec![]
    );
}

#[test]
fn user_action_table_reverse() {
    assert_eq!(
        check_reverse(
            USER_ACTION_MACHINE,
            user_action::next_kind,
            USER_ACTION_TRANSITIONS
        ),
        vec![]
    );
    assert_eq!(
        verify_machine(
            USER_ACTION_MACHINE,
            user_action::next_kind,
            USER_ACTION_TRANSITIONS
        ),
        Ok(())
    );
}

#[test]
fn user_action_scenarios() {
    // 正常执行
    assert_eq!(
        run(
            S::Pending,
            vec![
                T::Execute,
                T::Spawned,
                T::Finished {
                    exit: ExitStatus::Code(0)
                }
            ]
        ),
        vec![
            S::Starting,
            S::Executing,
            S::Done {
                exit: ExitStatus::Code(0)
            }
        ]
    );
    // 明确没起来：失败终态，带原因
    assert_eq!(
        run(
            S::Pending,
            vec![
                T::Execute,
                T::SpawnFailed {
                    error: "找不到程序".into()
                }
            ]
        ),
        vec![
            S::Starting,
            S::Failed {
                error: "找不到程序".into()
            }
        ]
    );
    // 等待失败：结果未知
    assert_eq!(
        run(
            S::Executing,
            vec![T::WaitFailed {
                error: "wait 报错".into()
            }]
        ),
        vec![S::Unknown {
            error: "wait 报错".into()
        }]
    );
    // 三个状态下后台服务中断 → 结果未知，带固定说明
    for start in [S::Starting, S::Executing, S::Stopping] {
        assert_eq!(
            run(start, vec![T::ProcessLost]),
            vec![S::Unknown {
                error: "后台服务中断".into()
            }]
        );
    }
    assert_eq!(PROCESS_LOST_ERROR, "后台服务中断");
    // 用户核对后确认结束
    assert_eq!(
        run(
            S::Unknown { error: "x".into() },
            vec![T::UserConfirmedEnded]
        ),
        vec![S::Ended]
    );
    // 删除对话时停止执行中的卡片，被信号结束的退出也记下
    assert_eq!(
        run(
            S::Executing,
            vec![
                T::Stop,
                T::Finished {
                    exit: ExitStatus::Signal(15)
                }
            ]
        ),
        vec![
            S::Stopping,
            S::Done {
                exit: ExitStatus::Signal(15)
            }
        ]
    );
    // 用户不执行 / 回合取消作废
    assert_eq!(run(S::Pending, vec![T::Dismiss]), vec![S::Dismissed]);
    assert_eq!(
        run(
            S::Pending,
            vec![T::Void {
                reason: VoidReason::ConversationDeleted
            }]
        ),
        vec![S::Voided {
            reason: VoidReason::ConversationDeleted
        }]
    );
    // 进程是用户本人起的：执行中不接受作废
    let mut card = card_in(S::Executing);
    assert_eq!(
        card.apply(T::Void {
            reason: VoidReason::TurnCancelled
        }),
        Err(IllegalTransition {
            machine: USER_ACTION_MACHINE,
            state: K::Executing,
            trigger: TK::Void,
        })
    );
    assert_eq!(card.state, S::Executing);
}

/// 每个种类的一个代表状态（载荷随便取）。
fn sample_states() -> Vec<UserActionState> {
    vec![
        S::Pending,
        S::Starting,
        S::Executing,
        S::Stopping,
        S::Done {
            exit: ExitStatus::Code(1),
        },
        S::Failed { error: "e".into() },
        S::Unknown { error: "e".into() },
        S::Ended,
        S::Dismissed,
        S::Voided {
            reason: VoidReason::AssistantDeleted,
        },
    ]
}

fn sample_triggers() -> Vec<UserActionTrigger> {
    vec![
        T::Execute,
        T::Dismiss,
        T::Void {
            reason: VoidReason::TurnCancelled,
        },
        T::Spawned,
        T::SpawnFailed { error: "e".into() },
        T::Finished {
            exit: ExitStatus::Code(0),
        },
        T::Stop,
        T::WaitFailed { error: "e".into() },
        T::ProcessLost,
        T::UserConfirmedEnded,
    ]
}

/// 带载荷的 next 与按种类的表一致：合法性只由表决定，载荷不改变结论。
#[test]
fn user_action_payload_next_agrees_with_kind_table() {
    let states = sample_states();
    let triggers = sample_triggers();
    // 对照：代表值覆盖了全部种类
    assert_eq!(
        states.iter().map(|s| s.kind()).collect::<Vec<_>>(),
        K::all()
    );
    assert_eq!(
        triggers.iter().map(|t| t.kind()).collect::<Vec<_>>(),
        TK::all()
    );
    for state in &states {
        for trigger in &triggers {
            let by_payload = user_action::next(state, trigger.clone()).map(|s| s.kind());
            let by_kind = user_action::next_kind(state.kind(), trigger.kind());
            assert_eq!(by_payload, by_kind, "{state:?} × {trigger:?}");
        }
    }
}

#[test]
fn user_action_terminal_is_final() {
    let terminal: Vec<_> = K::all().into_iter().filter(|k| k.is_terminal()).collect();
    assert_eq!(
        terminal,
        vec![K::Done, K::Failed, K::Ended, K::Dismissed, K::Voided]
    );
    for state in terminal {
        for trigger in TK::all() {
            assert!(
                user_action::next_kind(state, trigger).is_err(),
                "{state:?} × {trigger:?}"
            );
        }
    }
}

/// 卡片只会被执行一次：只有 Pending 能进 Starting，而没有任何转换回到 Pending。
#[test]
fn user_action_never_reruns() {
    for &(from, trigger, to) in USER_ACTION_TRANSITIONS {
        if to == K::Starting {
            assert_eq!(from, K::Pending, "{from:?} × {trigger:?} 回到了 Starting");
        }
        assert_ne!(to, K::Pending, "{from:?} × {trigger:?} 回到了 Pending");
    }
    // 对照：确实有一行进 Starting
    assert!(
        USER_ACTION_TRANSITIONS
            .iter()
            .any(|&(_, _, to)| to == K::Starting)
    );
}

/// Unknown 没有自动出口：除用户确认外的任何触发都是表外转换。
#[test]
fn user_action_unknown_only_user_confirms() {
    for trigger in TK::all() {
        let result = user_action::next_kind(K::Unknown, trigger);
        if trigger == TK::UserConfirmedEnded {
            assert_eq!(result, Ok(K::Ended));
        } else {
            assert_eq!(
                result,
                Err(IllegalTransition {
                    machine: USER_ACTION_MACHINE,
                    state: K::Unknown,
                    trigger,
                })
            );
        }
    }
    assert!(
        !K::Unknown.is_terminal(),
        "Unknown 不是终态：用户确认前不算结束"
    );
}

fn any_trigger() -> impl Strategy<Value = UserActionTrigger> {
    prop::sample::select(sample_triggers())
}

proptest! {
    /// 随机序列：Starting 至多进入一次；终态之后不再变化；失败不改卡片。
    #[test]
    fn user_action_random_sequences(triggers in prop::collection::vec(any_trigger(), 0..40)) {
        let mut card = card_in(S::Pending);
        let mut starts = 0;
        let mut terminal = None;
        for trigger in triggers {
            let before = card.clone();
            let result = card.apply(trigger);
            if let Some(t) = &terminal {
                prop_assert!(result.is_err());
                prop_assert_eq!(&card.state, t);
            }
            if result.is_err() {
                prop_assert_eq!(&card, &before);
            } else if card.state == S::Starting {
                starts += 1;
            }
            if terminal.is_none() && card.state.is_terminal() {
                terminal = Some(card.state.clone());
            }
        }
        prop_assert!(starts <= 1);
    }
}
