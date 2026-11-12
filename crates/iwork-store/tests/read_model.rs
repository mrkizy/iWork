//! 方案第 4b 步：`ReadModelPort` 的查询；S10：流式检查点。

mod support;

use iwork_domain::approval::ReviewState;
use iwork_domain::attachment::AttachmentOrigin;
use iwork_domain::common::{ArtifactArea, Timestamp};
use iwork_domain::event::*;
use iwork_domain::id::ReviewId;
use iwork_domain::message::{Author, Message};
use iwork_domain::message::{MessageBody, MessageStatus};
use iwork_domain::rule::{RuleLayer, RuleScope};
use iwork_domain::runtime::QuotaWindow;
use iwork_domain::turn::{InterruptReason, TurnState};
use iwork_domain::user_action::UserActionState;
use iwork_ports::read_model::{
    MessagePage, PendingItem, ReadModelPort, SummaryLine, SummarySource, SummaryText, SummaryWho,
};
use iwork_ports::store::CommitRequest;
use iwork_ports::store::StoragePort;
use support::*;

fn setup() -> (Env, Store) {
    let env = Env::new();
    let store = env.open();
    block(store.commit(cmd(
        1,
        vec![
            ProjectAdded { project: project(1) }.into(),
            ProjectAdded { project: project(2) }.into(),
            // 回合的助手：审批与卡片写入前查归属存续（T02 方案 2.6）
            AssistantCreated { assistant: assistant(1, false) }.into(),
            ConversationCreated { conversation: project_conversation(10, 1) }.into(),
            TurnQueued { turn: turn(20, 10) }.into(),
        ],
    )))
    .unwrap();
    (env, store)
}

fn page(before: Option<u128>, limit: u32) -> MessagePage {
    MessagePage {
        before: before.map(mid),
        limit,
    }
}

#[test]
fn conversation_page_keyset() {
    let (_env, store) = setup();
    let events = (1..=7u128)
        .map(|n| {
            MessageAppended {
                message: user_message(n, 10, &format!("m{n}")),
            }
            .into()
        })
        .collect();
    block(store.commit(cmd(2, events))).unwrap();
    let rm = read_model(&store);
    let ids = |v: &[iwork_domain::message::Message]| v.iter().map(|m| m.id).collect::<Vec<_>>();

    let latest = block(rm.conversation(cid(10), page(None, 3))).unwrap();
    assert_eq!(
        ids(&latest.messages),
        vec![mid(5), mid(6), mid(7)],
        "最新一页，升序"
    );
    let older = block(rm.conversation(cid(10), page(Some(5), 3))).unwrap();
    assert_eq!(ids(&older.messages), vec![mid(2), mid(3), mid(4)]);
    let oldest = block(rm.conversation(cid(10), page(Some(2), 3))).unwrap();
    assert_eq!(ids(&oldest.messages), vec![mid(1)]);
    let none = block(rm.conversation(cid(10), page(Some(1), 3))).unwrap();
    assert!(none.messages.is_empty());
    assert!(matches!(
        block(rm.conversation(cid(99), page(None, 3))),
        Err(iwork_ports::store::StorageError::NotFound {
            entity: "conversation",
            ..
        })
    ));
}

#[test]
fn conversation_view_includes_page_turns_and_unfinished_turns() {
    let (_env, store) = setup();
    let mut done = turn(21, 10);
    done.state = TurnState::Completed;
    let mut old = turn(22, 10);
    old.state = TurnState::Failed;
    block(store.commit(cmd(
        2,
        vec![
            TurnQueued { turn: done }.into(),
            TurnQueued { turn: old }.into(),
            MessageAppended { message: assistant_message(30, 10, 21) }.into(),
        ],
    )))
    .unwrap();
    let view = block(read_model(&store).conversation(cid(10), page(None, 10))).unwrap();
    let turns: Vec<_> = view.turns.iter().map(|t| t.id).collect();
    assert_eq!(
        turns,
        vec![tid(20), tid(21)],
        "没结束的 20 + 本页消息涉及的 21；已结束且无关的 22 不带"
    );
}

