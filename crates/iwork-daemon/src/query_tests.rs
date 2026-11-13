//! 查询、快照与订阅（T10a 方案 7.2 的 Q1、S1、S2 一行；7.3 的六种取法对拍）。
//!
//! 场景由一串随机操作驱动真存储（每个操作一笔提交，只走转换表里的边）：两个项目对话里发消息、排回合、开跑、
//! 调工具、助手回话、请求审批、决定、收尾（完成或失败）。用例：
//!
//! - `snapshot_parts_share_one_seq`：几个范围的快照各部分同一个 seq，等于返回的 head；一边写一边取，侧栏摘要的
//!   「最近一条消息」和同一快照里那一页的最后一条消息总对得上（一个读事务）。
//! - `snapshot_plus_events_equals_later_snapshot`（性质测试）：任意一个时刻的会话快照，加上之后路由到这个会话范围的
//!   事件，归约出的时间线行与最后直接取的快照逐项相等（S2）。归约器写在这里，独立于查询的实现。
//! - `six_paging_ways_match_fake_daemon`：同一份场景的事件折叠进假后台的表，六种取法与锚点查询两边逐项相等
//!   （差异只剩已登记的几种，见 [`normalize`]）。

use std::collections::BTreeMap;
use std::sync::Arc;

use iwork_app::codec::ProtoEventCodec;
use iwork_app::convert::persisted_event_to_proto;
use iwork_app::subscription::{routes_for, scope_streams};
use iwork_domain::approval::{ApprovalState, Decider};
use iwork_domain::event::{
    ApprovalDecided, ApprovalRequested, AssistantCreated, ConversationCreated, DomainEvent,
    MessageAppended, MessageCompleted, ProjectAdded, ToolCallFinished, ToolCallId, ToolCallStarted,
    TurnFinished, TurnQueued, TurnStateChanged,
};
use iwork_domain::message::{MessageBody, MessageStatus};
use iwork_domain::turn::TurnState;
use iwork_ports::store::{EventFilter, StoragePort};
use iwork_proto::pb;
use iwork_proto::pb::query::Kind as Q;
use iwork_proto::pb::query_result::Kind as RK;
use iwork_store::SqliteReadModel;
use proptest::prelude::*;
use proptest::strategy::ValueTree;

use crate::fixtures::*;
use crate::handler::query::Queries;
use crate::snapshot::ReadSnapshots;
use crate::source::SnapshotProvider;

const CONVS: [u128; 2] = [10, 11];

/// 驱动真存储的模拟：只记住推进合法状态要的东西。
struct World {
    store: Arc<Store>,
    next: u128,
    cmd: u128,
    turns: Vec<SimTurn>,
}

struct SimTurn {
    id: u128,
    conv: u128,
    state: TurnState,
    open_tools: Vec<String>,
    approval: Option<u128>,
}

impl World {
    fn new(env: &Env) -> Self {
        let store = Arc::new(env.open());
        let mut w = Self {
            store,
            next: 1000,
            cmd: 0,
            turns: Vec::new(),
        };
        let mut base: Vec<DomainEvent> = vec![
            AssistantCreated {
                assistant: assistant(1, false),
            }
            .into(),
            ProjectAdded {
                project: project(1),
            }
            .into(),
        ];
        for c in CONVS {
            base.push(
                ConversationCreated {
                    conversation: project_conversation(c, 1),
                }
                .into(),
            );
        }
        w.commit(base);
        w
    }

    fn fresh(&mut self) -> u128 {
        self.next += 1;
        self.next
    }

    fn commit(&mut self, events: Vec<DomainEvent>) {
        self.cmd += 1;
        block(self.store.commit(cmd(self.cmd, events))).expect("场景的提交走合法路径");
    }

    fn change(t: &SimTurn, to: TurnState, reason: Option<String>) -> DomainEvent {
        TurnStateChanged {
            conversation_id: Some(cid(t.conv)),
            turn_id: tid(t.id),
            from: t.state,
            to,
            reason,
        }
        .into()
    }

