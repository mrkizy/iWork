//! 回合宿主与受理要读的查询（T10a 方案 2.2）与投影的新列：一趟的起点、两种意图、中断时清继续意图、
//! 排进来与结束的 seq。期望值都按这条用例自己提交的先后手算。

mod support;

use iwork_domain::approval::{ConversationPresence, OwnerFacts, Presence};
use iwork_domain::approval_mode::ApprovalMode;
use iwork_domain::conversation::ConversationState;
use iwork_domain::event::*;
use iwork_domain::id::{ConversationId, Seq};
use iwork_domain::rule::{RuleLayer, RuleScope};
use iwork_domain::session::SessionLostReason;
use iwork_domain::turn::{InterruptReason, Turn, TurnIntentKind, TurnState};
use iwork_ports::read_model::{QuitCounts, ReadModelPort};
use iwork_ports::store::{StorageError, StoragePort};
use support::*;

struct Feed<'a> {
    store: &'a Store,
    next: std::cell::Cell<u128>,
}

impl Feed<'_> {
    /// 一笔提交，返回最后一条事件的 seq。
    fn ok(&self, events: Vec<DomainEvent>) -> Seq {
        let n = self.next.get();
        self.next.set(n + 1);
        block(self.store.commit(cmd(n, events)))
            .expect("提交")
            .last_seq
    }
}

/// 这组用例的回合排在哪个对话（和下面造的世界一致）。
fn conv_of(turn: u128) -> Option<ConversationId> {
    Some(cid(match turn {
        20 | 21 => 10,
        22 => 11,
        23 => 12,
        other => panic!("用例里没有回合 {other}"),
    }))
}

fn change(turn: u128, from: TurnState, to: TurnState) -> DomainEvent {
    TurnStateChanged {
        conversation_id: conv_of(turn),
        turn_id: tid(turn),
        from,
        to,
        reason: None,
    }
    .into()
}

fn intent(turn: u128, intent: TurnIntentKind) -> DomainEvent {
    TurnIntentRecorded {
        conversation_id: conv_of(turn),
        turn_id: tid(turn),
        intent,
    }
    .into()
}

fn bound(turn: u128, id: &str, replaced: Option<&str>) -> DomainEvent {
    TurnSessionBound {
        conversation_id: conv_of(turn),
        turn_id: tid(turn),
        runtime_session_id: id.into(),
        replaced: replaced.map(|old| SessionReplaced {
            old_runtime_session_id: old.into(),
            reason: SessionLostReason::NotFound,
        }),
    }
    .into()
}

const AWL: TurnState = TurnState::Interrupted {
    reason: InterruptReason::ApprovalWaitLimit,
};