#[test]
fn streaming_message_body_from_checkpoint() {
    let (env, store) = setup();
    block(store.commit(cmd(
        2,
        vec![MessageAppended { message: assistant_message(30, 10, 20) }.into()],
    )))
    .unwrap();
    let rm = read_model(&store);
    let body = |rm: &iwork_store::SqliteReadModel<_>| {
        block(rm.conversation(cid(10), page(None, 10)))
            .unwrap()
            .messages[0]
            .body
            .clone()
    };
    assert_eq!(
        body(&rm),
        MessageBody::Inline(String::new()),
        "还没有检查点：空串"
    );
    block(store.checkpoint_message(mid(30), 5, "hello")).unwrap();
    block(store.checkpoint_message(mid(30), 11, "hello world")).unwrap();
    assert_eq!(
        body(&rm),
        MessageBody::Inline("hello world".into()),
        "第二次的正文"
    );

    // 重建不清检查点
    block(store.rebuild_projections()).unwrap();
    assert_eq!(body(&rm), MessageBody::Inline("hello world".into()));

    block(store.commit(cmd(
        3,
        vec![
            MessageCompleted {
                conversation_id: cid(10),
                message_id: mid(30),
                body: MessageBody::Inline("final".into()),
                status: MessageStatus::Completed,
            }
            .into(),
        ],
    )))
    .unwrap();
    assert_eq!(
        body(&rm),
        MessageBody::Inline("final".into()),
        "结束后用事件里的正文"
    );
    assert_eq!(
        scalar(&env.db(), "SELECT COUNT(*) FROM message_checkpoints"),
        0,
        "检查点行删掉"
    );
}

#[test]
fn pending_items_four_sources() {
    let (_env, store) = setup();
    let mut reviewing = approval(41, 20, 10);
    reviewing.review = ReviewState::InProgress {
        review_id: ReviewId(tid(90)),
    };
    let mut done_card = card(52, 20, 10);
    done_card.state = UserActionState::Ended;
    let mut unknown_card = card(51, 20, 10);
    unknown_card.state = UserActionState::Unknown {
        error: "lost".into(),
    };
    let mut interrupted = turn(23, 10);
    interrupted.state = TurnState::Interrupted {
        reason: InterruptReason::ApprovalWaitLimit,
    };
    block(store.commit(cmd(
        2,
        vec![
            ApprovalRequested { approval: approval(40, 20, 10) }.into(),
            ApprovalRequested { approval: reviewing }.into(),
            UserActionProposed { card: card(50, 20, 10) }.into(),
            UserActionProposed { card: unknown_card.clone() }.into(),
            UserActionProposed { card: done_card }.into(),
            AgentsMdDraftProposed {
                card_id: jid(60),
                project_id: pid(1),
                content_artifact: artifact(1, ArtifactArea::Artifacts),
            }
            .into(),
            AgentsMdDraftProposed {
                card_id: jid(61),
                project_id: pid(1),
                content_artifact: artifact(2, ArtifactArea::Artifacts),
            }
            .into(),
            AgentsMdDraftDecided {
                card_id: jid(61),
                project_id: pid(1),
                decision: DraftDecision::Discard,
            }
            .into(),
            TurnQueued { turn: interrupted }.into(),
        ],
    )))
    .unwrap();
    let items = block(read_model(&store).pending_items()).unwrap();
    let kinds: Vec<String> = items
        .iter()
        .map(|i| match i {
            PendingItem::Approval(a) => format!("approval {}", a.id),
            PendingItem::UserAction(c) => format!("card {}", c.id),
            PendingItem::AgentsMdDraft(d) => format!("draft {}", d.card_id),
            PendingItem::Resume(r) => format!("resume {} {:?}", r.turn_id, r.reason),
        })
        .collect();
    assert_eq!(
        kinds,
        vec![
            format!("approval {}", apid(40)),
            format!("card {}", uaid(50)),
            format!("card {}", uaid(51)),
            format!("draft {}", jid(60)),
            format!("resume {} ApprovalWaitLimit", tid(23)),
        ],
        "审查中的审批、已结束的卡片、已处理的草稿都不算"
    );
    let PendingItem::AgentsMdDraft(d) = &items[3] else {
        unreachable!()
    };
    assert_eq!(d.content, artifact(1, ArtifactArea::Artifacts));
    assert_eq!(d.proposed_at, Timestamp(T0));
}

