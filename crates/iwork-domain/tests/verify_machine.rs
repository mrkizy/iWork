//! 对照（方案 4.3「测试怎么写」第 5 类）：验证函数对两个错误的桩实现都必须报错，
//! 否则「正向 + 反向」检查本身失效，空实现也能全绿。

mod support;

use iwork_domain::machine::IllegalTransition;
use iwork_domain::turn::{self, TURN_MACHINE, TURN_TRANSITIONS, TurnState, TurnTrigger};
use support::verify_machine;

/// 任何输入都返回表外转换。
fn always_illegal(
    state: TurnState,
    trigger: TurnTrigger,
) -> Result<TurnState, IllegalTransition<TurnState, TurnTrigger>> {
    Err(IllegalTransition {
        machine: TURN_MACHINE,
        state,
        trigger,
    })
}

/// 任何输入都原地不动。
fn always_identity(
    state: TurnState,
    _trigger: TurnTrigger,
) -> Result<TurnState, IllegalTransition<TurnState, TurnTrigger>> {
    Ok(state)
}

#[test]
fn verify_machine_rejects_always_illegal() {
    let mismatches = verify_machine(TURN_MACHINE, always_illegal, TURN_TRANSITIONS).unwrap_err();
    // 正向每一行都该报（反向那边它恰好对）
    assert_eq!(mismatches.len(), TURN_TRANSITIONS.len());
}

#[test]
fn verify_machine_rejects_always_identity() {
    let mismatches = verify_machine(TURN_MACHINE, always_identity, TURN_TRANSITIONS).unwrap_err();
    assert!(!mismatches.is_empty());
    // 至少一处是反向：表外组合竟然返回了 Ok
    assert!(
        mismatches
            .iter()
            .any(|m| m.expected.is_err() && m.actual.is_ok())
    );
}

#[test]
fn verify_machine_accepts_real_implementation() {
    assert_eq!(
        verify_machine(TURN_MACHINE, turn::next, TURN_TRANSITIONS),
        Ok(())
    );
}