#[test]
fn turn_work_bindings_counts_and_snapshot_inputs() {
    let env = Env::new();
    let store = env.open();
    let rm = read_model(&store);
    let feed = Feed {
        store: &store,
        next: std::cell::Cell::new(1),
    };
    feed.ok(vec![
        ProjectAdded {
            project: project(1),
        }
        .into(),
        AssistantCreated {
            assistant: assistant(1, false),
        }
        .into(),
        AssistantCreated {
            assistant: assistant(2, false),
        }
        .into(),
        ConversationCreated {
            conversation: project_conversation(10, 1),
        }
        .into(),
        ConversationCreated {
            conversation: dm(11, 2),
        }
        .into(),
        ConversationCreated {
            conversation: project_conversation(12, 1),
        }
        .into(),
    ]);
    feed.ok(vec![TurnQueued { turn: turn(20, 10) }.into()]);
    feed.ok(vec![TurnQueued { turn: turn(21, 10) }.into()]);
    feed.ok(vec![
        TurnQueued {
            turn: Turn {
                assistant_id: aid(2),
                ..turn(22, 11)
            },
        }
        .into(),
    ]);

    // 回合 20：跑起来、被要求取消、中断、被要求继续、续接、再次因等待上限中断
    feed.ok(vec![
        bound(20, "s-a", None),
        change(20, TurnState::Queued, TurnState::Running),
    ]);
    let cancel = feed.ok(vec![intent(20, TurnIntentKind::Cancel)]);
    feed.ok(vec![change(
        20,
        TurnState::Running,
        TurnState::Interrupted {
            reason: InterruptReason::Crash,
        },
    )]);
    feed.ok(vec![intent(20, TurnIntentKind::Resume)]);
    let resumed = feed.ok(vec![
        bound(20, "s-b", Some("s-a")),
        change(
            20,
            TurnState::Interrupted {
                reason: InterruptReason::Crash,
            },
            TurnState::Running,
        ),
    ]);
    // 续接失败的恢复退回旧编号：补记一条记回 s-a 的绑定（它是最后一条）
    feed.ok(vec![bound(20, "s-a", None)]);
    feed.ok(vec![change(
        20,
        TurnState::Running,
        TurnState::WaitingApproval,
    )]);
    let interrupted = feed.ok(vec![change(20, TurnState::WaitingApproval, AWL)]);

    // 回合 22 在私聊里跑着
    let run22 = feed.ok(vec![change(22, TurnState::Queued, TurnState::Running)]);

    // 对话 12：一个跑完的回合留下一张待处理的卡片，然后进删除中
    feed.ok(vec![TurnQueued { turn: turn(23, 12) }.into()]);
    feed.ok(vec![change(23, TurnState::Queued, TurnState::Running)]);
    feed.ok(vec![
        UserActionProposed {
            card: card(1, 23, 12),
        }
        .into(),
    ]);
    feed.ok(vec![
        change(23, TurnState::Running, TurnState::Completed),
        TurnFinished {
            conversation_id: conv_of(23),
            turn_id: tid(23),
            state: TurnState::Completed,
            usage: Default::default(),
            celebrate: false,
            change_summary: None,
        }
        .into(),
    ]);
    feed.ok(vec![
        ConversationDeleting {
            conversation_id: cid(12),
        }
        .into(),
    ]);

    // 设置与清单
    feed.ok(vec![
        ApprovalModeChanged {
            target: ApprovalModeTarget::Member {
                conversation_id: cid(10),
                assistant_id: aid(1),
            },
            mode: Some(ApprovalMode::Auto),
            cause: ApprovalModeChangeCause::User,
        }
        .into(),
    ]);
    let lists = feed.ok(vec![
        SecurityListEntryAdded {
            rule: rule(1, RuleLayer::SensitiveDirs, RuleScope::Global),
        }
        .into(),
    ]);
    let head = feed.ok(vec![]);

    // ---- turn_work ----
    let work = block(rm.turn_work()).unwrap();
    assert_eq!(
        work.head, lists,
        "head 是这个读事务看到的最新 seq（空提交不占号）"
    );
    assert_eq!(head, lists);
    let ids: Vec<_> = work
        .conversations
        .iter()
        .map(|c| (c.conversation.id, c.conversation.state, c.cards_open))
        .collect();
    assert_eq!(
        ids,
        vec![
            (cid(10), ConversationState::Active, false),
            (cid(11), ConversationState::Active, false),
            (cid(12), ConversationState::Deleting, true),
        ]
    );
    let c10 = &work.conversations[0];
    let open: Vec<_> = c10
        .open_turns
        .iter()
        .map(|t| (t.turn.id, t.turn.state))
        .collect();
    assert_eq!(
        open,
        vec![(tid(20), AWL), (tid(21), TurnState::Queued)],
        "按排进来的先后：最老的在前"
    );
    let t20 = &c10.open_turns[0];
    assert_eq!(
        t20.run_started_seq,
        Some(Seq(resumed.0)),
        "一趟的起点是续接那一笔"
    );
    assert_eq!(t20.cancel_intent_seq, Some(cancel));
    assert_eq!(t20.interrupted_seq, Some(interrupted));
    assert_eq!(
        t20.resume_intent_seq, None,
        "再次中断时清掉上一次的继续意图"
    );
    assert_eq!(
        t20.owner,
        OwnerFacts {
            turn: Presence::Present,
            conversation: ConversationPresence::Present,
            assistant: Presence::Present,
        }
    );
    let t21 = &c10.open_turns[1];
    assert_eq!(
        (
            t21.run_started_seq,
            t21.interrupted_seq,
            t21.cancel_intent_seq
        ),
        (None, None, None)
    );
    let t22 = &work.conversations[1].open_turns[0];
    assert_eq!(t22.run_started_seq, Some(run22));
    assert!(work.conversations[2].open_turns.is_empty());

    // ---- 绑定 ----
    assert_eq!(
        block(rm.last_session_binding(cid(10))).unwrap(),
        Some("s-a".into()),
        "最后一条是记回旧编号的那一条"
    );
    assert_eq!(
        block(rm.session_ids_of(cid(10))).unwrap(),
        vec!["s-a".to_owned(), "s-b".to_owned()]
    );
    assert_eq!(block(rm.last_session_binding(cid(11))).unwrap(), None);
    assert!(block(rm.session_ids_of(cid(11))).unwrap().is_empty());

    // ---- 退出前的数 ----
    assert_eq!(
        block(rm.quit_counts()).unwrap(),
        QuitCounts {
            running_turns: 1,
            queued_turns: 1,
        }
    );

    // ---- 会话快照的输入 ----
    let inputs = block(rm.snapshot_inputs(cid(10))).unwrap();
    assert_eq!(inputs.conversation.id, cid(10));
    assert_eq!(inputs.member_modes, vec![(aid(1), ApprovalMode::Auto)]);
    assert_eq!(
        inputs.projects.iter().map(|p| p.id).collect::<Vec<_>>(),
        vec![pid(1)]
    );
    assert_eq!(
        inputs.assistants.iter().map(|a| a.id).collect::<Vec<_>>(),
        vec![aid(1), aid(2)]
    );
    assert_eq!(
        inputs
            .security_lists
            .iter()
            .map(|r| r.id)
            .collect::<Vec<_>>(),
        vec![rid(1)]
    );
    assert_eq!(inputs.lists_seq, lists);
    assert!(matches!(
        block(rm.snapshot_inputs(cid(99))),
        Err(StorageError::NotFound { .. })
    ));

    // 重建之后投影的新列一样（投影器是唯一的写者，重建与在线同一口径）
    let before = dump_projections(&env.db());
    block(store.rebuild_projections()).unwrap();
    assert_eq!(dump_projections(&env.db()), before);
}