    /// 执行一个操作；当前状态下做不了的就跳过。`a`、`b` 选对象。
    fn step(&mut self, op: u8, a: u8, b: u8) {
        let conv = CONVS[usize::from(a) % CONVS.len()];
        let pick = |w: &Self, want: fn(&SimTurn) -> bool| -> Option<usize> {
            let found: Vec<usize> = (0..w.turns.len()).filter(|i| want(&w.turns[*i])).collect();
            (!found.is_empty()).then(|| found[usize::from(b) % found.len()])
        };
        match op % OPS {
            0 => {
                let n = self.fresh();
                let text = format!("问题 {n}\n第二行");
                self.commit(vec![
                    MessageAppended {
                        message: user_message(n, conv, &text),
                    }
                    .into(),
                ]);
            }
            1 => {
                let n = self.fresh();
                self.commit(vec![
                    TurnQueued {
                        turn: turn(n, conv),
                    }
                    .into(),
                ]);
                self.turns.push(SimTurn {
                    id: n,
                    conv,
                    state: TurnState::Queued,
                    open_tools: Vec::new(),
                    approval: None,
                });
            }
            2 => {
                if let Some(i) = pick(self, |t| t.state == TurnState::Queued) {
                    let e = Self::change(&self.turns[i], TurnState::Running, None);
                    self.commit(vec![e]);
                    self.turns[i].state = TurnState::Running;
                }
            }
            3 => {
                if let Some(i) = pick(self, |t| t.state == TurnState::Running) {
                    let call = format!("call-{}", self.fresh());
                    let t = &self.turns[i];
                    let e = ToolCallStarted {
                        conversation_id: Some(cid(t.conv)),
                        turn_id: tid(t.id),
                        tool_call_id: ToolCallId(call.clone()),
                        tool_name: "Bash".into(),
                        action: approval(1, t.id, t.conv).action,
                    };
                    self.commit(vec![e.into()]);
                    self.turns[i].open_tools.push(call);
                }
            }
            // 只从还有开着的工具调用的回合里挑：随便挑一个再看有没有，多数时候落空，S2 就几乎碰不到结束事件
            4 => {
                if let Some(i) = pick(self, |t| !t.open_tools.is_empty()) {
                    let call = self.turns[i].open_tools.remove(0);
                    let t = &self.turns[i];
                    let e = ToolCallFinished {
                        conversation_id: Some(cid(t.conv)),
                        turn_id: tid(t.id),
                        tool_call_id: ToolCallId(call),
                        ok: b.is_multiple_of(2),
                        output_artifact: None,
                    };
                    self.commit(vec![e.into()]);
                }
            }
            5 => {
                if let Some(i) = pick(self, |t| t.state == TurnState::Running) {
                    let n = self.fresh();
                    let t = &self.turns[i];
                    let mut m = assistant_message(n, t.conv, t.id);
                    m.created_at = iwork_domain::common::Timestamp(T0);
                    let completed = MessageCompleted {
                        conversation_id: cid(t.conv),
                        message_id: mid(n),
                        body: MessageBody::Inline(format!("回答 {n}")),
                        status: MessageStatus::Completed,
                    };
                    self.commit(vec![
                        MessageAppended { message: m }.into(),
                        completed.into(),
                    ]);
                }
            }
            6 => {
                if let Some(i) = pick(self, |t| t.state == TurnState::Running) {
                    let n = self.fresh();
                    let t = &self.turns[i];
                    let e = vec![
                        ApprovalRequested {
                            approval: approval(n, t.id, t.conv),
                        }
                        .into(),
                        Self::change(t, TurnState::WaitingApproval, None),
                    ];
                    self.commit(e);
                    self.turns[i].state = TurnState::WaitingApproval;
                    self.turns[i].approval = Some(n);
                }
            }
            7 => {
                if let Some(i) = pick(self, |t| t.state == TurnState::WaitingApproval) {
                    let t = &self.turns[i];
                    let state = if b.is_multiple_of(2) {
                        ApprovalState::Approved {
                            decider: Decider::User,
                        }
                    } else {
                        ApprovalState::Denied
                    };
                    let e = vec![
                        ApprovalDecided {
                            conversation_id: Some(cid(t.conv)),
                            approval_id: apid(t.approval.expect("等审批的回合有审批")),
                            state,
                            remember: None,
                        }
                        .into(),
                        Self::change(t, TurnState::Running, None),
                    ];
                    self.commit(e);
                    self.turns[i].state = TurnState::Running;
                    self.turns[i].approval = None;
                }
            }
            8 => {
                if let Some(i) = pick(self, |t| t.state == TurnState::Running) {
                    let t = &self.turns[i];
                    let (to, reason) = if b.is_multiple_of(3) {
                        (TurnState::Failed, Some(format!("失败 {}", t.id)))
                    } else {
                        (TurnState::Completed, None)
                    };
                    let e = vec![
                        Self::change(t, to, reason),
                        TurnFinished {
                            conversation_id: Some(cid(t.conv)),
                            turn_id: tid(t.id),
                            state: to,
                            usage: iwork_domain::common::Usage {
                                input_tokens: 10,
                                output_tokens: u64::from(b),
                                ..Default::default()
                            },
                            celebrate: b.is_multiple_of(2),
                            change_summary: None,
                        }
                        .into(),
                    ];
                    self.commit(e);
                    self.turns[i].state = to;
                }
            }
            // 跑着的回合中断（进程丢失）：时间线上出继续卡片，没有结束行
            9 => {
                if let Some(i) = pick(self, |t| t.state == TurnState::Running) {
                    let to = TurnState::Interrupted {
                        reason: iwork_domain::turn::InterruptReason::Crash,
                    };
                    let e = Self::change(&self.turns[i], to, None);
                    self.commit(vec![e]);
                    self.turns[i].state = to;
                }
            }
            // 中断的回合续接，或者放弃（收成已取消，有结束事件）
            _ => {
                if let Some(i) = pick(self, |t| matches!(t.state, TurnState::Interrupted { .. })) {
                    let t = &self.turns[i];
                    if b.is_multiple_of(2) {
                        let e = Self::change(t, TurnState::Running, None);
                        self.commit(vec![e]);
                        self.turns[i].state = TurnState::Running;
                    } else {
                        let e = vec![
                            Self::change(t, TurnState::Cancelled, None),
                            TurnFinished {
                                conversation_id: Some(cid(t.conv)),
                                turn_id: tid(t.id),
                                state: TurnState::Cancelled,
                                usage: Default::default(),
                                celebrate: false,
                                change_summary: None,
                            }
                            .into(),
                        ];
                        self.commit(e);
                        self.turns[i].state = TurnState::Cancelled;
                    }
                }
            }
        }
    }
}

