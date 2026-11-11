//! 审批引擎的交错性质测试（T07 方案 6.10.13「性质测试」）。
//!
//! 一步一步的假存储 + 手写的单线程执行器：调度表随机决定推进哪个操作的 future 一步、让存储处理一笔、
//! 送达哪份回执、让下一次提交 Busy / 读失败、丢弃哪个还在跑的 future、给下一个提交开入队门 / 开门。
//! 跑完后丢掉全部 future、开所有门、处理完队列，再随机二选一：调一次 `sync()`，或者丢掉引擎、
//! 用同一份日志 `start` 一个新的并提交恢复计划。然后断言方案列的 9 条。
//!
//! 默认 256 例；完整一轮 `PROPTEST_CASES=5000`（套 `lockf /tmp/iwork-heavy-test.lock`）。
//! 范围说明：AI 审查不在生成器里（审查运行时要异步定时器，审查的交错由确定性用例守）。

mod support;

use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;

use iwork_core::approval::engine::fold::Folded;
use iwork_core::approval::engine::{
    ApprovalEngine, EnginePorts, MSG_TURN_CANCELLED, MSG_USER_DENIED, Scope, SessionKeys,
    UserDecision,
};
use iwork_core::approval::session::tool_set_for;
use iwork_core::turn::Decided;
use iwork_domain::action::ActionFingerprint;
use iwork_domain::approval::{ApprovalState, Decider, VoidReason, owner_alive};
use iwork_domain::approval_mode::{ApprovalMode, RememberScope};
use iwork_domain::common::{Actor, ModelId, ModelSelection, ToolGroups};
use iwork_domain::event::*;
use iwork_domain::id::{ApprovalId, AssistantId, ConversationId, RawUlid, Seq, TurnId};
use iwork_domain::message::Author;
use iwork_domain::project::AiReviewSettings;
use iwork_domain::rule::Verdict;
use iwork_domain::turn::{InterruptReason, TurnKind, TurnState};
use iwork_domain::user_action::{ExitStatus, UserActionState};
use iwork_ports::agent::PermissionDecision;
use iwork_ports::store::{CommitRequest, StoragePort, StoredEvent};
use proptest::prelude::*;
use support::engine_fakes::{FakeReviewer, FakeTimer, Ids, TickClock};
use support::fake_fs::FakeFs;
use support::reference_projection::{ReferenceProjection, apply_all};
use support::step_store::{StepStore, Tasks};
use support::*;

type Engine = ApprovalEngine<TickClock, Ids, StepStore, FakeReviewer, FakeTimer, FakeFs>;

// ---------- 布景 ----------

fn aid(n: u8) -> AssistantId {
    AssistantId::from_raw(RawUlid::from_u128(0x21 + u128::from(n)))
}
fn cid(n: u8) -> ConversationId {
    ConversationId::from_raw(RawUlid::from_u128(0x31 + u128::from(n)))
}
fn tid(n: u8) -> TurnId {
    TurnId::from_raw(RawUlid::from_u128(0x501 + u128::from(n)))
}

/// 回合 → (对话, 助手)。c0 有两个回合（两个助手），c1 一个；t3 在第三个对话里，由「新建对话」操作建出来。
const TURNS: [(u8, u8); 4] = [(0, 0), (0, 1), (1, 0), (2, 1)];

fn keys_for_turn(t: u8, generation: u32) -> SessionKeys {
    let (c, a) = TURNS[usize::from(t)];
    SessionKeys {
        conversation: cid(c),
        assistant: aid(a),
        runtime_id: iwork_domain::id::RuntimeId::from_raw(RawUlid::from_u128(0x888)),
        assistant_version: generation,
        project: Some(project()),
        turn_kind: TurnKind::Assistant,
        snapshot: Arc::new(snapshot_on(
            ApprovalMode::Manual,
            &[],
            &env(),
            Seq(1),
            &fs(),
        )),
        tool_set: tool_set_for(TurnKind::Assistant, &ToolGroups::new()),
        disabled_groups: ToolGroups::new(),
        cwd: cp(WS),
        paths: env(),
        ai_review: AiReviewSettings {
            enabled: false,
            runtime_id: None,
            model: None,
            timeout_secs: 60,
        },
        review_model: ModelSelection {
            model_id: ModelId("m".into()),
            effort: None,
        },
    }
}

fn turn_entity(t: u8) -> iwork_domain::turn::Turn {
    let (c, a) = TURNS[usize::from(t)];
    let mut x = fold_fx::turn(1, 1, 1);
    x.id = tid(t);
    x.conversation_id = cid(c);
    x.assistant_id = aid(a);
    x
}

fn conversation_entity(c: u8) -> iwork_domain::conversation::Conversation {
    let mut x = fold_fx::project_conversation(1, 1);
    x.id = cid(c);
    x.kind = iwork_domain::conversation::ConversationKind::Project {
        project_id: project(),
    };
    x
}

fn world() -> Vec<DomainEvent> {
    let mut events = fold_fx::world_with(project(), cid(0), &[aid(0), aid(1)], None);
    events.push(
        ConversationCreated {
            conversation: conversation_entity(1),
        }
        .into(),
    );
    for t in 0..3 {
        events.push(
            TurnQueued {
                turn: turn_entity(t),
            }
            .into(),
        );
        events.push(state(t, TurnState::Queued, TurnState::WaitingApproval));
    }
    events
}

fn state(t: u8, from: TurnState, to: TurnState) -> DomainEvent {
    TurnStateChanged {
        turn_id: tid(t),
        from,
        to,
        reason: None,
    }
    .into()
}