#[test]
fn rules_scope_filter_and_security_lists_three_layers() {
    let (_env, store) = setup();
    let rules = [
        rule(1, RuleLayer::Remembered, RuleScope::Global),
        rule(2, RuleLayer::Remembered, RuleScope::Project(pid(1))),
        rule(3, RuleLayer::Remembered, RuleScope::Project(pid(2))),
        rule(4, RuleLayer::Remembered, RuleScope::Conversation(cid(10))),
        rule(5, RuleLayer::CredentialCommands, RuleScope::Global),
        rule(6, RuleLayer::SensitiveDirs, RuleScope::Global),
        rule(7, RuleLayer::WritableDirs, RuleScope::Project(pid(1))),
        rule(8, RuleLayer::RejectList, RuleScope::Global),
    ];
    let mut events: Vec<DomainEvent> = rules[..4]
        .iter()
        .map(|r| RuleRemembered { rule: r.clone() }.into())
        .collect();
    events.extend(
        rules[4..]
            .iter()
            .map(|r| SecurityListEntryAdded { rule: r.clone() }.into()),
    );
    block(store.commit(cmd(2, events))).unwrap();
    let rm = read_model(&store);
    let ids =
        |v: Vec<iwork_domain::rule::RuleEntry>| v.into_iter().map(|r| r.id).collect::<Vec<_>>();
    assert_eq!(block(rm.rules(None)).map(ids).unwrap().len(), 8);
    assert_eq!(
        block(rm.rules(Some(pid(1)))).map(ids).unwrap(),
        vec![rid(1), rid(2), rid(5), rid(6), rid(7), rid(8)],
        "全局 + 本项目"
    );
    assert_eq!(
        block(rm.security_lists()).map(ids).unwrap(),
        vec![rid(5), rid(6), rid(7)]
    );

    block(store.commit(cmd(
        3,
        vec![
            RuleRevoked { rule_id: rid(1) }.into(),
            SecurityListEntryRemoved { entry_id: rid(5) }.into(),
            RulesDeleted {
                rule_ids: vec![rid(4)],
                cause: RulesDeletedCause::ConversationDeleted,
            }
            .into(),
        ],
    )))
    .unwrap();
    assert_eq!(
        block(rm.rules(None)).map(ids).unwrap(),
        vec![rid(2), rid(3), rid(6), rid(7), rid(8)]
    );
}

#[test]
fn settings_defaults_when_no_row_and_fill_null_columns() {
    let (_env, store) = setup();
    let rm = read_model(&store);
    let defaults = block(rm.settings()).unwrap();
    assert!(defaults.playful_ui && !defaults.autostart);
    assert_eq!(
        defaults.projects_root.to_str(),
        Some("/home/demo/iWork/projects")
    );

    block(store.commit(cmd(
        2,
        vec![
            GlobalSettingsChanged {
                playful_ui: None,
                autostart: Some(true),
                projects_root: None,
            }
            .into(),
        ],
    )))
    .unwrap();
    block(store.commit(cmd(
        3,
        vec![
            GlobalSettingsChanged {
                playful_ui: None,
                autostart: None,
                projects_root: Some("/data/p".into()),
            }
            .into(),
        ],
    )))
    .unwrap();
    let s = block(rm.settings()).unwrap();
    assert!(s.playful_ui, "没设过：默认值");
    assert!(s.autostart, "前一次设的值保留");
    assert_eq!(s.projects_root.to_str(), Some("/data/p"));
}

