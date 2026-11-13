//! 崩溃恢复计划（T04 方案 2.5、7.2「恢复计划」）。期望值照方案 2.5 的规则表写死，不经转换表推导：
//! 运行中、等审批 → 已中断（崩溃）；取消中 → 已取消 + 回合结束；流式消息 → 已中断、正文是检查点；
//! 启动中、执行中、停止中的卡片 → 结果未知；审批不动。

use std::collections::HashMap;

use iwork_core::recovery::{RecoveryError, plan};
use iwork_domain::action::CanonicalPath;
use iwork_domain::common::{ArtifactArea, ArtifactRef, ModelId, ModelSelection, Sha256, Timestamp};
use iwork_domain::event::DomainEvent;
use iwork_domain::id::{
    AssistantId, ConversationId, MessageId, RawUlid, RuntimeId, TurnId, UserActionId,
};
use iwork_domain::message::{Author, Message, MessageBody, MessageStatus};
use iwork_domain::turn::{InterruptReason, Turn, TurnKind, TurnState};
use iwork_domain::user_action::{Proposal, UserActionCard, UserActionState};
use iwork_ports::read_model::Unsettled;
use proptest::prelude::*;

fn turn(n: u128, state: TurnState) -> Turn {
    Turn {
        id: TurnId::from_raw(RawUlid::from_u128(n)),
        conversation_id: ConversationId::from_raw(RawUlid::from_u128(1)),
        assistant_id: AssistantId::from_raw(RawUlid::from_u128(2)),
        assistant_version: 1,
        runtime_id: RuntimeId::from_raw(RawUlid::from_u128(3)),
        model: ModelSelection {
            model_id: ModelId("m".into()),
            effort: None,
        },
        runtime_session_id: Some(format!("session-{n}")),
        kind: TurnKind::Assistant,
        state,
        started_at: Timestamp(0),
        ended_at: None,
    }
}

fn streaming(n: u128, turn: Option<u128>, body: &str) -> Message {
    Message {
        id: MessageId::from_raw(RawUlid::from_u128(n)),
        conversation_id: ConversationId::from_raw(RawUlid::from_u128(1)),
        author: Author::Assistant {
            id: AssistantId::from_raw(RawUlid::from_u128(2)),
            name_snapshot: "iWork".into(),
        },
        context_project_id: None,
        status: MessageStatus::Streaming,
        body: MessageBody::Inline(body.into()),
        attachments: vec![],
        created_at: Timestamp(0),
        turn_id: turn.map(|t| TurnId::from_raw(RawUlid::from_u128(t))),
    }
}

fn card(n: u128, state: UserActionState, with_output: bool) -> UserActionCard {
    UserActionCard {
        id: UserActionId::from_raw(RawUlid::from_u128(n)),
        turn_id: TurnId::from_raw(RawUlid::from_u128(1)),
        conversation_id: ConversationId::from_raw(RawUlid::from_u128(1)),
        proposal: Proposal::Command {
            command: "true".into(),
            cwd: CanonicalPath("/work".into()),
        },
        reason: "r".into(),
        proposal_hash: Sha256([1; 32]),
        state,
        output_artifact: with_output.then_some(ArtifactRef {
            sha256: Sha256([9; 32]),
            area: ArtifactArea::Artifacts,
            size: 3,
        }),
    }
}

/// 方案 2.5 规则表：持有进程的回合各自的终点。
fn expected_turn_end(from: TurnState) -> TurnState {
    match from {
        TurnState::Running | TurnState::WaitingApproval => TurnState::Interrupted {
            reason: InterruptReason::Crash,
        },
        TurnState::Cancelling => TurnState::Cancelled,
        other => panic!("测试生成器不该产出 {other:?}"),
    }
}