const COMMANDS: [&str; 4] = ["ls", "npm test", "cat ~/.ssh/id_rsa", "rm a.txt"];

// ---------- 生成器 ----------

#[derive(Debug, Clone)]
enum Op {
    Request {
        turn: u8,
        cmd: u8,
    },
    Ask {
        turn: u8,
    },
    Decide {
        nth: u8,
        kind: u8,
    },
    Switch {
        conv: u8,
        mode: u8,
    },
    Deliver {
        nth: u8,
    },
    Interrupt {
        turn: u8,
    },
    Resume {
        turn: u8,
    },
    Cancel {
        turn: u8,
    },
    Finish {
        turn: u8,
    },
    DeleteConversation {
        conv: u8,
    },
    DropConversation {
        conv: u8,
    },
    DeleteAssistant {
        a: u8,
    },
    NewConversationTurn,
    UserMessage {
        conv: u8,
    },
    Card {
        nth: u8,
        kind: u8,
    },
    Revoke {
        nth: u8,
    },
    Sync,
    /// 中断（扮演等待上限）→ 续接换会话键 → 回到等审批，并原样重发这个回合最近一次被问的调用
    Retry {
        turn: u8,
        race: bool,
    },
    /// 第 9 轮 9/F-2：用户拒绝一条待审批，拒绝那笔入队后 decide 的 future 被丢弃（提交去向不明），
    /// 同一回合的同一调用紧跟着到达。下一个决定必须先过屏障再读，不能在旧视图上问或放行
    DenyDropped {
        nth: u8,
    },
}

#[derive(Debug, Clone)]
enum Choice {
    Start(Op),
    Poll(u8),
    Step,
    Deliver(u8),
    FailNext,
    FailRead,
    Drop(u8),
    Gate,
    Open(u8),
    /// 全体推进一轮：每个任务推一步、处理完队列、送达全部回执
    Settle,
}

fn op() -> impl Strategy<Value = Op> {
    let t = || 0u8..4;
    let c = || 0u8..3;
    prop_oneof![
        20 => (t(), 0u8..4).prop_map(|(turn, cmd)| Op::Request { turn, cmd }),
        3 => t().prop_map(|turn| Op::Ask { turn }),
        12 => (any::<u8>(), 0u8..4).prop_map(|(nth, kind)| Op::Decide { nth, kind }),
        2 => (c(), 0u8..3).prop_map(|(conv, mode)| Op::Switch { conv, mode }),
        8 => any::<u8>().prop_map(|nth| Op::Deliver { nth }),
        3 => t().prop_map(|turn| Op::Interrupt { turn }),
        2 => t().prop_map(|turn| Op::Resume { turn }),
        2 => t().prop_map(|turn| Op::Cancel { turn }),
        1 => t().prop_map(|turn| Op::Finish { turn }),
        1 => c().prop_map(|conv| Op::DeleteConversation { conv }),
        1 => c().prop_map(|conv| Op::DropConversation { conv }),
        1 => (0u8..2).prop_map(|a| Op::DeleteAssistant { a }),
        1 => Just(Op::NewConversationTurn),
        2 => c().prop_map(|conv| Op::UserMessage { conv }),
        2 => (any::<u8>(), 0u8..3).prop_map(|(nth, kind)| Op::Card { nth, kind }),
        1 => any::<u8>().prop_map(|nth| Op::Revoke { nth }),
        2 => Just(Op::Sync),
        8 => (t(), any::<bool>()).prop_map(|(turn, race)| Op::Retry { turn, race }),
        5 => any::<u8>().prop_map(|nth| Op::DenyDropped { nth }),
    ]
}

fn choice() -> impl Strategy<Value = Choice> {
    prop_oneof![
        10 => op().prop_map(Choice::Start),
        14 => any::<u8>().prop_map(Choice::Poll),
        8 => Just(Choice::Step),
        8 => any::<u8>().prop_map(Choice::Deliver),
        8 => Just(Choice::Settle),
        1 => Just(Choice::FailNext),
        1 => Just(Choice::FailRead),
        2 => any::<u8>().prop_map(Choice::Drop),
        1 => Just(Choice::Gate),
        2 => any::<u8>().prop_map(Choice::Open),
    ]
}

// ---------- 执行 ----------

/// 一个任务的结果。
enum Out {
    Request { turn: u8, decided: Decided },
    Other,
}

struct Run {
    store: StepStore,
    engine: Arc<Engine>,
    tasks: Tasks<Out>,
    /// 回合 → 当前会话键（续接换一份）
    keys: BTreeMap<u8, Arc<SessionKeys>>,
    generation: u32,
    /// 每个请求任务用的会话键（断言 9）
    request_keys: BTreeMap<usize, (u8, Arc<SessionKeys>)>,
    delivered: BTreeSet<ApprovalId>,
    new_conversation: bool,
}

fn ports(
    store: &StepStore,
) -> EnginePorts<TickClock, Ids, StepStore, FakeReviewer, FakeTimer, FakeFs> {
    EnginePorts {
        clock: TickClock::default(),
        ids: Ids::default(),
        store: store.clone(),
        reviewer: FakeReviewer::default(),
        timer: FakeTimer::default(),
        fs: fs(),
    }
}

fn cmd(events: Vec<DomainEvent>) -> CommitRequest {
    CommitRequest {
        command_id: iwork_domain::id::CommandId::from_raw(RawUlid::from_u128(7)),
        actor: Actor("test".into()),
        events,
        outbox: Vec::new(),
    }
}