impl World {
    /// 把还在跑的回合都中断掉（进程丢失）：场景结束时留下停在中断的回合。
    fn interrupt_running(&mut self) {
        let to = TurnState::Interrupted {
            reason: iwork_domain::turn::InterruptReason::Crash,
        };
        for i in 0..self.turns.len() {
            if self.turns[i].state == TurnState::Running {
                let e = Self::change(&self.turns[i], to, None);
                self.commit(vec![e]);
                self.turns[i].state = to;
            }
        }
    }
}

/// 操作的种类数（`World::step` 的 `match`）。
const OPS: u8 = 11;

fn read(store: &Store) -> Arc<SqliteReadModel<ProtoEventCodec>> {
    Arc::new(read_model(store))
}

fn conv_scope(c: u128) -> pb::SubscriptionScope {
    pb::SubscriptionScope {
        kind: Some(pb::subscription_scope::Kind::Conversation(
            pb::ConversationScope {
                conversation_id: cid(c).to_string(),
            },
        )),
    }
}

fn scope(kind: pb::subscription_scope::Kind) -> pb::SubscriptionScope {
    pb::SubscriptionScope { kind: Some(kind) }
}

// ---------- S1 ----------

#[test]
fn snapshot_parts_share_one_seq() {
    let env = Env::new();
    let mut w = World::new(&env);
    for i in 0..20u8 {
        w.step(i % OPS, i, i / 3);
    }
    let snaps = ReadSnapshots::new(read(&w.store));
    let scopes = [
        scope(pb::subscription_scope::Kind::Sidebar(pb::SidebarScope {})),
        conv_scope(10),
        scope(pb::subscription_scope::Kind::Pending(pb::PendingScope {})),
        scope(pb::subscription_scope::Kind::Settings(pb::SettingsScope {})),
    ];
    let (snap, head) = block(snaps.snapshot(&scopes)).unwrap();
    assert_eq!(head, block(w.store.head_seq()).unwrap().0);
    // 侧栏 3 条 + 会话 2 条 + 待处理 1 条 + 设置 1 条
    assert_eq!(snap.results.len(), 7, "对照：每个查询都在");
    for r in &snap.results {
        assert_eq!(
            r.seq,
            head,
            "{:?}",
            r.kind.as_ref().map(std::mem::discriminant)
        );
    }

    // 一边写一边取：快照里侧栏摘要的「最近一条消息首行」和同一快照里对话 10 那一页的最后一条消息总相同。
    // 分开几个读事务取的话，写入夹在中间时两者会错开。
    let store = w.store.clone();
    let writer = std::thread::spawn(move || {
        for k in 0..300u128 {
            block(store.commit(cmd(
                100_000 + k,
                vec![
                MessageAppended {
                    message: user_message(500_000 + k, 10, &format!("并发 {k}")),
                }
                .into(),
            ],
            )))
            .unwrap();
        }
    });
    let scopes = [
        scope(pb::subscription_scope::Kind::Sidebar(pb::SidebarScope {})),
        conv_scope(10),
    ];
    let mut checked = 0;
    while !writer.is_finished() || checked < 5 {
        let (snap, head) = block(snaps.snapshot(&scopes)).unwrap();
        let mut sidebar_line = None;
        let mut page_last = None;
        for r in &snap.results {
            assert_eq!(r.seq, head);
            match &r.kind {
                Some(RK::ListConversations(l)) => {
                    sidebar_line = l
                        .conversations
                        .iter()
                        .find(|c| c.id == cid(10).to_string())
                        .map(|c| c.last_message_first_line.clone());
                }
                Some(RK::GetConversation(g)) => {
                    page_last = g.messages.last().map(|m| {
                        match m.body.as_ref().and_then(|b| b.content.as_ref()) {
                            Some(pb::message_body::Content::InlineText(t)) => {
                                t.lines().next().unwrap_or("").to_owned()
                            }
                            _ => String::new(),
                        }
                    });
                }
                _ => {}
            }
        }
        assert_eq!(
            sidebar_line, page_last,
            "同一快照里两处对不上（head {head}）"
        );
        checked += 1;
    }
    writer.join().unwrap();
    assert!(checked >= 5);
}