#[test]
fn artifact_is_referenced() {
    let (_env, store) = setup();
    let a = artifact(1, ArtifactArea::Artifacts);
    let rm = read_model(&store);
    assert!(!block(rm.artifact_is_referenced(&a)).unwrap());
    let mut m = user_message(1, 10, "");
    m.body = MessageBody::Artifact(a);
    block(store.commit(cmd(2, vec![MessageAppended { message: m }.into()]))).unwrap();
    assert!(block(rm.artifact_is_referenced(&a)).unwrap());
    let other_area = artifact(1, ArtifactArea::Attachments);
    assert!(
        !block(rm.artifact_is_referenced(&other_area)).unwrap(),
        "区不同不算"
    );
}

#[test]
fn quota_keeps_latest_per_window_and_groups_by_runtime() {
    let (_env, store) = setup();
    let w = |name: &str, pct: u8, at: i64| QuotaWindow {
        window: name.into(),
        used_pct: pct,
        resets_at: None,
        observed_at: Timestamp(at),
    };
    block(store.commit(cmd(
        2,
        vec![
            QuotaObserved {
                runtime_id: rtid(1),
                windows: vec![w("5h", 10, 1), w("week", 20, 1)],
                observed_at: Timestamp(1),
            }
            .into(),
            QuotaObserved {
                runtime_id: rtid(1),
                windows: vec![w("5h", 30, 2)],
                observed_at: Timestamp(2),
            }
            .into(),
            QuotaObserved {
                runtime_id: rtid(2),
                windows: vec![w("5h", 5, 3)],
                observed_at: Timestamp(3),
            }
            .into(),
        ],
    )))
    .unwrap();
    let q = block(read_model(&store).quota()).unwrap();
    assert_eq!(q.len(), 2);
    assert_eq!(q[0].runtime_id, rtid(1));
    assert_eq!(q[0].windows, vec![w("5h", 30, 2), w("week", 20, 1)]);
    assert_eq!(q[0].observed_at, Timestamp(2));
    assert_eq!(q[1].windows, vec![w("5h", 5, 3)]);
}

#[test]
fn conversations_filter_and_order_by_update() {
    let (env, store) = setup();
    env.clock.set(T0 + 10);
    block(store.commit(cmd(
        2,
        vec![
            ConversationCreated { conversation: project_conversation(11, 2) }.into(),
            ConversationCreated { conversation: project_conversation(12, 1) }.into(),
        ],
    )))
    .unwrap();
    env.clock.set(T0 + 20);
    block(store.commit(cmd(
        3,
        vec![MessageAppended { message: user_message(1, 10, "x") }.into()],
    )))
    .unwrap();
    let rm = read_model(&store);
    let ids = |p| {
        block(rm.conversations(p))
            .unwrap()
            .into_iter()
            .map(|s| s.conversation.id)
            .collect::<Vec<_>>()
    };
    assert_eq!(
        ids(Some(pid(1))),
        vec![cid(10), cid(12)],
        "有新消息的排前面"
    );
    assert_eq!(ids(None).len(), 3);
}

