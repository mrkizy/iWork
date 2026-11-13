//! 审批 ↔ proto 的终态契约（T09 方案 G3；T10a 方案第 11 节步 7）。

mod support;

use iwork_app::convert::{ConvertError, approval_from_proto, approval_to_proto};
use iwork_domain::approval::{ApprovalState, Decider, VoidReason};
use iwork_domain::machine::Finite;
use iwork_proto::pb;
use proptest::strategy::{Strategy, ValueTree};
use proptest::test_runner::TestRunner;
use support::arb::approval;

/// 四种状态 × 三种批准者 × 全部作废原因：期望值逐个列出，不经 `approval_state_to_proto`。
fn all_states() -> Vec<ApprovalState> {
    let mut states = vec![ApprovalState::Pending, ApprovalState::Denied];
    for decider in [Decider::User, Decider::ModeSwitch, Decider::AiReview] {
        states.push(ApprovalState::Approved { decider });
    }
    for reason in VoidReason::all() {
        states.push(ApprovalState::Voided { reason });
    }
    states
}

#[test]
fn approval_proto_roundtrip_all_states() {
    let mut runner = TestRunner::deterministic();
    let states = all_states();
    // 对照：批准者三种、作废原因不止一种都进了样例
    assert!(states.len() >= 2 + 3 + 2, "{states:?}");
    for state in states {
        for _ in 0..8 {
            let mut a = approval().new_tree(&mut runner).unwrap().current();
            a.state = state;
            let p = approval_to_proto(&a).unwrap_or_else(|e| panic!("{state:?} 转不出：{e}"));
            // 表里的三列逐个对：决定者只在批准时有，作废原因只在作废时有
            let (want_state, has_decider, has_reason) = match state {
                ApprovalState::Pending => (pb::ApprovalState::Pending, false, false),
                ApprovalState::Approved { .. } => (pb::ApprovalState::Approved, true, false),
                ApprovalState::Denied => (pb::ApprovalState::Denied, false, false),
                ApprovalState::Voided { .. } => (pb::ApprovalState::Voided, false, true),
            };
            assert_eq!(p.state, want_state as i32, "{state:?}");
            assert_eq!(p.decider.is_some(), has_decider, "{state:?}");
            assert_eq!(p.void_reason.is_some(), has_reason, "{state:?}");
            assert_eq!(approval_from_proto(&p).unwrap(), a, "{state:?} 往返不等");
        }
    }
}

/// 不合契约表的组合报错并点名字段；对照：合表的同一条能转。
#[test]
fn approval_from_proto_rejects_combinations_outside_the_table() {
    let mut runner = TestRunner::deterministic();
    let mut base = approval().new_tree(&mut runner).unwrap().current();
    base.state = ApprovalState::Pending;
    let ok = approval_to_proto(&base).unwrap();
    assert!(approval_from_proto(&ok).is_ok(), "对照");
    let decider = Some(pb::ApprovalDecider::User as i32);
    let reason = Some(pb::VoidReason::TurnCancelled as i32);
    let cases: [(pb::ApprovalState, Option<i32>, Option<i32>, &str); 7] = [
        (pb::ApprovalState::Pending, decider, None, "decider"),
        (pb::ApprovalState::Pending, None, reason, "void_reason"),
        (pb::ApprovalState::Denied, decider, None, "decider"),
        (pb::ApprovalState::Approved, None, None, "decider"),
        (pb::ApprovalState::Approved, decider, reason, "void_reason"),
        (pb::ApprovalState::Voided, None, None, "void_reason"),
        (pb::ApprovalState::Voided, decider, reason, "decider"),
    ];
    for (state, decider, void_reason, field) in cases {
        let p = pb::Approval {
            state: state as i32,
            decider,
            void_reason,
            ..ok.clone()
        };
        let err = approval_from_proto(&p).expect_err(&format!("{state:?} {decider:?} {void_reason:?}"));
        let named = match &err {
            ConvertError::MissingField { field: f, .. } | ConvertError::InvalidValue { field: f, .. } => *f,
            other => panic!("{other:?}"),
        };
        assert_eq!(named, field, "{state:?} {decider:?} {void_reason:?}");
    }
}
