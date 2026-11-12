//! 回合的三处提交时校验（T10a 方案 2.2、0a.3 的 A3、A4、X5）在写线程上的样子，以及真存储与参照投影
//! （`iwork-ports/tests/reference_projection`，iwork-core 的假存储用它做提交时校验）的对拍。

#[path = "../../iwork-ports/tests/reference_projection/mod.rs"]
mod reference_projection;

use std::cell::Cell;
use std::collections::BTreeMap;

use iwork_domain::event::*;
use iwork_domain::id::{Seq, TurnId};
use iwork_domain::machine::Finite;
use iwork_domain::turn::{InterruptReason, Turn, TurnIntentKind, TurnState};
use iwork_ports::store::{StorageError, StoragePort};
use proptest::prelude::*;
use proptest::test_runner::{Config, TestRunner};
use reference_projection::{ReferenceProjection, apply_all};
use rusqlite::Connection;

use crate::fixtures::*;
use crate::sql::ColumnText;

struct Feed<'a> {
    store: &'a Store,
    next: Cell<u128>,
}

impl<'a> Feed<'a> {
    fn new(store: &'a Store) -> Self {
        Self {
            store,
            next: Cell::new(7000),
        }
    }
    fn commit(&self, events: Vec<DomainEvent>) -> Result<Seq, StorageError> {
        let n = self.next.get();
        self.next.set(n + 1);
        block(self.store.commit(cmd(n, events))).map(|r| r.last_seq)
    }
    fn ok(&self, events: Vec<DomainEvent>) -> Seq {
        self.commit(events).expect("提交")
    }
    /// 被拒，返回 `Projection` 的说明。
    fn rejected(&self, events: Vec<DomainEvent>) -> String {
        match self.commit(events) {
            Err(StorageError::Projection { message, .. }) => message,
            other => panic!("应被拒（Projection），实际 {other:?}"),
        }
    }
}

const INT_CRASH: TurnState = TurnState::Interrupted {
    reason: InterruptReason::Crash,
};

fn change(turn: u128, from: TurnState, to: TurnState) -> DomainEvent {
    TurnStateChanged {
        turn_id: tid(turn),
        from,
        to,
        reason: None,
    }
    .into()
}

fn intent(turn: u128, intent: TurnIntentKind) -> DomainEvent {
    TurnIntentRecorded {
        turn_id: tid(turn),
        intent,
    }
    .into()
}

fn queued(turn: u128, conv: u128, assistant: u128) -> DomainEvent {
    TurnQueued {
        turn: Turn {
            assistant_id: aid(assistant),
            ..crate::fixtures::turn(turn, conv)
        },
    }
    .into()
}

/// 项目 1、助手 1 与 2、项目对话 10、助手 2 的私聊 11。
fn world(feed: &Feed<'_>) {
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
    ]);
}

fn db_turn_state(env: &Env, turn: u128) -> TurnState {
    let conn =
        Connection::open_with_flags(env.db(), rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY).unwrap();
    let text: String = conn
        .query_row(
            "SELECT state FROM turns WHERE id = ?1",
            [tid(turn).to_string()],
            |r| r.get(0),
        )
        .unwrap();
    TurnState::from_text(&text).unwrap()
}