/// Q1：单条视图查询的 `seq` 是它那个读事务看到的 head；查询不提交（head 不动）。
#[test]
fn query_seq_is_transaction_head() {
    let env = Env::new();
    let mut w = World::new(&env);
    for i in 0..12u8 {
        w.step(i % OPS, i, i);
    }
    let head = block(w.store.head_seq()).unwrap().0;
    let q = Queries::new(read(&w.store));
    for kind in [
        get(10, |_| {}),
        Q::ListPendingItems(pb::ListPendingItems {}),
        Q::ListConversations(pb::ListConversations::default()),
        Q::ListAnchors(pb::ListAnchors {
            conversation_id: cid(10).to_string(),
            around_seq: 0,
            before: 5,
            after: 5,
        }),
    ] {
        assert_eq!(real_answer(&q, kind.clone()).unwrap().seq, head, "{kind:?}");
    }
    assert_eq!(block(w.store.head_seq()).unwrap().0, head, "查询不提交");
    // 没接的查询回「未接入」
    let e = real_answer(&q, Q::ListModels(pb::ListModels::default())).unwrap_err();
    assert_eq!(e.code, i32::from(pb::ErrorCode::Unavailable));
}

// ---------- S2 ----------

/// 时间线上的一行：排序字段与它现在的样子。独立于查询实现，只看协议字段与领域事件。
type Rows = BTreeMap<String, (u64, String)>;