/// T04 方案 2.5：上次运行没收尾的东西。各状态各放一条，只返回该返回的；流式消息的正文是检查点内容。
#[test]
fn unsettled_returns_only_in_flight() {
    use iwork_domain::approval::VoidReason;
    use iwork_domain::user_action::ExitStatus;

    let (_env, store) = setup(); // 回合 20 是 Queued
    let turn_in = |n: u128, state: TurnState| {
        let mut t = turn(n, 10);
        t.state = state;
        t
    };
    let card_in = |n: u128, state: UserActionState| {
        let mut c = card(n, 20, 10);
        c.state = state;
        c
    };
    let mut done_msg = assistant_message(32, 10, 21);
    done_msg.status = MessageStatus::Completed;
    let mut events: Vec<DomainEvent> = vec![
        TurnQueued {
            turn: turn_in(21, TurnState::Running),
        }
        .into(),
        TurnQueued {
            turn: turn_in(22, TurnState::WaitingApproval),
        }
        .into(),
        TurnQueued {
            turn: turn_in(23, TurnState::Cancelling),
        }
        .into(),
        TurnQueued {
            turn: turn_in(
                24,
                TurnState::Interrupted {
                    reason: InterruptReason::Crash,
                },
            ),
        }
        .into(),
        TurnQueued {
            turn: turn_in(25, TurnState::Completed),
        }
        .into(),
        TurnQueued {
            turn: turn_in(26, TurnState::Cancelled),
        }
        .into(),
        TurnQueued {
            turn: turn_in(27, TurnState::Failed),
        }
        .into(),
        // 回合 21 名下一条在流的、一条已结束的；已结束回合 25 名下一条（数据不一致也不算）
        MessageAppended {
            message: assistant_message(30, 10, 21),
        }
        .into(),
        MessageAppended { message: done_msg }.into(),
        MessageAppended {
            message: assistant_message(31, 10, 25),
        }
        .into(),
        MessageAppended {
            message: assistant_message(33, 10, 22),
        }
        .into(),
    ];
    let cards = [
        card_in(50, UserActionState::Pending),
        card_in(51, UserActionState::Starting),
        card_in(52, UserActionState::Executing),
        card_in(53, UserActionState::Stopping),
        card_in(
            54,
            UserActionState::Done {
                exit: ExitStatus::Code(0),
            },
        ),
        card_in(55, UserActionState::Failed { error: "e".into() }),
        card_in(56, UserActionState::Unknown { error: "u".into() }),
        card_in(57, UserActionState::Ended),
        card_in(58, UserActionState::Dismissed),
        card_in(
            59,
            UserActionState::Voided {
                reason: VoidReason::TurnCancelled,
            },
        ),
    ];
    events.extend(
        cards
            .iter()
            .map(|c| UserActionProposed { card: c.clone() }.into()),
    );
    block(store.commit(cmd(2, events))).unwrap();
    block(store.checkpoint_message(mid(30), 7, "partial")).unwrap();

    let u = block(read_model(&store).unsettled()).unwrap();
    let turn_ids: Vec<_> = u.turns.iter().map(|t| t.id).collect();
    assert_eq!(
        turn_ids,
        vec![tid(21), tid(22), tid(23)],
        "只有持有进程的三种"
    );
    let msg_ids: Vec<_> = u.streaming.iter().map(|m| m.id).collect();
    assert_eq!(msg_ids, vec![mid(30), mid(33)], "这些回合名下还在流的");
    assert_eq!(u.streaming[0].body, MessageBody::Inline("partial".into()));
    assert_eq!(
        u.streaming[1].body,
        MessageBody::Inline(String::new()),
        "没有检查点：空串"
    );
    let card_ids: Vec<_> = u.cards.iter().map(|c| c.id).collect();
    assert_eq!(
        card_ids,
        vec![uaid(51), uaid(52), uaid(53)],
        "启动中、执行中、停止中"
    );
}

/// 对照：什么都没收尾时是空的。
#[test]
fn unsettled_is_empty_when_nothing_in_flight() {
    let (_env, store) = setup();
    let u = block(read_model(&store).unsettled()).unwrap();
    assert!(
        u.turns.is_empty() && u.streaming.is_empty() && u.cards.is_empty(),
        "{u:?}"
    );
}

// ---------- 续接摘要的原料（T06 方案 5.2a） ----------

/// 用户发一条消息、同一次提交排一个回合（受理 `SendMessage` 的形状）。
fn send(n: u128, msg: Message, turn_n: u128, conv: u128) -> CommitRequest {
    cmd(
        n,
        vec![
            MessageAppended { message: msg }.into(),
            TurnQueued {
                turn: turn(turn_n, conv),
            }
            .into(),
        ],
    )
}

