//! 回合状态机（方案 4.3「回合 TurnState」）。

mod support;

use iwork_domain::common::{ModelId, ModelSelection, Timestamp};
use iwork_domain::id::{AssistantId, ConversationId, RawUlid, RuntimeId, TurnId};
use iwork_domain::machine::{Finite, IllegalTransition};
use iwork_domain::turn::{
    self, InterruptReason, TURN_MACHINE, TURN_TRANSITIONS, Turn, TurnKind, TurnState, TurnTrigger,
};
use proptest::prelude::*;
use support::{check_forward, check_reverse, reachable, verify_machine};

use InterruptReason::{ApprovalWaitLimit, Crash};
use TurnState as S;
use TurnTrigger as T;

fn turn_in(state: TurnState) -> Turn {
    Turn {
        id: TurnId::from_raw(RawUlid::from_u128(1)),
        conversation_id: ConversationId::from_raw(RawUlid::from_u128(2)),
        assistant_id: AssistantId::from_raw(RawUlid::from_u128(3)),
        assistant_version: 1,
        runtime_id: RuntimeId::from_raw(RawUlid::from_u128(4)),
        model: ModelSelection {
            model_id: ModelId("m".into()),
            effort: None,
        },
        runtime_session_id: None,
        kind: TurnKind::Assistant,
        state,
        started_at: Timestamp(0),
        ended_at: None,
    }
}

#[test]
fn turn_table_forward() {
    assert_eq!(
        TURN_TRANSITIONS.len(),
        21,
        "对照：表行数，防止表被清空后正向检查空转"
    );
    assert_eq!(check_forward(turn::next, TURN_TRANSITIONS), vec![]);
}

#[test]
fn turn_table_reverse() {
    assert_eq!(
        check_reverse(TURN_MACHINE, turn::next, TURN_TRANSITIONS),
        vec![]
    );
    // 对照：一个确定在表外的组合确实被拒且点名
    assert_eq!(
        turn::next(S::Completed, T::Resume),
        Err(IllegalTransition {
            machine: TURN_MACHINE,
            state: S::Completed,
            trigger: T::Resume,
        })
    );
}

#[test]
fn turn_table_verified() {
    assert_eq!(
        verify_machine(TURN_MACHINE, turn::next, TURN_TRANSITIONS),
        Ok(())
    );
}

/// 逐步应用触发，返回每一步之后的状态。期望值在用例里手写，不从表推。
fn run(start: TurnState, triggers: &[TurnTrigger]) -> Vec<TurnState> {
    let mut turn = turn_in(start);
    triggers
        .iter()
        .enumerate()
        .map(|(i, &t)| {
            turn.apply(t, Timestamp(i as i64 + 1))
                .unwrap_or_else(|e| panic!("第 {i} 步：{e}"))
        })
        .collect()
}

#[test]
fn turn_scenarios() {
    // 正常一轮：中间问了一次权限
    assert_eq!(
        run(
            S::Queued,
            &[
                T::Started,
                T::PermissionAsked,
                T::PermissionDecided,
                T::Completed
            ]
        ),
        vec![S::Running, S::WaitingApproval, S::Running, S::Completed]
    );
    // 运行中取消：先进 Cancelling，进程结束才 Cancelled（需求 12）
    assert_eq!(
        run(S::Running, &[T::Cancel, T::ProcessEnded]),
        vec![S::Cancelling, S::Cancelled]
    );
    // 等审批太久主动中断，审批批准后续接并完成
    assert_eq!(
        run(
            S::WaitingApproval,
            &[T::WaitLimitReached, T::Resume, T::Completed]
        ),
        vec![
            S::Interrupted {
                reason: ApprovalWaitLimit
            },
            S::Running,
            S::Completed
        ]
    );
    // 崩溃后用户放弃
    assert_eq!(
        run(S::WaitingApproval, &[T::ProcessLost, T::Abandon]),
        vec![S::Interrupted { reason: Crash }, S::Cancelled]
    );
    // 等审批时运行时进程意外退出（后台服务还在）：直接失败
    assert_eq!(run(S::WaitingApproval, &[T::Failed]), vec![S::Failed]);
    // 取消途中后台服务重启：直接算取消
    assert_eq!(run(S::Cancelling, &[T::ProcessLost]), vec![S::Cancelled]);
    // 还没起就取消
    assert_eq!(run(S::Queued, &[T::Cancel]), vec![S::Cancelled]);

    // 进入终态时记下结束时间，之前没有
    let mut turn = turn_in(S::Queued);
    turn.apply(T::Started, Timestamp(5)).unwrap();
    assert_eq!(turn.ended_at, None);
    turn.apply(T::Failed, Timestamp(9)).unwrap();
    assert_eq!((turn.state, turn.ended_at), (S::Failed, Some(Timestamp(9))));
}