/// 同步跑完一个 future：它要提交就一步步处理队列、送回执。
fn drive<T>(store: &StepStore, f: impl std::future::Future<Output = T> + Send + 'static) -> T
where
    T: Send + 'static,
{
    let mut tasks: Tasks<T> = Tasks::default();
    let i = tasks.spawn(f);
    for _ in 0..10_000 {
        tasks.poll(i);
        if let Some(out) = tasks.done.remove(&i) {
            return out;
        }
        while store.step().is_some() {}
        for t in store.undelivered() {
            store.deliver(t);
        }
    }
    panic!("任务没有结束");
}

impl Run {
    fn new() -> Self {
        let store = StepStore::default();
        store.seed(world());
        let (engine, plan) = drive(
            &store,
            Engine::start(ports(&store), Default::default(), |_, _| None),
        )
        .expect("启动");
        assert!(plan.is_empty());
        let keys = (0..4).map(|t| (t, Arc::new(keys_for_turn(t, 0)))).collect();
        Self {
            store,
            engine,
            tasks: Tasks::default(),
            keys,
            generation: 0,
            request_keys: BTreeMap::new(),
            delivered: BTreeSet::new(),
            new_conversation: false,
        }
    }

    fn log_events(&self) -> Vec<DomainEvent> {
        self.store.log().into_iter().map(|e| e.event).collect()
    }

    /// 回合最近落盘的状态。
    fn turn_state(&self, t: u8) -> Option<TurnState> {
        let mut out = None;
        for e in self.log_events() {
            match e {
                DomainEvent::TurnQueued(q) if q.turn.id == tid(t) => out = Some(TurnState::Queued),
                DomainEvent::TurnStateChanged(c) if c.turn_id == tid(t) => out = Some(c.to),
                _ => {}
            }
        }
        out
    }

    fn approvals(&self) -> Vec<ApprovalId> {
        self.log_events()
            .into_iter()
            .filter_map(|e| match e {
                DomainEvent::ApprovalRequested(r) => Some(r.approval.id),
                _ => None,
            })
            .collect()
    }

    fn cards(&self) -> Vec<(iwork_domain::id::UserActionId, ConversationId)> {
        self.log_events()
            .into_iter()
            .filter_map(|e| match e {
                DomainEvent::UserActionProposed(p) => Some((p.card.id, p.card.conversation_id)),
                _ => None,
            })
            .collect()
    }

    fn commit_task(&mut self, events: Vec<DomainEvent>) {
        let store = self.store.clone();
        self.tasks.spawn(async move {
            let _ = store.commit(cmd(events)).await;
            Out::Other
        });
    }

    fn start(&mut self, op: Op) {
        let engine = self.engine.clone();
        match op {
            Op::Request { turn, cmd } => {
                // 每个请求一份自己的 Arc：断言 9 能认出绑定来自哪个请求
                let keys = Arc::new((*self.keys[&turn]).clone());
                let req = bash_req(COMMANDS[usize::from(cmd)]);
                let k = keys.clone();
                let i = self.tasks.spawn(async move {
                    let decided = engine.on_permission(&k, tid(turn), &req).await;
                    Out::Request { turn, decided }
                });
                self.request_keys.insert(i, (turn, keys));
            }
            Op::Ask { turn } => {
                if let Some(from) = self.turn_state(turn)
                    && !from.is_terminal()
                {
                    self.commit_task(vec![state(turn, from, TurnState::WaitingApproval)]);
                }
            }
            Op::Decide { nth, kind } => {
                // 多数挑还待定的；偶尔挑已处理的（应当报 NotPending）
                let pending: Vec<ApprovalId> = self
                    .store
                    .projection()
                    .approvals
                    .iter()
                    .filter(|(_, (s, _, _, _))| *s == ApprovalState::Pending)
                    .map(|(id, _)| *id)
                    .collect();
                let ids = if pending.is_empty() || nth % 8 == 0 {
                    self.approvals()
                } else {
                    pending
                };
                if ids.is_empty() {
                    return;
                }
                let id = ids[usize::from(nth) % ids.len()];
                let decision = match kind {
                    0 => UserDecision::Deny,
                    1 => UserDecision::Allow {
                        remember: RememberScope::Once,
                    },
                    2 => UserDecision::Allow {
                        remember: RememberScope::Conversation,
                    },
                    _ => UserDecision::Allow {
                        remember: RememberScope::Project,
                    },
                };
                self.tasks.spawn(async move {
                    let _ = engine.decide(id, decision, Actor("user".into())).await;
                    Out::Other
                });
            }
            Op::Switch { conv, mode } => {
                let mode = [
                    ApprovalMode::Manual,
                    ApprovalMode::Auto,
                    ApprovalMode::FullAccess,
                ][usize::from(mode)];
                self.tasks.spawn(async move {
                    let _ = engine
                        .switch_mode(
                            ApprovalModeTarget::Conversation(cid(conv)),
                            Some(mode),
                            Actor("user".into()),
                        )
                        .await;
                    Out::Other
                });
            }
            Op::Deliver { nth } => {
                // 编排采纳一条已返回、出自审批的结果：回合在等审批才提交（T06 2.6 第 4 条）
                let adopted: Vec<(u8, ApprovalId)> = self
                    .tasks
                    .done
                    .values()
                    .filter_map(|o| match o {
                        Out::Request { turn, decided, .. } => decided.approval.map(|a| (*turn, a)),
                        Out::Other => None,
                    })
                    .filter(|(_, a)| !self.delivered.contains(a))
                    .collect();
                if adopted.is_empty() {
                    return;
                }
                let (turn, a) = adopted[usize::from(nth) % adopted.len()];
                if self.turn_state(turn) == Some(TurnState::WaitingApproval) {
                    self.delivered.insert(a);
                    self.commit_task(vec![
                        ApprovalDelivered {
                            approval_id: a,
                            turn_id: tid(turn),
                        }
                        .into(),
                    ]);
                }
            }
            Op::Interrupt { turn } => {
                if let Some(from) = self.turn_state(turn)
                    && !from.is_terminal()
                {
                    self.commit_task(vec![state(
                        turn,
                        from,
                        TurnState::Interrupted {
                            reason: InterruptReason::ApprovalWaitLimit,
                        },
                    )]);
                }
            }
            Op::Resume { turn } => {
                if let Some(from @ TurnState::Interrupted { .. }) = self.turn_state(turn) {
                    self.generation += 1;
                    self.keys
                        .insert(turn, Arc::new(keys_for_turn(turn, self.generation)));
                    self.commit_task(vec![state(turn, from, TurnState::Running)]);
                }
            }
            Op::Cancel { turn } => {
                let Some(from) = self.turn_state(turn) else {
                    return;
                };
                if from.is_terminal() || from == TurnState::Cancelling {
                    return;
                }
                let (c, _) = TURNS[usize::from(turn)];
                let store = self.store.clone();
                self.tasks.spawn(async move {
                    if let Ok((voids, hold)) = engine
                        .void_plan(cid(c), tid(turn), VoidReason::TurnCancelled)
                        .await
                    {
                        let mut events = vec![state(turn, from, TurnState::Cancelling)];
                        events.extend(voids);
                        let _ = store.commit(cmd(events)).await;
                        drop(hold);
                        engine.nudge().await;
                    }
                    Out::Other
                });
            }
            Op::Finish { turn } => {
                if let Some(from) = self.turn_state(turn)
                    && !from.is_terminal()
                    && !matches!(from, TurnState::WaitingApproval)
                {
                    self.commit_task(vec![
                        state(turn, from, TurnState::Cancelled),
                        TurnFinished {
                            turn_id: tid(turn),
                            state: TurnState::Cancelled,
                            usage: Default::default(),
                            celebrate: false,
                            change_summary: None,
                        }
                        .into(),
                    ]);
                }
            }
            Op::DeleteConversation { conv } => {
                self.tasks.spawn(async move {
                    let _ = engine
                        .commit_with(
                            Scope::ConversationDeleting(cid(conv)),
                            cmd(vec![
                                ConversationDeleting {
                                    conversation_id: cid(conv),
                                }
                                .into(),
                            ]),
                        )
                        .await;
                    Out::Other
                });
            }
            Op::DropConversation { conv } => {
                self.commit_task(vec![
                    ConversationDeleted {
                        conversation_id: cid(conv),
                    }
                    .into(),
                ]);
            }
            Op::DeleteAssistant { a } => {
                self.tasks.spawn(async move {
                    let _ = engine
                        .commit_with(
                            Scope::AssistantDeleted(aid(a)),
                            cmd(vec![
                                AssistantDeleted {
                                    assistant_id: aid(a),
                                    name_snapshot: "a".into(),
                                }
                                .into(),
                            ]),
                        )
                        .await;
                    Out::Other
                });
            }
            Op::NewConversationTurn => {
                if !self.new_conversation {
                    self.new_conversation = true;
                    self.commit_task(vec![
                        ConversationCreated {
                            conversation: conversation_entity(2),
                        }
                        .into(),
                        TurnQueued {
                            turn: turn_entity(3),
                        }
                        .into(),
                        state(3, TurnState::Queued, TurnState::WaitingApproval),
                    ]);
                }
            }
            Op::UserMessage { conv } => {
                let mut m = fold_fx::user_message(9, 1);
                m.id = iwork_domain::id::MessageId::from_raw(RawUlid::from_u128(
                    0x7000 + self.store.log().len() as u128,
                ));
                m.conversation_id = cid(conv);
                self.commit_task(vec![MessageAppended { message: m }.into()]);
            }
            Op::Card { nth, kind } => {
                let cards = self.cards();
                if cards.is_empty() {
                    return;
                }
                let (card, conv) = cards[usize::from(nth) % cards.len()];
                let change = |s| -> DomainEvent {
                    UserActionStateChanged {
                        card_id: card,
                        state: s,
                        output_artifact: None,
                    }
                    .into()
                };
                match kind {
                    0 => {
                        let events = vec![change(UserActionState::Starting)];
                        self.tasks.spawn(async move {
                            let _ = engine.commit_with(Scope::Card(conv), cmd(events)).await;
                            Out::Other
                        });
                    }
                    1 => {
                        let events = vec![change(UserActionState::Dismissed)];
                        self.tasks.spawn(async move {
                            let _ = engine.commit_with(Scope::Card(conv), cmd(events)).await;
                            Out::Other
                        });
                    }
                    _ => self.commit_task(vec![
                        change(UserActionState::Executing),
                        change(UserActionState::Done {
                            exit: ExitStatus::Code(0),
                        }),
                    ]),
                }
            }
            Op::Revoke { nth } => {
                let rules: Vec<_> = self
                    .log_events()
                    .into_iter()
                    .filter_map(|e| match e {
                        DomainEvent::RuleRemembered(r) => Some(r.rule.id),
                        _ => None,
                    })
                    .collect();
                if !rules.is_empty() {
                    let rule_id = rules[usize::from(nth) % rules.len()];
                    self.commit_task(vec![RuleRevoked { rule_id }.into()]);
                }
            }
            Op::Sync => {
                self.tasks.spawn(async move {
                    let _ = engine.sync().await;
                    Out::Other
                });
            }
            Op::DenyDropped { nth } => self.deny_dropped(nth),
            Op::Retry { turn, race } => {
                let Some(from) = self.turn_state(turn) else {
                    return;
                };
                if from.is_terminal() || from == TurnState::Cancelling {
                    return;
                }
                // 优先挑这个回合上已被批准的那条（一次性放行就出自它），没有就挑最近一次被问的
                let events = self.log_events();
                let approved: BTreeSet<ApprovalId> = events
                    .iter()
                    .filter_map(|e| match e {
                        DomainEvent::ApprovalDecided(d)
                            if matches!(d.state, ApprovalState::Approved { .. }) =>
                        {
                            Some(d.approval_id)
                        }
                        _ => None,
                    })
                    .collect();
                let asked: Vec<(ApprovalId, String)> = events
                    .iter()
                    .filter_map(|e| match e {
                        DomainEvent::ApprovalRequested(r) if r.approval.turn_id == tid(turn) => {
                            Some((r.approval.id, r.approval.action.raw_summary.clone()))
                        }
                        _ => None,
                    })
                    .collect();
                let Some(command) = asked
                    .iter()
                    .rev()
                    .find(|(id, _)| approved.contains(id))
                    .or(asked.last())
                    .map(|(_, c)| c.clone())
                else {
                    return;
                };
                // 等待上限中断（没中断过的先中断）→ 续接换一份会话键 → 回到等审批
                self.generation += 1;
                self.keys
                    .insert(turn, Arc::new(keys_for_turn(turn, self.generation)));
                let interrupted = TurnState::Interrupted {
                    reason: InterruptReason::ApprovalWaitLimit,
                };
                let mut events = Vec::new();
                if !matches!(from, TurnState::Interrupted { .. }) {
                    events.push(state(turn, from, interrupted));
                }
                events.push(state(turn, interrupted, TurnState::WaitingApproval));
                let keys = Arc::new((*self.keys[&turn]).clone());
                let store = self.store.clone();
                let k = keys.clone();
                let i = self.tasks.spawn(async move {
                    let _ = store.commit(cmd(events)).await;
                    let decided = engine
                        .on_permission(&k, tid(turn), &bash_req(&command))
                        .await;
                    Out::Request { turn, decided }
                });
                self.request_keys.insert(i, (turn, keys));
                if race {
                    // 编排的等待上限和这次请求并发：中断可能落在求值之后、审计之前（方案审查 1/F-1）
                    self.commit_task(vec![state(turn, TurnState::WaitingApproval, interrupted)]);
                }
            }
        }
    }

    fn deny_dropped(&mut self, nth: u8) {
        let pending: Vec<(ApprovalId, TurnId, String)> = self
            .log_events()
            .into_iter()
            .filter_map(|e| match e {
                DomainEvent::ApprovalRequested(r) => Some((
                    r.approval.id,
                    r.approval.turn_id,
                    r.approval.action.raw_summary.clone(),
                )),
                _ => None,
            })
            .filter(|(id, _, _)| {
                self.store
                    .projection()
                    .approvals
                    .get(id)
                    .is_some_and(|(s, _, _, _)| *s == ApprovalState::Pending)
            })
            .collect();
        if pending.is_empty() {
            return;
        }
        let (id, turn_id, command) = pending[usize::from(nth) % pending.len()].clone();
        let Some(turn) = (0..4u8).find(|t| tid(*t) == turn_id) else {
            return;
        };
        let engine = self.engine.clone();
        let decide = self.tasks.spawn(async move {
            let _ = engine
                .decide(id, UserDecision::Deny, Actor("user".into()))
                .await;
            Out::Other
        });
        // 推到拒绝那笔入队为止；它前面要是先排了一笔屏障，就让存储处理掉、送回执
        for _ in 0..20 {
            self.tasks.poll(decide);
            let queued = self.store.queued_kinds();
            if queued.iter().any(|k| k.contains(&"approval_decided"))
                || self.tasks.done.contains_key(&decide)
            {
                break;
            }
            if !queued.is_empty() {
                self.store.step();
                for t in self.store.undelivered() {
                    self.store.deliver(t);
                }
            }
        }
        self.tasks.drop_task(decide);
        let keys = Arc::new((*self.keys[&turn]).clone());
        let k = keys.clone();
        let engine = self.engine.clone();
        let i = self.tasks.spawn(async move {
            let decided = engine
                .on_permission(&k, tid(turn), &bash_req(&command))
                .await;
            Out::Request { turn, decided }
        });
        self.tasks.poll(i);
        self.request_keys.insert(i, (turn, keys));
    }

    fn apply(&mut self, c: Choice) {
        let pick = |v: &[usize], n: u8| (!v.is_empty()).then(|| v[usize::from(n) % v.len()]);
        match c {
            Choice::Start(op) => self.start(op),
            Choice::Poll(n) => {
                if let Some(i) = pick(&self.tasks.running(), n) {
                    self.tasks.poll(i);
                }
            }
            Choice::Step => {
                self.store.step();
            }
            Choice::Deliver(n) => {
                let pending: Vec<usize> = self
                    .store
                    .undelivered()
                    .iter()
                    .map(|t| *t as usize)
                    .collect();
                if let Some(t) = pick(&pending, n) {
                    self.store.deliver(t as u64);
                }
            }
            Choice::FailNext => self.store.fail_next(),
            Choice::FailRead => self.store.fail_read(),
            Choice::Drop(n) => {
                if let Some(i) = pick(&self.tasks.running(), n) {
                    self.tasks.drop_task(i);
                }
            }
            Choice::Gate => self.store.gate_next_commit(),
            Choice::Settle => {
                self.tasks.poll_all();
                while self.store.step().is_some() {}
                for t in self.store.undelivered() {
                    self.store.deliver(t);
                }
                self.tasks.poll_all();
            }
            Choice::Open(n) => {
                let gates: Vec<usize> = self
                    .store
                    .closed_gates()
                    .iter()
                    .map(|g| *g as usize)
                    .collect();
                if let Some(g) = pick(&gates, n) {
                    self.store.open_gate(g as u64);
                }
            }
        }
    }

    /// 收尾：丢掉全部还在跑的 future、开所有门、处理完队列。
    fn wind_down(&mut self) {
        // 先让完成在即的任务把结果交出来（断言 7 只看没被丢弃的）
        for _ in 0..3 {
            self.tasks.poll_all();
        }
        for i in self.tasks.running() {
            self.tasks.drop_task(i);
        }
        self.store.clear_faults();
        for g in self.store.closed_gates() {
            self.store.open_gate(g);
        }
        while self.store.step().is_some() {}
        for t in self.store.undelivered() {
            self.store.deliver(t);
        }
    }
}

// ---------- 断言用的独立参照 ----------

/// 拒绝记忆的参照实现（20 行，独立于折叠）：对话 → 指纹 → 是否自动拒绝（否则强制询问一次）。
#[derive(Default, Clone)]
struct DenialRef {
    by_conversation: BTreeMap<ConversationId, BTreeMap<ActionFingerprint, bool>>,
    fingerprints: BTreeMap<ApprovalId, (ConversationId, BTreeSet<ActionFingerprint>)>,
}

impl DenialRef {
    fn apply(&mut self, e: &DomainEvent) {
        match e {
            DomainEvent::ApprovalRequested(r) => {
                let a = &r.approval;
                self.fingerprints
                    .insert(a.id, (a.conversation_id, a.fingerprints()));
            }
            DomainEvent::ApprovalDecided(d) => {
                let Some((c, fps)) = self.fingerprints.get(&d.approval_id).cloned() else {
                    return;
                };
                let m = self.by_conversation.entry(c).or_default();
                match d.state {
                    ApprovalState::Denied => fps.into_iter().for_each(|f| {
                        m.insert(f, true);
                    }),
                    ApprovalState::Approved {
                        decider: Decider::User,
                    } => m.retain(|f, auto| *auto || !fps.contains(f)),
                    _ => {}
                }
            }
            DomainEvent::MessageAppended(m) if m.message.author == Author::User => {
                if let Some(c) = self.by_conversation.get_mut(&m.message.conversation_id) {
                    c.values_mut().for_each(|auto| *auto = false);
                }
            }
            DomainEvent::ConversationDeleted(d) => {
                self.by_conversation.remove(&d.conversation_id);
            }
            _ => {}
        }
    }
}

fn check(run: &Run, engine: &Engine, check_bindings: bool) -> Result<(), TestCaseError> {
    let log: Vec<StoredEvent> = run.store.log();
    let folded = engine.folded();
    // 1. 内存等于折叠
    prop_assert_eq!(
        &folded,
        &Folded::replay(log.clone()).map_err(|e| TestCaseError::fail(e.to_string()))?
    );
    // 3. 日志合法：从空的参照投影重放整份日志
    let mut projection = ReferenceProjection::default();
    for e in &log {
        apply_all(&mut projection, std::slice::from_ref(&e.event))
            .map_err(|m| TestCaseError::fail(format!("seq {} 不合法：{m}", e.seq)))?;
    }
    // 2. 折叠等于投影：待审批集合、卡片状态；拒绝记忆等于参照实现
    let pending_projection: BTreeSet<ApprovalId> = projection
        .approvals
        .iter()
        .filter(|(_, (s, _, _, _))| *s == ApprovalState::Pending)
        .map(|(id, _)| *id)
        .collect();
    let pending_fold: BTreeSet<ApprovalId> = folded
        .conversations()
        .flat_map(|(_, c)| c.pending.keys().copied())
        .collect();
    prop_assert_eq!(&pending_fold, &pending_projection);
    for (id, (state, _, _)) in &projection.cards {
        if !state.is_terminal() {
            let conv = folded
                .card_conversation(*id)
                .expect("没到终态的卡片在折叠里");
            prop_assert_eq!(&folded.conversation(conv).unwrap().cards[id].state, state);
        }
    }
    let mut denial = DenialRef::default();
    // 前缀上的检查：4（AC05）、5（批一次最多执行一次）、6（批准时归属在）、8（授权不卡在中间）
    let mut prefix_projection = ReferenceProjection::default();
    let mut replay = Folded::default();
    let mut delivered: BTreeMap<ApprovalId, usize> = BTreeMap::new();
    let mut last_interrupt: BTreeMap<TurnId, Seq> = BTreeMap::new();
    let mut valid_uses: BTreeMap<ApprovalId, Vec<Seq>> = BTreeMap::new();
    let mut interrupts: BTreeMap<TurnId, Vec<Seq>> = BTreeMap::new();
    let mut approved: BTreeMap<ApprovalId, TurnId> = BTreeMap::new();
    let mut invalidated: BTreeSet<ApprovalId> = BTreeSet::new();
    let mut ended: BTreeSet<TurnId> = BTreeSet::new();
    for e in &log {
        match &e.event {
            DomainEvent::ApprovalDecided(d) => {
                if let ApprovalState::Approved { decider } = d.state {
                    let (conv, fps) = denial.fingerprints[&d.approval_id].clone();
                    if decider != Decider::User {
                        let memory = denial.by_conversation.get(&conv);
                        for f in &fps {
                            prop_assert!(
                                memory.is_none_or(|m| !m.contains_key(f)),
                                "AC05：seq {} 的非用户放行越过了拒绝记忆",
                                e.seq
                            );
                        }
                    }
                    let turn = prefix_projection.approvals[&d.approval_id].2;
                    prop_assert!(
                        owner_alive(&prefix_projection.owner_facts(turn)).is_ok(),
                        "seq {} 批准时归属已不在",
                        e.seq
                    );
                    approved.insert(d.approval_id, turn);
                }
            }
            DomainEvent::ApprovalDelivered(d) => {
                *delivered.entry(d.approval_id).or_default() += 1;
            }
            DomainEvent::TurnStateChanged(c) => {
                if matches!(c.to, TurnState::Interrupted { .. }) {
                    last_interrupt.insert(c.turn_id, e.seq);
                    interrupts.entry(c.turn_id).or_default().push(e.seq);
                }
                if c.to.is_terminal() {
                    ended.insert(c.turn_id);
                }
            }
            DomainEvent::TurnFinished(f) => {
                ended.insert(f.turn_id);
            }
            DomainEvent::ApprovalEvaluated(a) => {
                // 4'（9/F-2）：审计落盘那一刻，子动作指纹在拒绝记忆里——自动拒绝相位时结果必须是拒绝，
                // 强制询问相位时不能是放行。规则放行、全开放行、一次性放行都要过这一关
                if let Some(conv) = prefix_projection.turns.get(&a.turn_id).map(|(c, _)| *c)
                    && let Some(memory) = denial.by_conversation.get(&conv)
                {
                    for x in &a.sub_actions {
                        match memory.get(&x.fingerprint()) {
                            Some(true) => prop_assert!(
                                matches!(a.verdict, Verdict::Deny(_)),
                                "AC05：seq {} 的审计越过了自动拒绝：{:?}",
                                e.seq,
                                a.verdict
                            ),
                            Some(false) => prop_assert!(
                                a.verdict != Verdict::Allow,
                                "AC05：seq {} 的审计越过了强制询问",
                                e.seq
                            ),
                            None => {}
                        }
                    }
                }
                if let Some(g) = a.grant_approval_id {
                    let stale = last_interrupt
                        .get(&a.turn_id)
                        .is_some_and(|b| *b > a.live_rules_seq);
                    if a.verdict == Verdict::Allow && !stale {
                        valid_uses.entry(g).or_default().push(e.seq);
                    } else if a.verdict != Verdict::Allow {
                        invalidated.insert(g);
                    }
                }
            }
            _ => {}
        }
        denial.apply(&e.event);
        let _ = apply_all(&mut prefix_projection, std::slice::from_ref(&e.event));
        replay
            .apply(e.seq, &e.event)
            .or_else(|err| {
                if iwork_core::approval::engine::fold::in_engine_streams(&e.event) {
                    Err(err)
                } else {
                    Ok(Vec::new())
                }
            })
            .map_err(|err| TestCaseError::fail(err.to_string()))?;
        // 8. 回合在 Interrupted：等交付里没有它的条目；批准了、没交付、没作废、回合没结束的，都在放行里
        for (_, conv) in replay.conversations() {
            for g in conv.awaiting_delivery.values() {
                prop_assert!(
                    !replay.is_interrupted(g.turn),
                    "seq {} 授权卡在等交付",
                    e.seq
                );
            }
        }
        for (a, turn) in &approved {
            if replay.is_interrupted(*turn)
                && !delivered.contains_key(a)
                && !invalidated.contains(a)
                && !ended.contains(turn)
                && replay.turn_owner(*turn).is_some()
                && has_record(&log, *a)
            {
                prop_assert!(
                    replay.grant(*a).is_some(),
                    "seq {} 批准 {a} 应是一次性放行",
                    e.seq
                );
            }
        }
    }
    // 5. 每条审批最多一条交付；两次有效消费之间隔着一次那个回合的中断
    for (a, n) in &delivered {
        prop_assert!(*n <= 1, "审批 {a} 交付了 {n} 次");
    }
    for (a, uses) in &valid_uses {
        let turn = approved.get(a).copied();
        for w in uses.windows(2) {
            let between = turn
                .and_then(|t| interrupts.get(&t))
                .is_some_and(|v| v.iter().any(|s| *s > w[0] && *s < w[1]));
            prop_assert!(
                between,
                "审批 {a} 的放行在 {} 与 {} 之间被用了两次",
                w[0],
                w[1]
            );
        }
    }
    // 6. 没有孤儿
    prop_assert!(
        folded.orphans().is_empty(),
        "收尾后还有孤儿：{:?}",
        folded.orphans()
    );
    // 7. 等待方拿到的答复和审批的终态一致
    for out in run.tasks.done.values() {
        if let Out::Request { decided, .. } = out
            && let Some(a) = decided.approval
            && let Some((state, _, _, _)) = projection.approvals.get(&a)
        {
            match &decided.decision {
                PermissionDecision::Allow => prop_assert!(
                    matches!(state, ApprovalState::Approved { .. }),
                    "{a} 放行了但终态是 {state:?}"
                ),
                PermissionDecision::Deny { message } if message == MSG_USER_DENIED => {
                    prop_assert_eq!(*state, ApprovalState::Denied)
                }
                PermissionDecision::Deny { message } if message == MSG_TURN_CANCELLED => {}
                other => prop_assert!(false, "{a} 的答复 {other:?}"),
            }
        }
    }
    // 9. 绑定不串：每条待审批的绑定出自同一个回合上的某个请求
    if check_bindings {
        for a in &pending_fold {
            let turn = projection.approvals[a].2;
            if let Some(k) = engine.binding_keys(*a) {
                let ok = run
                    .request_keys
                    .values()
                    .any(|(t, keys)| tid(*t) == turn && Arc::ptr_eq(keys, &k));
                prop_assert!(ok, "审批 {a} 的绑定不是它那个回合上任何一次请求带进来的");
            }
        }
    }
    Ok(())
}

/// 审批有调用记录（一次性放行只对有记录的审批发）。生成器的审批都来自引擎，都有。
fn has_record(log: &[StoredEvent], a: ApprovalId) -> bool {
    log.iter().any(|e| matches!(&e.event, DomainEvent::ApprovalRequested(r) if r.approval.id == a && r.approval.call.is_some()))
}

proptest! {
    #![proptest_config(ProptestConfig { cases: 256, failure_persistence: None, .. ProptestConfig::default() })]

    #[test]
    fn engine_interleavings(schedule in prop::collection::vec(choice(), 1..240), restart in any::<bool>()) {
        let mut run = Run::new();
        for c in schedule {
            run.apply(c);
        }
        run.wind_down();
        if restart {
            let store = run.store.clone();
            let (engine, plan) = drive(&store, Engine::start(ports(&store), Default::default(), |c, t| {
                TURNS.iter().position(|(cc, _)| cid(*cc) == c).map(|_| {
                    let n = (0..4u8).find(|n| tid(*n) == t).unwrap_or(0);
                    keys_for_turn(n, 999)
                })
            })).map_err(|e| TestCaseError::fail(e.to_string()))?;
            if !plan.is_empty() {
                run.store.seed(plan);
            }
            let e2 = engine.clone();
            drive(&store, async move { e2.catch_up().await.unwrap() });
            check(&run, &engine, false)?;
        } else {
            let engine = run.engine.clone();
            let store = run.store.clone();
            let e2 = engine.clone();
            drive(&store, async move { e2.sync().await }).map_err(|e| TestCaseError::fail(e.to_string()))?;
            check(&run, &engine, true)?;
        }
    }
}

// ---------- sync() 的接线（方案 6.10.8；方案审查 3/F-2，假存储版） ----------

/// 删除落盘留下同对话的孤儿。观察者照 T04 的契约在写线程上、回执送达之前同步调用，只叫醒；
/// 另一个被轮询的任务跑 `sync()`。删除的回执不依赖 `sync()` 跑完；任务被轮询后孤儿被作废。
/// 对照：把 `sync()` 直接写进观察者 → 观察者里碰了存储，panic（证明守卫真的在守）。
#[test]
fn observer_only_wakes_sync_runs_off_writer() {
    use std::sync::atomic::{AtomicBool, Ordering};
    let run = Run::new();
    let store = run.store.clone();
    let engine = run.engine.clone();
    // 助手 0 在对话 0 的回合 0 上有一条待审批
    let e = engine.clone();
    let mut tasks: Tasks<Decided> = Tasks::default();
    let k = Arc::new(keys_for_turn(0, 0));
    let request =
        tasks.spawn(async move { e.on_permission(&k, tid(0), &bash_req("npm test")).await });
    for _ in 0..50 {
        tasks.poll(request);
        while store.step().is_some() {}
        store.undelivered().into_iter().for_each(|t| {
            store.deliver(t);
        });
    }
    assert_eq!(run.approvals().len(), 1);
    let woken = Arc::new(AtomicBool::new(false));
    let w = woken.clone();
    store.set_observer(Box::new(move |_batch| w.store(true, Ordering::SeqCst)));
    // 绕过引擎删助手（孤儿留下）
    let s2 = store.clone();
    let deletion = tasks.spawn(async move {
        s2.commit(cmd(vec![
            AssistantDeleted {
                assistant_id: aid(0),
                name_snapshot: "a".into(),
            }
            .into(),
        ]))
        .await
        .unwrap();
        Decided {
            decision: PermissionDecision::Allow,
            approval: None,
        }
    });
    tasks.poll(deletion);
    let ticket = store.step().expect("删除入队了");
    assert!(woken.load(Ordering::SeqCst), "观察者叫醒了 sync 任务");
    store.deliver(ticket);
    tasks.poll(deletion);
    assert!(
        tasks.done.contains_key(&deletion),
        "删除的回执不依赖 sync 跑完"
    );
    let e = engine.clone();
    drive(&store, async move { e.catch_up().await.unwrap() });
    assert!(!engine.folded().orphans().is_empty(), "只追平不清孤儿");
    // 被叫醒的任务在写线程之外跑 sync
    let e = engine.clone();
    drive(&store, async move { e.sync().await }).unwrap();
    assert!(engine.folded().orphans().is_empty());
    for _ in 0..5 {
        tasks.poll(request);
    }
    assert_eq!(
        tasks.done.get(&request).map(|d| d.decision.clone()),
        Some(PermissionDecision::Deny {
            message: MSG_TURN_CANCELLED.into()
        })
    );

    // 对照：观察者里直接调 sync（读存储）→ panic
    let run = Run::new();
    let store = run.store.clone();
    let engine = run.engine.clone();
    let s = store.clone();
    store.set_observer(Box::new(move |_batch| {
        let e = engine.clone();
        let mut t: Tasks<()> = Tasks::default();
        let i = t.spawn(async move {
            let _ = e.sync().await;
        });
        t.poll(i);
        let _ = &s;
    }));
    let s3 = store.clone();
    let mut t: Tasks<()> = Tasks::default();
    let i = t.spawn(async move {
        let _ = s3
            .commit(cmd(vec![
                MessageAppended {
                    message: fold_fx::user_message(1, 1),
                }
                .into(),
            ]))
            .await;
    });
    t.poll(i);
    let panicked = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        store.step();
    }))
    .is_err();
    assert!(panicked, "观察者里碰存储必须 panic");
}