/// A4 ①②③、A3 在写线程上：合前提的落盘，不合的整笔拒、点名回合与两边的状态，什么都不留。
#[test]
fn turn_preconditions_on_writer() {
    let env = Env::new();
    let store = env.open();
    let feed = Feed::new(&store);
    world(&feed);
    feed.ok(vec![queued(20, 10, 1)]);

    // A4 ①：库里是 Queued，事件说 from Running → 拒，点名两边
    let text = feed.rejected(vec![change(20, TurnState::Running, TurnState::Completed)]);
    assert!(
        text.contains(&tid(20).to_string()) && text.contains("Running") && text.contains("Queued"),
        "{text}"
    );
    // from 对得上、但不是表里的边 → 拒
    let text = feed.rejected(vec![change(20, TurnState::Queued, TurnState::Completed)]);
    assert!(text.contains("TURN_TRANSITIONS"), "{text}");
    // A4 ③：继续意图落在 Queued 上 → 拒；A4 ②：取消意图同样拒
    feed.rejected(vec![intent(20, TurnIntentKind::Resume)]);
    feed.rejected(vec![intent(20, TurnIntentKind::Cancel)]);
    assert_eq!(db_turn_state(&env, 20), TurnState::Queued, "被拒的都没写");

    // 合前提的一串照常落盘
    feed.ok(vec![change(20, TurnState::Queued, TurnState::Running)]);
    feed.ok(vec![intent(20, TurnIntentKind::Cancel)]);
    feed.rejected(vec![intent(20, TurnIntentKind::Resume)]);
    feed.ok(vec![change(20, TurnState::Running, INT_CRASH)]);
    feed.ok(vec![intent(20, TurnIntentKind::Resume)]);
    feed.rejected(vec![intent(20, TurnIntentKind::Cancel)]);

    // 一笔里后一条不合前提：整笔回滚，前一条也没落
    let text = feed.rejected(vec![
        change(20, INT_CRASH, TurnState::Running),
        change(20, TurnState::Running, TurnState::Queued),
    ]);
    assert!(text.contains("TURN_TRANSITIONS"), "{text}");
    assert_eq!(db_turn_state(&env, 20), INT_CRASH);

    // 同一个回合两个写者：撤回与派发只放过先到的那个（G1）
    feed.ok(vec![queued(21, 10, 1)]);
    feed.ok(vec![change(21, TurnState::Queued, TurnState::Cancelled)]);
    feed.rejected(vec![change(21, TurnState::Queued, TurnState::Running)]);

    // A3：对话没有记录、在删、助手没有记录都排不进新回合，点名缺的那样
    let text = feed.rejected(vec![queued(30, 99, 1)]);
    assert!(text.contains("对话没有记录"), "{text}");
    feed.ok(vec![
        AssistantCreated {
            assistant: assistant(3, false),
        }
        .into(),
    ]);
    feed.ok(vec![
        AssistantDeleted {
            assistant_id: aid(3),
            name_snapshot: "a3".into(),
        }
        .into(),
    ]);
    let text = feed.rejected(vec![queued(31, 10, 3)]);
    assert!(text.contains("助手没有记录"), "{text}");
    feed.ok(vec![
        ConversationDeleting {
            conversation_id: cid(10),
        }
        .into(),
    ]);
    let text = feed.rejected(vec![queued(32, 10, 1)]);
    assert!(text.contains("对话正在删除"), "{text}");
    // 对照：别的对话照常排
    feed.ok(vec![queued(33, 11, 2)]);
}

/// X5：回合 Queued、Running、已中断时，删对话（`ConversationDeleted`）与连带删私聊的删助手
/// （`AssistantDeleted`）都被拒并点名；都到终态之后通过（对照）。
#[test]
fn cascade_rejected_while_turn_open() {
    // 每种没到终态的状态，和把它带到终态的一串事件
    let open_states: [(&str, Vec<TurnState>, Vec<(TurnState, TurnState)>); 3] = [
        (
            "Queued",
            vec![],
            vec![(TurnState::Queued, TurnState::Cancelled)],
        ),
        (
            "Running",
            vec![TurnState::Running],
            vec![
                (TurnState::Running, TurnState::Cancelling),
                (TurnState::Cancelling, TurnState::Cancelled),
            ],
        ),
        (
            "Interrupted",
            vec![TurnState::Running, INT_CRASH],
            vec![(INT_CRASH, TurnState::Cancelled)],
        ),
    ];
    let mut cases = 0;
    for (name, path, settle) in open_states {
        for via_assistant in [false, true] {
            let env = Env::new();
            let store = env.open();
            let feed = Feed::new(&store);
            world(&feed);
            // 删助手 2 会连带删它的私聊 11；删对话就删项目对话 10
            let (conv, assistant) = if via_assistant { (11, 2) } else { (10, 1) };
            feed.ok(vec![queued(20, conv, assistant)]);
            let mut at = TurnState::Queued;
            for to in path.iter().copied() {
                feed.ok(vec![change(20, at, to)]);
                at = to;
            }
            let delete: DomainEvent = if via_assistant {
                AssistantDeleted {
                    assistant_id: aid(2),
                    name_snapshot: "a2".into(),
                }
                .into()
            } else {
                ConversationDeleted {
                    conversation_id: cid(10),
                }
                .into()
            };
            let text = feed.rejected(vec![delete.clone()]);
            assert!(
                text.contains(&cid(conv).to_string()) && text.contains(&tid(20).to_string()),
                "{name}，删助手 {via_assistant}：{text}"
            );
            assert_eq!(db_turn_state(&env, 20), at, "回合的行还在");
            for (from, to) in settle.iter().copied() {
                feed.ok(vec![change(20, from, to)]);
            }
            feed.ok(vec![delete]);
            cases += 1;
        }
    }
    assert_eq!(cases, 6, "三种状态 × 两种事件");
}

// ---------- 对拍 ----------

#[derive(Debug, Clone)]
enum Op {
    Queue {
        turn: u128,
        conv: u128,
        assistant: u128,
    },
    Change {
        turn: u128,
        from: TurnState,
        to: TurnState,
    },
    Intent {
        turn: u128,
        kind: TurnIntentKind,
    },
    Deleting {
        conv: u128,
    },
    Delete {
        conv: u128,
    },
    DeleteAssistant {
        assistant: u128,
    },
}

fn turn_state() -> impl Strategy<Value = TurnState> {
    prop::sample::select(TurnState::all())
}

