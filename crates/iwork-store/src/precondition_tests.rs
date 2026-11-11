//! 提交时校验（方案 2.6「状态前提」「归属存续」，第 11 节第 4a 步的六条）。放在 crate 内部：
//! 对拍要用写线程那一份读法（`projector::owner::owner_facts_on`）在只读连接上取归属事实。

#[path = "../../iwork-ports/tests/reference_projection/mod.rs"]
mod reference_projection;

use std::cell::Cell;

use iwork_app::codec::ProtoEventCodec;
use iwork_domain::approval::{
    ApprovalState, ConversationPresence, Decider, OwnerFacts, Presence, ReviewState, VoidReason,
};
use iwork_domain::approval_mode::RememberScope;
use iwork_domain::event::*;
use iwork_domain::id::{ReviewId, Seq};
use iwork_domain::rule::{RuleLayer, RuleScope};
use iwork_domain::turn::TurnState;
use iwork_domain::user_action::{ExitStatus, UserActionState};
use iwork_ports::store::{EventCodec, StorageError, StoragePort};
use reference_projection::{ReferenceProjection, apply_all};
use rusqlite::Connection;

use crate::fixtures::*;
use crate::projector::owner::owner_facts_on;

struct Feed<'a> {
    store: &'a Store,
    next: Cell<u128>,
}