/// 快照里的会话页 → 行（取的是最新一页、页长足够大，所以就是整个对话）。
fn rows_of_page(g: &pb::GetConversationResult) -> Rows {
    let mut rows = Rows::new();
    for m in &g.messages {
        let text = match m.body.as_ref().and_then(|b| b.content.as_ref()) {
            Some(pb::message_body::Content::InlineText(t)) => t.clone(),
            _ => String::new(),
        };
        rows.insert(
            format!("m:{}", m.id),
            (m.created_seq, format!("{} {text}", m.status)),
        );
    }
    for c in &g.tool_calls {
        rows.insert(
            format!("t:{}/{}", c.turn_id, c.tool_call_id),
            (
                c.created_seq,
                format!("{} {}", c.finished, c.finished && c.ok),
            ),
        );
    }
    for a in &g.approvals {
        rows.insert(format!("a:{}", a.id), (a.created_seq, a.state.to_string()));
    }
    for t in &g.turns {
        rows.insert(
            format!("turn:{}", t.id),
            (t.created_seq, t.state.to_string()),
        );
        if t.ended_seq != 0 {
            rows.insert(format!("end:{}", t.id), (t.ended_seq, t.state.to_string()));
        }
    }
    rows
}

/// 一条事件落到行上（客户端时间线的规则：创建事件建行，变化事件改已有的行）。
fn apply(rows: &mut Rows, seq: u64, e: &DomainEvent) {
    use pb::ApprovalState as PA;
    let status = |s: MessageStatus| iwork_app::convert::message_status_to_proto(s);
    match e {
        DomainEvent::MessageAppended(x) => {
            let text = match &x.message.body {
                MessageBody::Inline(t) => t.clone(),
                MessageBody::Artifact(_) => String::new(),
            };
            rows.insert(
                format!("m:{}", x.message.id),
                (seq, format!("{} {text}", status(x.message.status))),
            );
        }
        DomainEvent::MessageCompleted(x) => {
            // 客户端的规则：找不到目标行就丢掉（行在页外）
            let Some(row) = rows.get_mut(&format!("m:{}", x.message_id)) else {
                return;
            };
            let text = match &x.body {
                MessageBody::Inline(t) => t.clone(),
                MessageBody::Artifact(_) => String::new(),
            };
            row.1 = format!("{} {text}", status(x.status));
        }
        DomainEvent::ToolCallStarted(x) => {
            rows.insert(
                format!("t:{}/{}", x.turn_id, x.tool_call_id.0),
                (seq, "false false".into()),
            );
        }
        DomainEvent::ToolCallFinished(x) => {
            if let Some(row) = rows.get_mut(&format!("t:{}/{}", x.turn_id, x.tool_call_id.0)) {
                row.1 = format!("true {}", x.ok);
            }
        }
        DomainEvent::ApprovalRequested(x) => {
            rows.insert(
                format!("a:{}", x.approval.id),
                (seq, (PA::Pending as i32).to_string()),
            );
        }
        DomainEvent::ApprovalDecided(x) => {
            let s = match x.state {
                ApprovalState::Approved { .. } => PA::Approved,
                _ => PA::Denied,
            };
            if let Some(row) = rows.get_mut(&format!("a:{}", x.approval_id)) {
                row.1 = (s as i32).to_string();
            }
        }
        DomainEvent::TurnQueued(x) => {
            rows.insert(
                format!("turn:{}", x.turn.id),
                (
                    seq,
                    iwork_app::convert::turn_state_to_proto(x.turn.state)
                        .0
                        .to_string(),
                ),
            );
        }
        DomainEvent::TurnStateChanged(x) => {
            let to = iwork_app::convert::turn_state_to_proto(x.to).0.to_string();
            if let Some(row) = rows.get_mut(&format!("turn:{}", x.turn_id)) {
                row.1 = to.clone();
            }
            if let Some(end) = rows.get_mut(&format!("end:{}", x.turn_id)) {
                end.1 = to;
            }
        }
        DomainEvent::TurnFinished(x) => {
            let s = iwork_app::convert::turn_state_to_proto(x.state)
                .0
                .to_string();
            rows.insert(format!("end:{}", x.turn_id), (seq, s));
        }
        _ => {}
    }
}

