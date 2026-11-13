//! 编码边界：编码成功的值，同一个 codec 一定能无损解回；proto 装不下或语义上不该出现的值，
//! 在编码时就点名报错（不能「编得进去、解不出来」）。

mod support;

use std::cell::Cell;

use iwork_app::codec::ProtoEventCodec;
use iwork_app::convert::{ConvertError, event_from_proto, event_to_proto};
use iwork_domain::common::{Timestamp, Usage};
use iwork_domain::event::{DomainEvent, QuotaObserved, TurnFinished};
use iwork_domain::id::{ConversationId, RawUlid, RuntimeId, TurnId};
use iwork_domain::runtime::QuotaWindow;
use iwork_domain::turn::{InterruptReason, TurnState};
use iwork_ports::store::EventCodec;
use proptest::test_runner::{Config, TestCaseError, TestRunner};
use support::arb::boundary_strategies;

fn finished(state: TurnState) -> DomainEvent {
    TurnFinished {
        conversation_id: Some(ConversationId::from_raw(RawUlid::from_u128(2))),
        turn_id: TurnId::from_raw(RawUlid::from_u128(1)),
        state,
        usage: Usage::default(),
        celebrate: false,
        change_summary: None,
    }
    .into()
}

fn quota(used_pct: u8) -> DomainEvent {
    QuotaObserved {
        runtime_id: RuntimeId::from_raw(RawUlid::from_u128(1)),
        windows: vec![QuotaWindow {
            window: "5h".into(),
            used_pct,
            resets_at: None,
            observed_at: Timestamp(0),
        }],
        observed_at: Timestamp(0),
    }
    .into()
}

fn roundtrips(event: &DomainEvent) -> bool {
    let codec = ProtoEventCodec;
    let encoded = codec.encode(event).expect("应能编码");
    codec.decode(encoded.kind, &encoded.payload).as_ref() == Ok(event)
}

#[test]
fn turn_finished_accepts_only_terminal_states() {
    for state in [
        TurnState::Completed,
        TurnState::Cancelled,
        TurnState::Failed,
    ] {
        assert!(roundtrips(&finished(state)), "{state:?}");
    }
    for state in [
        TurnState::Interrupted {
            reason: InterruptReason::Crash,
        },
        TurnState::Interrupted {
            reason: InterruptReason::ApprovalWaitLimit,
        },
        TurnState::Running,
        TurnState::Queued,
    ] {
        assert!(
            matches!(
                event_to_proto(&finished(state)),
                Err(ConvertError::InvalidValue {
                    kind: "TurnFinished",
                    field: "state",
                    ..
                })
            ),
            "{state:?} 应在编码时被拒"
        );
    }
}

#[test]
fn quota_used_pct_is_at_most_100() {
    assert!(roundtrips(&quota(100)));
    assert!(roundtrips(&quota(0)));
    assert!(matches!(
        event_to_proto(&quota(101)),
        Err(ConvertError::InvalidValue {
            kind: "QuotaWindow",
            field: "used_pct",
            ..
        })
    ));
}

/// 宽生成器覆盖拒绝路径：要么编码报错，要么编出来的一定原样解回。同时断言两条路径都走到了。
#[test]
fn encode_success_implies_lossless_decode() {
    for (kind, strategy) in boundary_strategies() {
        let mut runner = TestRunner::new(Config {
            cases: 256,
            ..Config::default()
        });
        let (accepted, rejected) = (Cell::new(0u32), Cell::new(0u32));
        let count = |ok: bool| {
            if ok {
                accepted.set(accepted.get() + 1)
            } else {
                rejected.set(rejected.get() + 1)
            }
        };
        runner
            .run(&strategy, |event| {
                match event_to_proto(&event) {
                    Err(_) => count(false),
                    Ok(proto) => {
                        count(true);
                        let back = event_from_proto(proto).map_err(|e| {
                            TestCaseError::fail(format!("编得进去却解不出来：{e}\n{event:?}"))
                        })?;
                        if back != event {
                            return Err(TestCaseError::fail(format!("往返不等：{event:?}")));
                        }
                    }
                }
                Ok(())
            })
            .unwrap_or_else(|e| panic!("事件 {kind}：{e}"));
        let (accepted, rejected) = (accepted.get(), rejected.get());
        assert!(
            accepted > 0 && rejected > 0,
            "{kind}：接受 {accepted}、拒绝 {rejected}，两条路径都应覆盖"
        );
    }
}