impl<'a> Feed<'a> {
    fn new(store: &'a Store) -> Self {
        Self {
            store,
            next: Cell::new(5000),
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
}

fn is_projection(r: &Result<Seq, StorageError>) -> bool {
    matches!(r, Err(StorageError::Projection { .. }))
}

fn projection_text(r: Result<Seq, StorageError>) -> String {
    match r {
        Err(StorageError::Projection { message, .. }) => message,
        other => panic!("应是 Projection，实际 {other:?}"),
    }
}

/// 助手 1、项目 1、项目对话 10、回合 20（助手 1）。
fn owned_base(feed: &Feed<'_>) {
    feed.ok(vec![
        ProjectAdded {
            project: project(1),
        }
        .into(),
        AssistantCreated {
            assistant: assistant(1, false),
        }
        .into(),
        ConversationCreated {
            conversation: project_conversation(10, 1),
        }
        .into(),
        TurnQueued { turn: turn(20, 10) }.into(),
    ]);
}

fn requested(n: u128) -> DomainEvent {
    ApprovalRequested {
        approval: approval(n, 20, 10),
    }
    .into()
}

fn decided(n: u128, state: ApprovalState) -> DomainEvent {
    ApprovalDecided {
        approval_id: apid(n),
        state,
        remember: None,
    }
    .into()
}

fn voided(n: u128) -> DomainEvent {
    ApprovalVoided {
        approval_id: apid(n),
        reason: VoidReason::TurnCancelled,
    }
    .into()
}

fn proposed(n: u128) -> DomainEvent {
    UserActionProposed {
        card: card(n, 20, 10),
    }
    .into()
}

fn card_to(n: u128, state: UserActionState) -> DomainEvent {
    UserActionStateChanged {
        card_id: uaid(n),
        state,
        output_artifact: None,
    }
    .into()
}

/// 把审批 n 推到终态的一条事件。
type Finish = fn(u128) -> DomainEvent;

const APPROVED: ApprovalState = ApprovalState::Approved {
    decider: Decider::User,
};

/// 两张转换表在写线程上的样子：表里的边放行，表外的拒绝。期望值手写。
#[test]
fn precondition_matches_domain_table() {
    let env = Env::new();
    let store = env.open();
    let feed = Feed::new(&store);
    owned_base(&feed);
    // 审批主状态：只有 Pending 能接批准、拒绝、作废
    let finishers: [(&str, Finish); 3] = [
        ("批准", |n| decided(n, APPROVED)),
        ("拒绝", |n| decided(n, ApprovalState::Denied)),
        ("作废", voided),
    ];
    let mut n = 100;
    for (from_name, to_from) in [
        ("Pending", None),
        ("Approved", Some(0)),
        ("Denied", Some(1)),
        ("Voided", Some(2)),
    ] {
        for (name, finish) in finishers {
            n += 1;
            feed.ok(vec![requested(n)]);
            if let Some(i) = to_from {
                feed.ok(vec![finishers[i].1(n)]);
            }
            let r = feed.commit(vec![finish(n)]);
            assert_eq!(
                r.is_ok(),
                from_name == "Pending",
                "{from_name} ← {name}：{r:?}"
            );
            if from_name != "Pending" {
                assert!(is_projection(&r));
            }
        }
    }
    // 审查子状态：开始、同一次的结论放行；review_id 不同、表外的边拒绝
    n += 1;
    feed.ok(vec![requested(n)]);
    let rid = ReviewId(tid(900));
    let review = |r: ReviewState| -> DomainEvent {
        ApprovalReviewUpdated {
            approval_id: apid(n),
            review: r,
        }
        .into()
    };
    assert!(is_projection(
        &feed.commit(vec![review(ReviewState::Errored { review_id: rid })])
    ));
    feed.ok(vec![review(ReviewState::InProgress { review_id: rid })]);
    assert!(is_projection(&feed.commit(vec![review(
        ReviewState::Errored {
            review_id: ReviewId(tid(901))
        }
    )])));
    feed.ok(vec![review(ReviewState::Errored { review_id: rid })]);
    assert!(is_projection(
        &feed.commit(vec![review(ReviewState::TimedOut { review_id: rid })])
    ));

    // 卡片：每个起点状态 × 每个目标状态，手写允许的对
    use UserActionState as S;
    let states = || {
        vec![
            S::Pending,
            S::Starting,
            S::Executing,
            S::Stopping,
            S::Done {
                exit: ExitStatus::Code(0),
            },
            S::Failed { error: "e".into() },
            S::Unknown { error: "u".into() },
            S::Ended,
            S::Dismissed,
            S::Voided {
                reason: VoidReason::TurnCancelled,
            },
        ]
    };
    // 到每个起点状态的合法路径
    let path_to = |s: &S| -> Vec<S> {
        match s {
            S::Pending => vec![],
            S::Starting => vec![S::Starting],
            S::Executing => vec![S::Starting, S::Executing],
            S::Stopping => vec![S::Starting, S::Executing, S::Stopping],
            S::Done { .. } => vec![S::Starting, S::Executing, s.clone()],
            S::Failed { .. } => vec![S::Starting, s.clone()],
            S::Unknown { .. } => vec![S::Starting, s.clone()],
            S::Ended => vec![S::Starting, S::Unknown { error: "u".into() }, S::Ended],
            S::Dismissed | S::Voided { .. } => vec![s.clone()],
        }
    };
    let allowed: &[(&str, &str)] = &[
        ("Pending", "Starting"),
        ("Pending", "Dismissed"),
        ("Pending", "Voided"),
        ("Starting", "Executing"),
        ("Starting", "Failed"),
        ("Starting", "Unknown"),
        ("Executing", "Done"),
        ("Executing", "Stopping"),
        ("Executing", "Unknown"),
        ("Stopping", "Done"),
        ("Stopping", "Unknown"),
        ("Unknown", "Ended"),
    ];
    let mut card_n = 300;
    let mut passed = 0;
    for from in states() {
        for to in states() {
            card_n += 1;
            let mut setup = vec![proposed(card_n)];
            setup.extend(path_to(&from).into_iter().map(|s| card_to(card_n, s)));
            feed.ok(setup);
            let (f, t) = (format!("{:?}", from.kind()), format!("{:?}", to.kind()));
            let want = allowed.contains(&(f.as_str(), t.as_str()));
            let r = feed.commit(vec![card_to(card_n, to.clone())]);
            assert_eq!(r.is_ok(), want, "卡片 {f} → {t}：{r:?}");
            passed += usize::from(r.is_ok());
        }
    }
    assert_eq!(passed, allowed.len(), "对照：放行的恰好是手写的那些");
}

/// 一条命令里有一条不通过：整条回滚，同一笔的放行规则也没落；seq 不留洞。
#[test]
fn precondition_failure_rolls_back_whole_command() {
    let env = Env::new();
    let store = env.open();
    let feed = Feed::new(&store);
    owned_base(&feed);
    feed.ok(vec![requested(1)]);
    feed.ok(vec![voided(1)]);
    let head = block(store.head_seq()).unwrap();
    let rules = |env: &Env| scalar(&env.db(), "SELECT COUNT(*) FROM approval_rules");
    let before = rules(&env);
    let r = feed.commit(vec![
        ApprovalDecided {
            approval_id: apid(1),
            state: APPROVED,
            remember: Some(RememberScope::Project),
        }
        .into(),
        RuleRemembered {
            rule: rule(1, RuleLayer::Remembered, RuleScope::Global),
        }
        .into(),
    ]);
    assert!(is_projection(&r), "{r:?}");
    assert_eq!(rules(&env), before, "放行规则一起回滚");
    assert_eq!(block(store.head_seq()).unwrap(), head);
    // 下一笔紧接着 head，不留洞
    let next = feed.ok(vec![requested(2)]);
    assert_eq!(next, Seq(head.0 + 1));
}

/// 绕过 commit 往事件表塞「作废之后又批准」：重建报 Projection 并点名 seq，旧投影不变。
#[test]
fn rebuild_rejects_illegal_history() {
    let env = Env::new();
    let store = env.open();
    let feed = Feed::new(&store);
    owned_base(&feed);
    feed.ok(vec![requested(1)]);
    let head = feed.ok(vec![voided(1)]);
    let illegal = Seq(head.0 + 1);
    {
        let conn = Connection::open(env.db()).unwrap();
        let encoded = ProtoEventCodec.encode(&decided(1, APPROVED)).unwrap();
        let (kind, id) = decided(1, APPROVED).stream();
        conn.execute(
            "INSERT INTO events(seq, stream_kind, stream_id, type, payload, command_id, actor, at) \
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, 'test', 0)",
            rusqlite::params![
                illegal.0 as i64,
                crate::sql::ColumnText::to_text(kind),
                id.map(|s| s.0),
                encoded.kind,
                encoded.payload,
                iwork_domain::id::CommandId::from_raw(iwork_domain::id::RawUlid::from_u128(0xBAD))
                    .to_string(),
            ],
        )
        .unwrap();
    }
    let state = |env: &Env| -> String {
        Connection::open(env.db())
            .unwrap()
            .query_row(
                "SELECT state FROM approvals WHERE id = ?1",
                [apid(1).to_string()],
                |r| r.get(0),
            )
            .unwrap()
    };
    let before = state(&env);
    match block(store.rebuild_projections()) {
        Err(StorageError::Projection { seq, .. }) => assert_eq!(seq, illegal),
        other => panic!("应报 Projection，实际 {other:?}"),
    }
    assert_eq!(state(&env), before, "旧投影不变");
}

/// 别的实体的状态事件照旧不校验：回合、对话的「现在 → 新值」随便给都写入。
#[test]
fn precondition_only_for_approvals_and_cards() {
    let env = Env::new();
    let store = env.open();
    let feed = Feed::new(&store);
    owned_base(&feed);
    // 回合从 Queued 直接到 Completed，再回 Running：领域状态机不允许，投影器照写
    for (from, to) in [
        (TurnState::Queued, TurnState::Completed),
        (TurnState::Completed, TurnState::Running),
    ] {
        feed.ok(vec![
            TurnStateChanged {
                turn_id: tid(20),
                from,
                to,
                reason: None,
            }
            .into(),
        ]);
    }
    // 对话进删除中两次也照写
    for _ in 0..2 {
        feed.ok(vec![
            ConversationDeleting {
                conversation_id: cid(10),
            }
            .into(),
        ]);
    }
}

fn facts(store_db: &std::path::Path, turn: u128) -> OwnerFacts {
    let conn =
        Connection::open_with_flags(store_db, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY).unwrap();
    owner_facts_on(&conn, &ProtoEventCodec, tid(turn)).unwrap()
}

/// 同一串事件喂 SQLite 与参照投影，每个前缀上读出的归属事实都相等。
#[test]
fn owner_facts_sqlite_matches_reference() {
    let env = Env::new();
    let store = env.open();
    let feed = Feed::new(&store);
    let mut reference = ReferenceProjection::default();
    let steps: Vec<Vec<DomainEvent>> = vec![
        vec![
            ProjectAdded {
                project: project(1),
            }
            .into(),
        ],
        vec![
            AssistantCreated {
                assistant: assistant(1, false),
            }
            .into(),
        ],
        vec![
            AssistantCreated {
                assistant: assistant(2, false),
            }
            .into(),
        ],
        vec![
            ConversationCreated {
                conversation: project_conversation(10, 1),
            }
            .into(),
        ],
        vec![
            ConversationCreated {
                conversation: project_conversation(11, 1),
            }
            .into(),
        ],
        vec![
            ConversationCreated {
                conversation: dm(12, 2),
            }
            .into(),
        ],
        vec![TurnQueued { turn: turn(20, 10) }.into()],
        vec![TurnQueued { turn: turn(21, 11) }.into()],
        vec![
            TurnQueued {
                turn: iwork_domain::turn::Turn {
                    assistant_id: aid(2),
                    ..turn(22, 12)
                },
            }
            .into(),
        ],
        vec![
            TurnQueued {
                turn: iwork_domain::turn::Turn {
                    assistant_id: aid(2),
                    ..turn(23, 10)
                },
            }
            .into(),
        ],
        // 回合结束：行还在
        vec![
            TurnFinished {
                turn_id: tid(20),
                state: TurnState::Completed,
                usage: Default::default(),
                celebrate: false,
                change_summary: None,
            }
            .into(),
        ],
        // 对话删除中
        vec![
            ConversationDeleting {
                conversation_id: cid(11),
            }
            .into(),
        ],
        // 助手 2 删除：私聊 12 连带回合 22 一起没了；回合 23 在项目对话里，行还在、助手没了
        vec![
            AssistantDeleted {
                assistant_id: aid(2),
                name_snapshot: "a2".into(),
            }
            .into(),
        ],
        // 对话整条删掉：回合 21 级联没了
        vec![
            ConversationDeleted {
                conversation_id: cid(11),
            }
            .into(),
        ],
    ];
    let turns = [20u128, 21, 22, 23, 99];
    for (i, events) in steps.into_iter().enumerate() {
        apply_all(&mut reference, &events).unwrap();
        feed.ok(events);
        for t in turns {
            assert_eq!(
                facts(&env.db(), t),
                reference.owner_facts(tid(t)),
                "第 {i} 步之后回合 {t}"
            );
        }
    }
    // 对照：最后的事实确实各不相同，比较不是空的
    let present = OwnerFacts {
        turn: Presence::Present,
        conversation: ConversationPresence::Present,
        assistant: Presence::Present,
    };
    assert_eq!(facts(&env.db(), 20), present);
    assert_eq!(facts(&env.db(), 21).turn, Presence::Gone);
    assert_eq!(facts(&env.db(), 22).turn, Presence::Gone);
    assert_eq!(facts(&env.db(), 23).assistant, Presence::Gone);
}

/// 归属存续：四种受管事件 × 三样各缺一样 / 对话删除中都拒并点名；回合结束了照样通过；
/// 拒绝、作废、关闭不查。助手删除与另一个对话的审批按两种顺序提交。
#[test]
fn owner_alive_precondition() {
    let env = Env::new();
    let store = env.open();
    let feed = Feed::new(&store);
    owned_base(&feed);
    feed.ok(vec![
        AssistantCreated {
            assistant: assistant(2, false),
        }
        .into(),
        ConversationCreated {
            conversation: project_conversation(11, 1),
        }
        .into(),
        ConversationCreated {
            conversation: project_conversation(12, 1),
        }
        .into(),
    ]);
    // 回合结束了：照样能建、能批
    feed.ok(vec![
        TurnFinished {
            turn_id: tid(20),
            state: TurnState::Completed,
            usage: Default::default(),
            celebrate: false,
            change_summary: None,
        }
        .into(),
    ]);
    feed.ok(vec![requested(1), proposed(1)]);
    feed.ok(vec![
        decided(1, APPROVED),
        card_to(1, UserActionState::Starting),
    ]);

    // 回合不存在
    let r = feed.commit(vec![
        ApprovalRequested {
            approval: approval(2, 77, 10),
        }
        .into(),
    ]);
    assert!(projection_text(r).contains("回合没有记录"));

    // 对话删除中：在对话 11 起回合 21、先建好审批与卡片，再进删除中
    feed.ok(vec![TurnQueued { turn: turn(21, 11) }.into()]);
    feed.ok(vec![
        ApprovalRequested {
            approval: approval(3, 21, 11),
        }
        .into(),
        UserActionProposed {
            card: card(3, 21, 11),
        }
        .into(),
        ApprovalRequested {
            approval: approval(4, 21, 11),
        }
        .into(),
        UserActionProposed {
            card: card(4, 21, 11),
        }
        .into(),
    ]);
    feed.ok(vec![
        ConversationDeleting {
            conversation_id: cid(11),
        }
        .into(),
    ]);
    for (what, events) in [
        (
            "建审批",
            vec![
                ApprovalRequested {
                    approval: approval(5, 21, 11),
                }
                .into(),
            ],
        ),
        (
            "建卡片",
            vec![
                UserActionProposed {
                    card: card(5, 21, 11),
                }
                .into(),
            ],
        ),
        ("批准", vec![decided(3, APPROVED)]),
        ("卡片启动", vec![card_to(3, UserActionState::Starting)]),
    ] {
        let text = projection_text(feed.commit(events));
        assert!(text.contains("对话正在删除"), "{what}：{text}");
    }
    // 拒绝、作废、关闭照样写入（孤儿清得掉）
    feed.ok(vec![decided(3, ApprovalState::Denied), voided(4)]);
    feed.ok(vec![card_to(3, UserActionState::Dismissed)]);
    feed.ok(vec![card_to(
        4,
        UserActionState::Voided {
            reason: VoidReason::ConversationDeleted,
        },
    )]);

    // 助手没了：项目对话 12 里起一个助手 2 的回合，建好审批后删助手
    feed.ok(vec![
        TurnQueued {
            turn: iwork_domain::turn::Turn {
                assistant_id: aid(2),
                ..turn(22, 12)
            },
        }
        .into(),
    ]);
    // 顺序一：审批先落盘，删除后落盘 → 审批留下但批不了
    feed.ok(vec![
        ApprovalRequested {
            approval: approval(6, 22, 12),
        }
        .into(),
    ]);
    feed.ok(vec![
        AssistantDeleted {
            assistant_id: aid(2),
            name_snapshot: "a2".into(),
        }
        .into(),
    ]);
    let text = projection_text(feed.commit(vec![decided(6, APPROVED)]));
    assert!(text.contains("助手没有记录"), "{text}");
    // 顺序二：删除先落盘，审批后到 → 被拒
    let text = projection_text(feed.commit(vec![ApprovalRequested {
        approval: approval(7, 22, 12),
    }
    .into()]));
    assert!(text.contains("助手没有记录"), "{text}");
    // 孤儿能被作废
    feed.ok(vec![
        ApprovalVoided {
            approval_id: apid(6),
            reason: VoidReason::AssistantDeleted,
        }
        .into(),
    ]);

    // 对话整条没了：回合行随之级联，再建就是回合没有记录
    feed.ok(vec![
        ConversationDeleted {
            conversation_id: cid(11),
        }
        .into(),
    ]);
    let text = projection_text(feed.commit(vec![ApprovalRequested {
        approval: approval(8, 21, 11),
    }
    .into()]));
    assert!(text.contains("回合没有记录"), "{text}");
}