/// 把计划里的事件逐条作用到输入上（独立于实现的一个小模拟），返回作用后的样子。
/// 每条事件都要合法：from 对得上、消息还在流式、只出现这四种事件。
fn apply(u: &Unsettled, events: &[DomainEvent]) -> Unsettled {
    let mut turns: HashMap<TurnId, Turn> = u.turns.iter().map(|t| (t.id, t.clone())).collect();
    let mut msgs: HashMap<MessageId, Message> =
        u.streaming.iter().map(|m| (m.id, m.clone())).collect();
    let mut cards: HashMap<UserActionId, UserActionCard> =
        u.cards.iter().map(|c| (c.id, c.clone())).collect();
    let mut finished: Vec<TurnId> = Vec::new();
    for e in events {
        match e {
            DomainEvent::TurnStateChanged(c) => {
                let t = turns.get_mut(&c.turn_id).expect("不认识的回合");
                assert_eq!(t.state, c.from, "from 与当前状态不符");
                assert!(!finished.contains(&c.turn_id), "回合结束后又改状态");
                t.state = c.to;
            }
            DomainEvent::TurnFinished(f) => {
                let t = &turns[&f.turn_id];
                assert_eq!(t.state, f.state, "TurnFinished 的状态与回合当前状态不符");
                assert!(f.state.is_terminal());
                assert!(!f.celebrate);
                assert!(!finished.contains(&f.turn_id), "TurnFinished 重复");
                finished.push(f.turn_id);
            }
            DomainEvent::MessageCompleted(c) => {
                let m = msgs.get_mut(&c.message_id).expect("不认识的消息");
                assert_eq!(m.status, MessageStatus::Streaming, "消息已经结束了");
                assert_eq!(c.status, MessageStatus::Interrupted);
                assert_eq!(c.body, m.body, "正文应当是检查点");
                m.status = c.status;
            }
            DomainEvent::UserActionStateChanged(c) => {
                let card = cards.get_mut(&c.card_id).expect("不认识的卡片");
                assert!(
                    matches!(c.state, UserActionState::Unknown { .. }),
                    "{:?}",
                    c.state
                );
                assert_eq!(
                    c.output_artifact, card.output_artifact,
                    "卡片上已有的输出原样带上"
                );
                card.state = c.state.clone();
            }
            other => panic!("恢复不该产生 {}", other.kind()),
        }
    }
    for t in turns.values() {
        if t.state.is_terminal() {
            assert!(finished.contains(&t.id), "到了终态的回合要有 TurnFinished");
        }
    }
    // 再按 unsettled() 的口径筛一遍：恢复之后还剩什么
    let turns: Vec<Turn> = turns
        .into_values()
        .filter(|t| t.state.holds_process())
        .collect();
    let mut out = Unsettled {
        recorded_usage: u
            .recorded_usage
            .iter()
            .filter(|(id, _)| turns.iter().any(|t| t.id == **id))
            .map(|(id, used)| (*id, *used))
            .collect(),
        turns,
        streaming: msgs
            .into_values()
            .filter(|m| m.status == MessageStatus::Streaming)
            .collect(),
        cards: cards
            .into_values()
            .filter(|c| {
                matches!(
                    c.state,
                    UserActionState::Starting
                        | UserActionState::Executing
                        | UserActionState::Stopping
                )
            })
            .collect(),
    };
    out.turns.sort_by_key(|t| t.id);
    out.streaming.sort_by_key(|m| m.id);
    out.cards.sort_by_key(|c| c.id);
    out
}

fn holding_state() -> impl Strategy<Value = TurnState> {
    prop::sample::select(vec![
        TurnState::Running,
        TurnState::WaitingApproval,
        TurnState::Cancelling,
    ])
}

fn in_flight_card_state() -> impl Strategy<Value = UserActionState> {
    prop::sample::select(vec![
        UserActionState::Starting,
        UserActionState::Executing,
        UserActionState::Stopping,
    ])
}

/// 任意组合：至多 6 个持有进程的回合，每个名下 0–2 条流式消息；0–2 条不属于这些回合的流式消息；0–5 张卡片。
fn any_unsettled() -> impl Strategy<Value = Unsettled> {
    (
        prop::collection::vec((holding_state(), 0usize..3), 0..6),
        0usize..3,
        prop::collection::vec((in_flight_card_state(), any::<bool>()), 0..5),
        "[a-z ]{0,12}",
    )
        .prop_map(|(turn_specs, orphans, card_specs, body)| {
            let mut u = Unsettled::default();
            let mut next_msg = 1000u128;
            for (i, (state, msgs)) in turn_specs.into_iter().enumerate() {
                let tn = 100 + i as u128;
                u.turns.push(turn(tn, state));
                for _ in 0..msgs {
                    next_msg += 1;
                    u.streaming.push(streaming(next_msg, Some(tn), &body));
                }
            }
            for k in 0..orphans {
                next_msg += 1;
                let owner = if k % 2 == 0 { None } else { Some(999) };
                u.streaming.push(streaming(next_msg, owner, "orphan"));
            }
            for (i, (state, out)) in card_specs.into_iter().enumerate() {
                u.cards.push(card(500 + i as u128, state, out));
            }
            u.streaming.sort_by_key(|m| m.id);
            u
        })
}