/// 一条结束了的助手回复。
fn reply(n: u128, conv: u128, turn_n: u128, text: &str, line: Option<u128>) -> Vec<DomainEvent> {
    let mut m = assistant_message(n, conv, turn_n);
    m.context_project_id = line.map(pid);
    vec![
        MessageAppended { message: m }.into(),
        MessageCompleted {
            conversation_id: cid(conv),
            message_id: mid(n),
            body: MessageBody::Inline(text.into()),
            status: MessageStatus::Completed,
        }
        .into(),
    ]
}

fn said(text: &str) -> SummaryLine {
    SummaryLine {
        who: SummaryWho::User,
        text: SummaryText::Inline(text.into()),
        attachments: vec![],
    }
}

fn answered(text: &str) -> SummaryLine {
    SummaryLine {
        who: SummaryWho::Assistant {
            name: "iWork".into(),
        },
        text: SummaryText::Inline(text.into()),
        attachments: vec![],
    }
}

#[test]
fn summary_source_takes_first_user_and_recent_before_the_request() {
    let (_env, store) = setup();
    block(store.commit(send(2, user_message(101, 10, "第一句"), 21, 10))).unwrap();
    block(store.commit(cmd(3, reply(102, 10, 21, "回复一", None)))).unwrap();
    // 系统消息、正文为空的助手消息都不取
    let mut system = user_message(103, 10, "系统说明");
    system.author = Author::System;
    let mut empty = assistant_message(104, 10, 21);
    empty.status = MessageStatus::Failed;
    block(store.commit(cmd(
        4,
        vec![
            MessageAppended { message: system }.into(),
            MessageAppended { message: empty }.into(),
        ],
    )))
    .unwrap();
    let mut with_file = user_message(105, 10, "第二句");
    with_file.attachments = vec![atid(1)];
    block(store.commit(cmd(
        5,
        vec![AttachmentStored {
            attachment: attachment(1, 10, AttachmentOrigin::RepoPath("a.txt".into())),
        }
        .into()],
    )))
    .unwrap();
    block(store.commit(send(6, with_file, 22, 10))).unwrap();
    block(store.commit(cmd(7, reply(106, 10, 22, "回复二", None)))).unwrap();
    block(store.commit(send(8, user_message(107, 10, "这一轮"), 23, 10))).unwrap();
    // 这一轮之后的不取（续接时这一轮自己已经吐出的半截回复）
    block(store.commit(cmd(9, reply(108, 10, 23, "半截", None)))).unwrap();

    let rm = read_model(&store);
    let got = block(rm.summary_source(tid(23), None, 100)).unwrap();
    let mut second = said("第二句");
    second.attachments = vec!["a.txt".into()];
    assert_eq!(
        got,
        SummarySource {
            first_user: Some(said("第一句")),
            earlier: vec![answered("回复二"), second.clone(), answered("回复一")],
            unfetched: 0,
        }
    );
    // 条数上限：只取最新的，余下的记数
    let capped = block(rm.summary_source(tid(23), None, 2)).unwrap();
    assert_eq!(capped.earlier, vec![answered("回复二"), second]);
    assert_eq!(capped.unfetched, 1);
    assert_eq!(capped.first_user, Some(said("第一句")));
}