fn any_state() -> impl Strategy<Value = TurnState> {
    prop::sample::select(TurnState::all())
}

fn any_trigger() -> impl Strategy<Value = TurnTrigger> {
    prop::sample::select(TurnTrigger::all())
}

proptest! {
    /// 随机触发序列下只发生表内转换；表外触发不改回合的任何字段。
    #[test]
    fn turn_only_table_transitions(start in any_state(), triggers in prop::collection::vec(any_trigger(), 0..40)) {
        let mut turn = turn_in(start);
        for (i, trigger) in triggers.into_iter().enumerate() {
            let before = turn.clone();
            match turn.apply(trigger, Timestamp(i as i64)) {
                Ok(to) => prop_assert!(
                    TURN_TRANSITIONS.contains(&(before.state, trigger, to)),
                    "表外转换 {:?} × {:?} → {:?}", before.state, trigger, to
                ),
                Err(_) => prop_assert_eq!(&turn, &before),
            }
        }
    }

    /// 终态之后任何触发都是 IllegalTransition，状态不再变。
    #[test]
    fn turn_terminal_is_final(start in any_state(), triggers in prop::collection::vec(any_trigger(), 0..40)) {
        let mut state = start;
        let mut terminal_seen = None;
        for trigger in triggers {
            let result = turn::next(state, trigger);
            if let Some(terminal) = terminal_seen {
                prop_assert!(result.is_err(), "终态 {:?} 收到 {:?} 竟然转换了", terminal, trigger);
                continue;
            }
            if let Ok(to) = result {
                state = to;
                if to.is_terminal() {
                    terminal_seen = Some(to);
                }
            }
        }
    }
}

#[test]
fn turn_terminal_states_are_exactly_three() {
    let terminal: Vec<_> = TurnState::all()
        .into_iter()
        .filter(|s| s.is_terminal())
        .collect();
    assert_eq!(terminal, vec![S::Completed, S::Cancelled, S::Failed]);
}

/// 每个非终态都有路径到终态：表里没有卡死态（出事场景 S2 的止血前提）。
#[test]
fn turn_every_nonterminal_reaches_terminal() {
    let nonterminal: Vec<_> = TurnState::all()
        .into_iter()
        .filter(|s| !s.is_terminal())
        .collect();
    assert_eq!(
        nonterminal.len(),
        6,
        "对照：Queued/Running/WaitingApproval/Cancelling + 两种 Interrupted"
    );
    for state in nonterminal {
        let reach = reachable(TURN_TRANSITIONS, state);
        assert!(
            reach.iter().any(|s| s.is_terminal()),
            "{state:?} 到不了任何终态，可达：{reach:?}"
        );
    }
}

/// 崩溃恢复不会碰到表外状态：持有进程的三个状态都定义了 ProcessLost。
/// 只查持有进程的状态（主管裁定 (a)）；Queued 与 Interrupted 由下一条用例守住「拒绝」。
#[test]
fn turn_process_lost_defined_for_process_holding_states() {
    let holding: Vec<_> = TurnState::all()
        .into_iter()
        .filter(|s| s.holds_process())
        .collect();
    assert_eq!(holding, vec![S::Running, S::WaitingApproval, S::Cancelling]);
    for state in holding {
        assert!(
            turn::next(state, T::ProcessLost).is_ok(),
            "{state:?} 没有定义 ProcessLost"
        );
    }
}

/// 调用方漏了过滤、对没有进程的回合发 ProcessLost 时被拒，不会悄悄改状态。
#[test]
fn turn_process_lost_illegal_without_process() {
    for state in [
        S::Queued,
        S::Interrupted { reason: Crash },
        S::Interrupted {
            reason: ApprovalWaitLimit,
        },
    ] {
        assert_eq!(
            turn::next(state, T::ProcessLost),
            Err(IllegalTransition {
                machine: TURN_MACHINE,
                state,
                trigger: T::ProcessLost,
            })
        );
    }
}