fn conversation_page(snap: &pb::Snapshot) -> &pb::GetConversationResult {
    snap.results
        .iter()
        .find_map(|r| match &r.kind {
            Some(RK::GetConversation(g)) => Some(g),
            _ => None,
        })
        .expect("会话范围的快照有一页")
}

proptest! {
    #![proptest_config(ProptestConfig { cases: 48, ..ProptestConfig::default() })]

    /// S2：任意时刻的会话快照 + 之后路由到会话范围的事件 = 最后直接取的快照。
    #[test]
    fn snapshot_plus_events_equals_later_snapshot(
        ops in proptest::collection::vec((0..OPS, any::<u8>(), any::<u8>()), 1..40),
        cut in 0usize..40,
        which in 0usize..2,
    ) {
        let env = Env::new();
        let mut w = World::new(&env);
        let snaps = ReadSnapshots::new(read(&w.store));
        let c = CONVS[which];
        let scope = [conv_scope(c)];
        let keys = scope_streams(&scope[0]);
        let mut early = None;
        for (i, (op, a, b)) in ops.iter().enumerate() {
            if i == cut.min(ops.len() - 1) {
                early = Some(block(snaps.snapshot(&scope)).unwrap());
            }
            w.step(*op, *a, *b);
        }
        let (early, s1) = early.unwrap();
        let (late, s2) = block(snaps.snapshot(&scope)).unwrap();
        let mut rows = rows_of_page(conversation_page(&early));
        let events = block(w.store.read_events(EventFilter::All, iwork_domain::id::Seq(s1), 100_000)).unwrap();
        for e in events.iter().filter(|e| e.seq.0 <= s2) {
            let routes = routes_for(&e.event);
            if keys.iter().any(|k| routes.iter().any(|r| k.covers(r))) {
                apply(&mut rows, e.seq.0, &e.event);
            }
        }
        prop_assert_eq!(rows, rows_of_page(conversation_page(&late)));
    }
}

// ---------- 对拍假后台 ----------