/// 助手私聊里按「助手 × 项目」分线：别的项目段（含私聊段）的消息不取。
#[test]
fn summary_source_filters_by_session_line() {
    let (_env, store) = setup();
    block(store.commit(cmd(
        2,
        vec![ConversationCreated { conversation: dm(11, 1) }.into()],
    )))
    .unwrap();
    let in_line = |n: u128, text: &str, line: Option<u128>| {
        let mut m = user_message(n, 11, text);
        m.context_project_id = line.map(pid);
        m
    };
    block(store.commit(send(3, in_line(201, "私聊段", None), 31, 11))).unwrap();
    block(store.commit(send(4, in_line(202, "项目一第一句", Some(1)), 32, 11))).unwrap();
    block(store.commit(cmd(5, reply(203, 11, 32, "项目一回复", Some(1))))).unwrap();
    block(store.commit(send(6, in_line(204, "项目二", Some(2)), 33, 11))).unwrap();
    block(store.commit(send(7, in_line(205, "项目一这一轮", Some(1)), 34, 11))).unwrap();

    let rm = read_model(&store);
    let got = block(rm.summary_source(tid(34), Some(pid(1)), 100)).unwrap();
    assert_eq!(got.first_user, Some(said("项目一第一句")));
    assert_eq!(got.earlier, vec![answered("项目一回复")]);
    // 对照：私聊段那条线上只有它自己
    let dm_line = block(rm.summary_source(tid(34), None, 100)).unwrap();
    assert_eq!(dm_line.first_user, Some(said("私聊段")));
    assert!(dm_line.earlier.is_empty());
}

/// 这一轮就是第一句：此前什么都没有；回合不存在报 NotFound。
#[test]
fn summary_source_of_first_turn_is_empty() {
    let (_env, store) = setup();
    block(store.commit(send(2, user_message(301, 10, "第一句"), 41, 10))).unwrap();
    let rm = read_model(&store);
    assert_eq!(
        block(rm.summary_source(tid(41), None, 100)).unwrap(),
        SummarySource::default(),
        "这一轮的请求和排回合同一次提交，在界外"
    );
    assert!(matches!(
        block(rm.summary_source(tid(99), None, 100)),
        Err(iwork_ports::store::StorageError::NotFound { entity: "turn", .. })
    ));
}

/// 不是由用户消息触发的回合（系统回合）也有上界：排它的那次提交之后写下的消息不取（审查 1/F-5）。
#[test]
fn summary_source_of_system_turn_stops_at_its_queue_commit() {
    let (_env, store) = setup();
    block(store.commit(send(2, user_message(401, 10, "之前的话"), 51, 10))).unwrap();
    block(store.commit(cmd(3, reply(402, 10, 51, "之前的回复", None)))).unwrap();
    // 系统回合：同一次提交里没有用户消息
    let mut system = turn(52, 10);
    system.kind = iwork_domain::turn::TurnKind::SystemAgentsMdDraft;
    block(store.commit(cmd(4, vec![TurnQueued { turn: system }.into()]))).unwrap();
    block(store.commit(cmd(5, reply(403, 10, 52, "系统回合自己的回复", None)))).unwrap();
    block(store.commit(send(6, user_message(404, 10, "之后的话"), 53, 10))).unwrap();

    let rm = read_model(&store);
    assert_eq!(
        block(rm.summary_source(tid(52), None, 100)).unwrap(),
        SummarySource {
            first_user: Some(said("之前的话")),
            earlier: vec![answered("之前的回复")],
            unfetched: 0,
        }
    );
    // 对照：后面那个回合看得到系统回合的回复
    assert_eq!(
        block(rm.summary_source(tid(53), None, 100))
            .unwrap()
            .earlier,
        vec![answered("系统回合自己的回复"), answered("之前的回复")]
    );
}

/// 用户消息触发的回合，请求和排回合分两次提交：前提不成立，报错点名回合，不把请求收进摘要（审查 2/F-3）。
#[test]
fn summary_source_rejects_request_outside_queue_commit() {
    let (_env, store) = setup();
    block(store.commit(cmd(
        2,
        vec![MessageAppended { message: user_message(501, 10, "请求") }.into()],
    )))
    .unwrap();
    block(store.commit(cmd(3, vec![TurnQueued { turn: turn(61, 10) }.into()]))).unwrap();
    let rm = read_model(&store);
    let err = block(rm.summary_source(tid(61), None, 100)).unwrap_err();
    assert!(err.to_string().contains(&tid(61).to_string()), "{err}");
    // 对照：同一次提交的照常
    block(store.commit(send(4, user_message(502, 10, "下一句"), 62, 10))).unwrap();
    assert!(block(rm.summary_source(tid(62), None, 100)).is_ok());
}