fn op() -> impl Strategy<Value = Op> {
    let turn = 20u128..24;
    prop_oneof![
        2 => (turn.clone(), prop::sample::select(vec![(10u128, 1u128), (11, 2), (10, 2)]))
            .prop_map(|(turn, (conv, assistant))| Op::Queue { turn, conv, assistant }),
        6 => (turn.clone(), turn_state(), turn_state())
            .prop_map(|(turn, from, to)| Op::Change { turn, from, to }),
        2 => (turn, prop::sample::select(vec![TurnIntentKind::Cancel, TurnIntentKind::Resume]))
            .prop_map(|(turn, kind)| Op::Intent { turn, kind }),
        1 => (10u128..12).prop_map(|conv| Op::Deleting { conv }),
        1 => (10u128..12).prop_map(|conv| Op::Delete { conv }),
        1 => Just(Op::DeleteAssistant { assistant: 2 }),
    ]
}

impl Op {
    /// 参照投影只管提交时校验，不像 SQLite 那样对「删一个不存在的对象」「重复建同一个回合」报错；
    /// 这几种不在对拍范围里，跳过。
    fn applicable(&self, r: &ReferenceProjection) -> bool {
        match self {
            Op::Queue { turn, .. } => !r.turns.contains_key(&tid(*turn)),
            Op::Delete { conv } | Op::Deleting { conv } => {
                r.conversations.contains_key(&cid(*conv))
            }
            Op::DeleteAssistant { assistant } => r.assistants.contains(&aid(*assistant)),
            Op::Change { .. } | Op::Intent { .. } => true,
        }
    }

    fn event(&self) -> DomainEvent {
        match *self {
            Op::Queue {
                turn,
                conv,
                assistant,
            } => queued(turn, conv, assistant),
            Op::Change { turn, from, to } => change(turn, from, to),
            Op::Intent { turn, kind } => intent(turn, kind),
            Op::Deleting { conv } => ConversationDeleting {
                conversation_id: cid(conv),
            }
            .into(),
            Op::Delete { conv } => ConversationDeleted {
                conversation_id: cid(conv),
            }
            .into(),
            Op::DeleteAssistant { assistant } => AssistantDeleted {
                assistant_id: aid(assistant),
                name_snapshot: "a".into(),
            }
            .into(),
        }
    }
}

fn db_turn_states(env: &Env) -> BTreeMap<TurnId, TurnState> {
    let conn =
        Connection::open_with_flags(env.db(), rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY).unwrap();
    let mut stmt = conn.prepare("SELECT id, state FROM turns").unwrap();
    stmt.query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)))
        .unwrap()
        .map(|row| {
            let (id, state) = row.unwrap();
            (id.parse().unwrap(), TurnState::from_text(&state).unwrap())
        })
        .collect()
}

/// 同一串命令喂 SQLite 与参照投影：每一笔两边要么都收、要么都拒；最后各回合的状态相等。
/// 真存储与 iwork-core 的假存储共用 domain 的判断函数，这条用例守的是两边取事实的读法一致。
#[test]
fn precondition_parity() {
    let accepted = Cell::new(0usize);
    let rejected = Cell::new(0usize);
    let mut runner = TestRunner::new(Config {
        cases: 48,
        ..Config::default()
    });
    runner
        .run(&prop::collection::vec(op(), 1..16), |ops| {
            let env = Env::new();
            let store = env.open();
            let feed = Feed::new(&store);
            world(&feed);
            let mut reference = ReferenceProjection::default();
            apply_all(&mut reference, &world_events()).unwrap();
            for (i, op) in ops.iter().enumerate() {
                if !op.applicable(&reference) {
                    continue;
                }
                let event = op.event();
                let by_reference = apply_all(&mut reference, std::slice::from_ref(&event));
                let by_sqlite = feed.commit(vec![event]);
                match (&by_reference, &by_sqlite) {
                    (Ok(()), Ok(_)) => accepted.set(accepted.get() + 1),
                    (Err(_), Err(StorageError::Projection { .. })) => {
                        rejected.set(rejected.get() + 1)
                    }
                    _ => {
                        return Err(TestCaseError::fail(format!(
                            "第 {i} 笔 {op:?}：参照 {by_reference:?}，SQLite {by_sqlite:?}"
                        )));
                    }
                }
            }
            prop_assert_eq!(db_turn_states(&env), reference.turn_states.clone());
            Ok(())
        })
        .unwrap();
    // 对照：两种结果都真的出现过，比较不是空的
    assert!(accepted.get() > 50, "收下 {}", accepted.get());
    assert!(rejected.get() > 50, "拒掉 {}", rejected.get());
}

/// `world` 里那一笔，给参照投影用。
fn world_events() -> Vec<DomainEvent> {
    vec![
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
    ]
}