/// 假后台和真后台已登记的差异抹平（回报里逐条说明）：
/// - 时间戳：假后台全用信封时间造，真后台用各实体自己的时间；
/// - 列表顺序：假后台按 id，真后台按排序字段；两边都按身份排序后比；
/// - 还在跑的回合的状态说明：真后台只给停下来的回合取（见 `views.rs` 的 `state_reason`）；
/// - 未读数：真存储的投影按「发给用户的消息、待处理卡片」两类事件累加（中断进入时计一次，之后续接了也不减），
///   假后台按现在还待处理的项重算。技术设计 9.1 的口径是真存储这一种：假后台口径待改，见 STATUS
///   （supervisor 2026-11-13 定，另排小卡在 T09 组 3 合入后改假后台并重录受影响的录像）。
fn normalize(mut g: pb::GetConversationResult) -> pb::GetConversationResult {
    if let Some(c) = g.conversation.as_mut() {
        c.updated_at = None;
        c.unread = 0;
    }
    for m in &mut g.messages {
        m.created_at = None;
    }
    for t in &mut g.turns {
        t.started_at = None;
        t.ended_at = None;
        if t.ended_seq == 0 {
            t.state_reason = None;
        }
    }
    for a in &mut g.approvals {
        a.requested_at = None;
        a.decided_at = None;
    }
    g.turns.sort_by(|a, b| a.id.cmp(&b.id));
    g.tool_calls
        .sort_by(|a, b| (&a.turn_id, &a.tool_call_id).cmp(&(&b.turn_id, &b.tool_call_id)));
    g.approvals.sort_by(|a, b| a.id.cmp(&b.id));
    g.user_actions.sort_by(|a, b| a.id.cmp(&b.id));
    g
}

fn fake_tables(store: &Store) -> (iwork_fake_daemon::Tables, u64) {
    let mut tables = iwork_fake_daemon::Tables::default();
    let events =
        block(store.read_events(EventFilter::All, iwork_domain::id::Seq(0), 1_000_000)).unwrap();
    let mut head = 0;
    for e in &events {
        let p = persisted_event_to_proto(e).unwrap();
        let inner = p.event.unwrap().event.unwrap();
        tables.fold(e.seq.0, &iwork_fake_daemon::envelope_time(e.seq.0), &inner);
        head = e.seq.0;
    }
    (tables, head)
}

fn real_answer(q: &Queries<ProtoEventCodec>, kind: Q) -> Result<pb::QueryResult, pb::Error> {
    block(q.answer(pb::Query { kind: Some(kind) }))
}

fn get(conv: u128, f: impl FnOnce(&mut pb::GetConversation)) -> Q {
    let mut g = pb::GetConversation {
        conversation_id: cid(conv).to_string(),
        limit: 4,
        ..Default::default()
    };
    f(&mut g);
    Q::GetConversation(g)
}