/// 哪些触发作废审批：手写期望，不从实现推。
#[test]
fn triggers_that_void_pending_approvals() {
    use iwork_domain::approval::VoidReason;
    let voiding: Vec<_> = TurnTrigger::all()
        .into_iter()
        .filter_map(|t| t.void_reason().map(|r| (t, r)))
        .collect();
    assert_eq!(
        voiding,
        vec![
            (T::Cancel, VoidReason::TurnCancelled),
            (T::Failed, VoidReason::TurnFailed),
            (T::Abandon, VoidReason::TurnAbandonedAfterInterrupt),
        ]
    );
    // 对照：等待上限中断不作废
    assert_eq!(T::WaitLimitReached.void_reason(), None);
}

/// 中断的回合可以失败（两种原因都行），失败作废审批；中断的回合不能「完成」。手写期望。
#[test]
fn interrupted_can_fail() {
    for reason in [Crash, ApprovalWaitLimit] {
        assert_eq!(
            turn::next(S::Interrupted { reason }, T::Failed),
            Ok(S::Failed)
        );
    }
    assert_eq!(
        T::Failed.void_reason(),
        Some(iwork_domain::approval::VoidReason::TurnFailed)
    );
    // 对照：中断之后不能直接「完成」
    assert!(turn::next(S::Interrupted { reason: Crash }, T::Completed).is_err());
}

// ---------- 提交时校验（T10a 方案 0a.3 第 A4 行）----------

use iwork_domain::event::TurnStateChanged;
use iwork_domain::turn::{TurnIntentKind, check_intent_committed, check_turn_committed};

/// 转换表里有的「从 → 到」，手写一份当独立的期望值（不从 TURN_TRANSITIONS 推）。
fn legal_edges() -> Vec<(TurnState, TurnState)> {
    let ic = S::Interrupted { reason: Crash };
    let iw = S::Interrupted {
        reason: ApprovalWaitLimit,
    };
    let mut edges = vec![
        (S::Queued, S::Running),
        (S::Queued, S::Cancelled),
        (S::Queued, S::Failed),
        (S::Running, S::WaitingApproval),
        (S::Running, S::Cancelling),
        (S::Running, S::Completed),
        (S::Running, S::Failed),
        (S::Running, ic),
        (S::WaitingApproval, S::Running),
        (S::WaitingApproval, S::Cancelling),
        (S::WaitingApproval, S::Failed),
        (S::WaitingApproval, ic),
        (S::WaitingApproval, iw),
        (S::Cancelling, S::Cancelled),
    ];
    for i in [ic, iw] {
        edges.extend([(i, S::Running), (i, S::Cancelled), (i, S::Failed)]);
    }
    edges
}

fn changed(from: TurnState, to: TurnState) -> TurnStateChanged {
    TurnStateChanged {
        turn_id: TurnId::from_raw(RawUlid::from_u128(1)),
        from,
        to,
        reason: None,
    }
}

/// 现在的状态 × 事件的 from × 事件的 to 全部穷举：只有「现在 == from 且是表里的边」放行。
#[test]
fn check_turn_committed_matches_transition_table() {
    let edges = legal_edges();
    assert_eq!(edges.len(), 20, "对照：手写的边数");
    let mut accepted = 0;
    for current in TurnState::all() {
        for from in TurnState::all() {
            for to in TurnState::all() {
                let expect = current == from && edges.contains(&(from, to));
                let got = check_turn_committed(current, &changed(from, to));
                assert_eq!(
                    got.is_ok(),
                    expect,
                    "现在 {current:?}，事件 {from:?} → {to:?}：{got:?}"
                );
                accepted += usize::from(expect);
            }
        }
    }
    assert_eq!(accepted, 20, "每条边恰好在现在 == from 时放行一次");
    // 拒绝时点名两边的状态
    let err = check_turn_committed(S::Cancelled, &changed(S::Queued, S::Running)).unwrap_err();
    assert!(
        err.message.contains("Queued") && err.message.contains("Cancelled"),
        "{err}"
    );
}

#[test]
fn intent_lands_only_on_its_states() {
    for state in TurnState::all() {
        let cancel = matches!(state, S::Running | S::WaitingApproval);
        let resume = matches!(state, S::Interrupted { .. });
        assert_eq!(
            check_intent_committed(state, TurnIntentKind::Cancel).is_ok(),
            cancel,
            "取消意图落在 {state:?}"
        );
        assert_eq!(
            check_intent_committed(state, TurnIntentKind::Resume).is_ok(),
            resume,
            "继续意图落在 {state:?}"
        );
    }
}