proptest! {
    #[test]
    fn plan_settles_everything_and_is_idempotent(u in any_unsettled()) {
        let p = plan(&u).unwrap();
        // 确定性
        prop_assert_eq!(&plan(&u).unwrap(), &p);
        let after = apply(&u, &p.events);
        prop_assert!(after.turns.is_empty(), "还有回合持有进程：{:?}", after.turns);
        prop_assert!(after.streaming.is_empty(), "还有流式的孤儿：{:?}", after.streaming);
        prop_assert!(after.cards.is_empty(), "还有执行到一半的卡片：{:?}", after.cards);
        // 再算一遍是空的
        let again = plan(&after).unwrap();
        prop_assert!(again.events.is_empty() && again.interrupted.is_empty());
        // 每个回合的终点、interrupted 名单（取消中的不在里面）
        let mut expected_interrupted = Vec::new();
        for t in &u.turns {
            let end = expected_turn_end(t.state);
            let changed = p.events.iter().any(|e| matches!(e,
                DomainEvent::TurnStateChanged(c) if c.turn_id == t.id && c.to == end));
            prop_assert!(changed, "回合 {} 应当到 {:?}", t.id, end);
            if matches!(end, TurnState::Interrupted { .. }) {
                expected_interrupted.push(t.id);
            }
        }
        prop_assert_eq!(p.interrupted, expected_interrupted);
    }
}

#[test]
fn empty_input_gives_empty_plan() {
    let p = plan(&Unsettled::default()).unwrap();
    assert!(p.events.is_empty() && p.interrupted.is_empty());
}

/// 方案 2.5 的规则表逐行：一个运行中、一个取消中的回合，各带一条流式消息，一张执行中的卡片。
#[test]
fn rule_table_rows_in_commit_order() {
    let u = Unsettled {
        turns: vec![turn(1, TurnState::Running), turn(2, TurnState::Cancelling)],
        streaming: vec![streaming(10, Some(1), "ab"), streaming(11, Some(2), "cd")],
        cards: vec![card(20, UserActionState::Executing, true)],
        ..Unsettled::default()
    };
    let p = plan(&u).unwrap();
    let kinds: Vec<&str> = p.events.iter().map(DomainEvent::kind).collect();
    assert_eq!(
        kinds,
        [
            "turn_state_changed",
            "message_completed",
            "turn_state_changed",
            "message_completed",
            "turn_finished",
            "user_action_state_changed",
        ],
        "每个回合：状态 → 它的消息 → （到终态时）结束；最后是卡片"
    );
    assert_eq!(p.interrupted, vec![TurnId::from_raw(RawUlid::from_u128(1))]);
}

#[test]
fn turn_not_holding_process_is_a_named_error() {
    for state in [
        TurnState::Queued,
        TurnState::Completed,
        TurnState::Cancelled,
        TurnState::Failed,
        TurnState::Interrupted {
            reason: InterruptReason::Crash,
        },
    ] {
        let u = Unsettled {
            turns: vec![turn(1, TurnState::Running), turn(7, state)],
            ..Unsettled::default()
        };
        match plan(&u) {
            Err(RecoveryError::Turn {
                turn_id, state: s, ..
            }) => {
                assert_eq!(turn_id, TurnId::from_raw(RawUlid::from_u128(7)));
                assert_eq!(s, state);
            }
            other => panic!("{state:?} 应当报错：{other:?}"),
        }
    }
}

#[test]
fn finished_message_or_settled_card_is_a_named_error() {
    let mut done = streaming(10, None, "x");
    done.status = MessageStatus::Completed;
    let u = Unsettled {
        streaming: vec![done],
        ..Unsettled::default()
    };
    assert!(matches!(plan(&u), Err(RecoveryError::Message { .. })));

    let u = Unsettled {
        cards: vec![card(20, UserActionState::Pending, false)],
        ..Unsettled::default()
    };
    assert!(matches!(plan(&u), Err(RecoveryError::Card { .. })));
}

/// 取消中的回合进程没了，收成已取消：结束事件带上它已记下的用量累计；没记过的带零（事件里的用量是回合累计，
/// 审查 2026-11-13-m1-t10a-d7 1/F-3）。
#[test]
fn finished_turn_carries_recorded_usage() {
    let used = iwork_domain::common::Usage {
        input_tokens: 30,
        output_tokens: 4,
        ..Default::default()
    };
    let u = Unsettled {
        turns: vec![
            turn(1, TurnState::Cancelling),
            turn(2, TurnState::Cancelling),
        ],
        recorded_usage: [(TurnId::from_raw(RawUlid::from_u128(1)), used)].into(),
        ..Unsettled::default()
    };
    let finished: Vec<(TurnId, iwork_domain::common::Usage)> = plan(&u)
        .unwrap()
        .events
        .iter()
        .filter_map(|e| match e {
            DomainEvent::TurnFinished(f) => Some((f.turn_id, f.usage)),
            _ => None,
        })
        .collect();
    assert_eq!(
        finished,
        [
            (TurnId::from_raw(RawUlid::from_u128(1)), used),
            (TurnId::from_raw(RawUlid::from_u128(2)), Default::default()),
        ]
    );
}