/// 六种取法、锚点：同一份场景上真后台与假后台逐项相等（7.3；T09 第 3 节要点 ⑤）。场景里有中断以外的各种回合，
/// 行级游标落在每一行的排序字段上与它两侧。
#[test]
fn six_paging_ways_match_fake_daemon() {
    let env = Env::new();
    let mut w = World::new(&env);
    let mut runner = proptest::test_runner::TestRunner::deterministic();
    let ops = proptest::collection::vec((0..OPS, any::<u8>(), any::<u8>()), 120)
        .new_tree(&mut runner)
        .unwrap()
        .current();
    for (op, a, b) in ops {
        w.step(op, a, b);
    }
    w.interrupt_running();
    // 对照：场景里有停在中断的回合（没有结束行、出继续卡片），也有放弃后收成已取消的、失败的
    let count = |want: fn(TurnState) -> bool| w.turns.iter().filter(|t| want(t.state)).count();
    assert!(
        count(|s| matches!(s, TurnState::Interrupted { .. })) > 0,
        "场景里没有中断的回合"
    );
    assert!(
        count(|s| s == TurnState::Cancelled) > 0,
        "场景里没有放弃的回合"
    );
    assert!(
        count(|s| s == TurnState::Failed) > 0,
        "场景里没有失败的回合"
    );
    let (tables, head) = fake_tables(&w.store);
    let queries = Queries::new(read(&w.store));
    let mut compared = 0;
    for c in CONVS {
        let full = match real_answer(&queries, get(c, |g| g.limit = 1000))
            .unwrap()
            .kind
        {
            Some(RK::GetConversation(g)) => g,
            other => panic!("{other:?}"),
        };
        let messages: Vec<String> = full.messages.iter().map(|m| m.id.clone()).collect();
        let mut seqs: Vec<u64> = rows_of_page(&full).values().map(|v| v.0).collect();
        seqs.sort_unstable();
        seqs.dedup();
        let mut kinds: Vec<Q> = vec![get(c, |_| {}), get(c, |g| g.limit = 0)];
        // 往后翻到最后一条之后是一页空的：真后台不带任何实体，假后台带上整个对话（已登记的差异，单独一条用例）
        for (i, m) in messages.iter().enumerate() {
            kinds.push(get(c, |g| g.before_message_id = Some(m.clone())));
            if i + 1 < messages.len() {
                kinds.push(get(c, |g| g.after_message_id = Some(m.clone())));
            }
        }
        for s in seqs.iter().flat_map(|s| [s - 1, *s, s + 1]) {
            kinds.push(get(c, |g| g.around_seq = Some(s)));
            if s > 0 {
                kinds.push(get(c, |g| g.before_seq = Some(s)));
                kinds.push(get(c, |g| g.after_seq = Some(s)));
            }
            kinds.push(Q::ListAnchors(pb::ListAnchors {
                conversation_id: cid(c).to_string(),
                around_seq: s,
                before: 3,
                after: 2,
            }));
        }
        kinds.push(Q::ListAnchors(pb::ListAnchors {
            conversation_id: cid(c).to_string(),
            around_seq: 0,
            before: 60,
            after: 60,
        }));
        for kind in kinds {
            let fake = tables.query(
                head,
                &pb::Query {
                    kind: Some(kind.clone()),
                },
            );
            let real = real_answer(&queries, kind.clone());
            match (real, fake) {
                (Ok(r), Ok(f)) => {
                    assert_eq!(r.seq, f.seq);
                    match (r.kind, f.kind) {
                        (Some(RK::GetConversation(r)), Some(RK::GetConversation(f))) => {
                            assert_eq!(normalize(r), normalize(f), "{kind:?}");
                        }
                        (Some(RK::ListAnchors(mut r)), Some(RK::ListAnchors(mut f))) => {
                            for a in r.anchors.iter_mut().chain(f.anchors.iter_mut()) {
                                a.at = None;
                            }
                            assert_eq!(r, f, "{kind:?}");
                        }
                        other => panic!("{kind:?}：{other:?}"),
                    }
                }
                (Err(r), Err(f)) => assert_eq!(r.code, f.code, "{kind:?}"),
                (r, f) => panic!("{kind:?}：真 {r:?} 假 {f:?}"),
            }
            compared += 1;
        }
    }
    assert!(compared > 50, "对照：比了 {compared} 次");
}

/// 已登记的差异：往后翻到最后一条消息之后，这一页是空的；真后台不带任何实体（游标之前的行归游标所在那一页），
/// 假后台带上整个对话的回合、工具调用、审批。两边都说 `has_more`、没有 `has_newer`。
#[test]
fn after_last_message_page_is_empty() {
    let env = Env::new();
    let mut w = World::new(&env);
    for (op, a, b) in [(0, 0, 0), (1, 0, 0), (2, 0, 0), (3, 0, 0), (0, 0, 0)] {
        w.step(op, a, b);
    }
    let queries = Queries::new(read(&w.store));
    let full = match real_answer(&queries, get(10, |g| g.limit = 1000))
        .unwrap()
        .kind
    {
        Some(RK::GetConversation(g)) => g,
        other => panic!("{other:?}"),
    };
    assert!(!full.tool_calls.is_empty(), "对照：对话里有工具调用");
    let last = full.messages.last().unwrap().id.clone();
    let page = match real_answer(&queries, get(10, |g| g.after_message_id = Some(last)))
        .unwrap()
        .kind
    {
        Some(RK::GetConversation(g)) => g,
        other => panic!("{other:?}"),
    };
    assert!(page.messages.is_empty() && page.turns.is_empty() && page.tool_calls.is_empty());
    assert!(page.has_more && !page.has_newer);
}
