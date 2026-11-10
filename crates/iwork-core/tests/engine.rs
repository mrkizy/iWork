//! 审批引擎（T07 第 6 步；验收 1b 作废与强制询问、2 档位、3 拒绝记忆、6 AI 审查、6c、7 审计、
//! T06 卡第 3 条一次性放行、崩溃恢复）。

mod support;

use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::Duration;

use ApprovalMode::{Auto, FullAccess, Manual};
use iwork_core::approval::engine::{
    ApprovalEngine, DecideError, EnginePorts, GrantOnce, MSG_TURN_CANCELLED, MSG_USER_DENIED,
    MSG_USER_NOT_EXECUTED, ModeError, Scope, SessionDecider, SessionKeys, UserDecision,
};
use iwork_core::approval::session::tool_set_for;
use iwork_core::turn::PermissionDecider;
use iwork_domain::approval::{
    AbandonReason, Approval, ApprovalKind, ApprovalState, Decider, ReviewState, VoidReason,
};
use iwork_domain::approval_mode::{ApprovalMode, ProjectDefaultMode, RememberScope};
use iwork_domain::common::{Actor, ModelId, ModelSelection, Timestamp, ToolGroups};
use iwork_domain::event::{
    ApprovalModeTarget, AssistantCreated, AssistantDeleted, ConversationCreated,
    ConversationDeleting, DomainEvent, MessageAppended, ProjectAdded, TurnQueued, TurnStateChanged,
};
use iwork_domain::id::{ApprovalId, AssistantId, ConversationId, MessageId, RawUlid, Seq, TurnId};
use iwork_domain::message::{Author, Message, MessageBody, MessageStatus};
use iwork_domain::project::AiReviewSettings;
use iwork_domain::rule::{RuleEntry, Verdict};
use iwork_domain::turn::{InterruptReason, TurnKind, TurnState};
use iwork_domain::user_action::{ExitStatus, UserActionState};
use iwork_ports::agent::{PermissionDecision, PermissionRequest};
use support::engine_fakes::*;
use support::fake_fs::FakeFs;
use support::fold_fx;
use support::*;

type Engine = ApprovalEngine<TickClock, Ids, MemStore, FakeReviewer, FakeTimer, FakeFs>;
type Decider_ = SessionDecider<TickClock, Ids, MemStore, FakeReviewer, FakeTimer, FakeFs>;

struct Bench {
    engine: Arc<Engine>,
    store: MemStore,
    reviewer: FakeReviewer,
    timer: FakeTimer,
    fs: FakeFs,
}

fn turn(n: u128) -> TurnId {
    TurnId::from_raw(RawUlid::from_u128(0x500 + n))
}

/// 测试里用到的另一个助手。
fn other_assistant() -> AssistantId {
    AssistantId::from_raw(RawUlid::from_u128(0x22))
}

fn keys_with(lists: &[RuleEntry], assistant: AssistantId) -> SessionKeys {
    SessionKeys {
        conversation: conversation(),
        assistant,
        runtime_id: iwork_domain::id::RuntimeId::from_raw(RawUlid::from_u128(0x888)),
        assistant_version: 7,
        project: Some(project()),
        turn_kind: TurnKind::Assistant,
        snapshot: Arc::new(snapshot_on(Manual, lists, &env(), Seq(7), &fs())),
        tool_set: tool_set_for(TurnKind::Assistant, &ToolGroups::new()),
        disabled_groups: ToolGroups::new(),
        cwd: cp(WS),
        paths: env(),
        ai_review: AiReviewSettings {
            enabled: true,
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

fn keys() -> SessionKeys {
    keys_with(&[], assistant())
}

/// 布景：项目（手动默认）、两个助手、项目对话（对话级档位 `mode`）。
fn world(mode: Option<ApprovalMode>) -> Vec<DomainEvent> {
    let mut p = fold_fx::project(1, ProjectDefaultMode::Manual);
    p.id = project();
    let mut a = fold_fx::assistant(1);
    a.id = assistant();
    let mut o = fold_fx::assistant(2);
    o.id = other_assistant();
    let mut c = fold_fx::project_conversation(1, 1);
    c.id = conversation();
    c.kind = iwork_domain::conversation::ConversationKind::Project {
        project_id: project(),
    };
    c.approval_mode = mode;
    vec![
        ProjectAdded { project: p }.into(),
        AssistantCreated { assistant: a }.into(),
        AssistantCreated { assistant: o }.into(),
        ConversationCreated { conversation: c }.into(),
    ]
}

fn ports(
    store: &MemStore,
    reviewer: &FakeReviewer,
    timer: &FakeTimer,
    fs: FakeFs,
) -> EnginePorts<TickClock, Ids, MemStore, FakeReviewer, FakeTimer, FakeFs> {
    EnginePorts {
        clock: TickClock::default(),
        ids: Ids::default(),
        store: store.clone(),
        reviewer: reviewer.clone(),
        timer: timer.clone(),
        fs,
    }
}

impl Bench {
    async fn new(mode: Option<ApprovalMode>) -> Self {
        Self::with_fs(mode, fs()).await
    }

    async fn with_fs(mode: Option<ApprovalMode>, fs: FakeFs) -> Self {
        let store = MemStore::default();
        store.seed(world(mode));
        let reviewer = FakeReviewer::default();
        let timer = FakeTimer::default();
        let (engine, plan) = Engine::start(
            ports(&store, &reviewer, &timer, fs.clone()),
            Default::default(),
            |_, _| None,
        )
        .await
        .unwrap();
        assert!(plan.is_empty());
        Self {
            engine,
            store,
            reviewer,
            timer,
            fs,
        }
    }

    /// 后台重启：用同一份日志（拷进一个新存储）`start` 一个新引擎，提交它的恢复计划。
    /// 新引擎有自己的审查运行时与定时器；旧引擎留在旧存储上，互不影响。返回恢复计划。
    async fn restart(
        &self,
        keys_for: impl Fn(ConversationId, TurnId) -> Option<SessionKeys>,
    ) -> (Self, Vec<DomainEvent>) {
        let store = MemStore::default();
        store.seed(self.store.all());
        let reviewer = FakeReviewer::default();
        let timer = FakeTimer::default();
        let p = ports(&store, &reviewer, &timer, self.fs.clone());
        // 新引擎的 id 不能和旧日志里的撞：先推进计数器
        for _ in 0..10_000 {
            iwork_ports::clock::IdGenerator::next(&p.ids);
        }
        let (engine, plan) = Engine::start(p, Default::default(), keys_for)
            .await
            .unwrap();
        if !plan.is_empty() {
            store.seed(plan.clone());
            engine.sync().await.unwrap();
        }
        (
            Self {
                engine,
                store,
                reviewer,
                timer,
                fs: self.fs.clone(),
            },
            plan,
        )
    }

    fn decider(&self) -> Arc<Decider_> {
        Arc::new(self.engine.decider_for(keys()))
    }

    /// 回合最近一次落盘的状态（没有状态变化记录时是 `Queued`）；没登记过返回 `None`。
    fn turn_state(&self, t: TurnId) -> Option<TurnState> {
        let events = self.store.all();
        let queued = events
            .iter()
            .any(|e| matches!(e, DomainEvent::TurnQueued(q) if q.turn.id == t));
        if !queued {
            return None;
        }
        Some(
            events
                .iter()
                .rev()
                .find_map(|e| match e {
                    DomainEvent::TurnStateChanged(c) if c.turn_id == t => Some(c.to),
                    _ => None,
                })
                .unwrap_or(TurnState::Queued),
        )
    }

    /// 扮演编排：回合 `t`（属于 `assistant`）登记过并处于等审批（`PermissionAsked` 已落盘）。
    fn waiting(&self, t: TurnId, assistant: AssistantId) {
        let mut events = Vec::new();
        let from = match self.turn_state(t) {
            Some(state) => state,
            None => {
                let mut turn = fold_fx::turn(1, 1, 1);
                turn.id = t;
                turn.conversation_id = conversation();
                turn.assistant_id = assistant;
                events.push(TurnQueued { turn }.into());
                TurnState::Queued
            }
        };
        if from != TurnState::WaitingApproval {
            events.push(state_changed(t, from, TurnState::WaitingApproval));
        }
        if !events.is_empty() {
            self.store.seed(events);
        }
    }

    /// 扮演编排：等待上限到时，回合中断（不作废审批）。
    async fn interrupt(&self, t: TurnId) {
        let from = self.turn_state(t).expect("回合已登记");
        self.store.seed(vec![state_changed(
            t,
            from,
            TurnState::Interrupted {
                reason: InterruptReason::ApprovalWaitLimit,
            },
        )]);
        self.engine.sync().await.unwrap();
    }

    /// 扮演编排：回合以 `reason` 结束等待，作废计划与回合状态同一笔提交。返回作废事件。
    async fn void_turn(&self, d: &Decider_, t: TurnId, reason: VoidReason) -> Vec<DomainEvent> {
        let events = d.void_pending(t, reason).await;
        let from = self.turn_state(t).expect("回合已登记");
        let mut all = vec![state_changed(t, from, TurnState::Cancelling)];
        all.extend(events.iter().cloned());
        self.store.seed(all);
        d.committed(&events).await;
        events
    }

    /// 在后台发起一次请求，返回答复句柄。
    fn ask(
        &self,
        d: &Arc<Decider_>,
        t: TurnId,
        req: PermissionRequest,
    ) -> tokio::task::JoinHandle<PermissionDecision> {
        self.waiting(t, d.keys().assistant);
        let d = d.clone();
        tokio::spawn(async move { d.decide(t, &req).await })
    }

    async fn call(
        &self,
        d: &Arc<Decider_>,
        t: TurnId,
        req: PermissionRequest,
    ) -> PermissionDecision {
        self.waiting(t, d.keys().assistant);
        d.decide(t, &req).await
    }

    /// 等第 `n` 个（从 0 数）审批请求出现。
    async fn approval(&self, n: usize) -> ApprovalId {
        for _ in 0..400 {
            let ids: Vec<_> = self
                .store
                .all()
                .into_iter()
                .filter_map(|e| match e {
                    DomainEvent::ApprovalRequested(r) => Some(r.approval.id),
                    _ => None,
                })
                .collect();
            if let Some(id) = ids.get(n) {
                return *id;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        panic!("第 {n} 个审批没有出现：{:#?}", self.store.all());
    }

    /// 追平后的待审批。
    async fn pending(&self, id: ApprovalId) -> Option<Approval> {
        self.engine.catch_up().await.unwrap();
        self.engine.pending_approval(id)
    }

    async fn grant(&self, id: ApprovalId) -> Option<GrantOnce> {
        self.engine.catch_up().await.unwrap();
        self.engine.grant(id)
    }

    async fn mode(&self, c: ConversationId, a: AssistantId) -> ApprovalMode {
        self.engine.catch_up().await.unwrap();
        self.engine.effective_mode(c, a)
    }

    /// 别的写者提交（扮演 T10 等），然后提醒引擎。
    async fn external(&self, events: Vec<DomainEvent>) {
        self.store.seed(events);
        self.engine.sync().await.unwrap();
    }

    async fn user_message(&self) {
        let m = Message {
            id: MessageId::from_raw(RawUlid::from_u128(0x77)),
            conversation_id: conversation(),
            author: Author::User,
            context_project_id: Some(project()),
            status: MessageStatus::Completed,
            body: MessageBody::Inline("继续".into()),
            attachments: Vec::new(),
            created_at: Timestamp(1),
            turn_id: None,
        };
        self.external(vec![MessageAppended { message: m }.into()])
            .await;
    }

    fn count(&self, pred: impl Fn(&DomainEvent) -> bool) -> usize {
        self.store.all().iter().filter(|e| pred(e)).count()
    }
}

fn card_state(card: iwork_domain::id::UserActionId, state: UserActionState) -> DomainEvent {
    iwork_domain::event::UserActionStateChanged {
        card_id: card,
        state,
        output_artifact: None,
    }
    .into()
}

/// T10 交给 `commit_with` 的一笔命令。
fn command(events: Vec<DomainEvent>) -> iwork_ports::store::CommitRequest {
    iwork_ports::store::CommitRequest {
        command_id: iwork_domain::id::CommandId::from_raw(RawUlid::from_u128(0x7777)),
        actor: user(),
        events,
        outbox: Vec::new(),
    }
}

fn state_changed(t: TurnId, from: TurnState, to: TurnState) -> DomainEvent {
    TurnStateChanged {
        turn_id: t,
        from,
        to,
        reason: None,
    }
    .into()
}

fn user() -> Actor {
    Actor("user".into())
}

fn allow(remember: RememberScope) -> UserDecision {
    UserDecision::Allow { remember }
}

fn is_deny(d: &PermissionDecision, msg: &str) -> bool {
    matches!(d, PermissionDecision::Deny { message } if message.contains(msg))
}

async fn settle<T>(h: tokio::task::JoinHandle<T>) -> T {
    tokio::time::timeout(Duration::from_secs(5), h)
        .await
        .expect("答复超时")
        .unwrap()
}

#[tokio::test]
async fn ask_then_user_allow_and_audit_every_call() {
    let b = Bench::new(None).await;
    let d = b.decider();
    assert_eq!(
        b.call(&d, turn(1), bash_req("ls")).await,
        PermissionDecision::Allow
    );
    let h = b.ask(&d, turn(1), bash_req("npm test"));
    let id = b.approval(0).await;
    let a = b.pending(id).await.unwrap();
    assert_eq!(a.kind, ApprovalKind::Ask);
    assert!(a.call.is_some());
    let out = b
        .engine
        .decide(id, allow(RememberScope::Once), user())
        .await
        .unwrap();
    assert_eq!(out.resume_turn, None);
    assert_eq!(settle(h).await, PermissionDecision::Allow);
    // 每次求值恰好一条审计，放行也记；两个游标分别记
    let audits: Vec<_> = b
        .store
        .all()
        .into_iter()
        .filter_map(|e| match e {
            DomainEvent::ApprovalEvaluated(a) => Some(a),
            _ => None,
        })
        .collect();
    assert_eq!(audits.len(), 2);
    assert_eq!(audits[0].verdict, Verdict::Allow);
    assert_eq!(audits[1].snapshot_lists_seq, Seq(7));
    assert_eq!(audits[1].sub_actions.len(), 3, "npm test 的全部子动作");
    assert!(audits[1].call_shape.is_some());
    // 已决定的不能再决定
    assert_eq!(
        b.engine.decide(id, UserDecision::Deny, user()).await,
        Err(DecideError::NotPending(id))
    );
}

/// 验收 3：拒绝后同一指纹跨回合自动拒绝；换工具（Write → echo >）同样被拒；用户新消息后变询问而非放行。
#[tokio::test]
async fn denial_memory_cross_turn_and_tool() {
    let b = Bench::new(None).await;
    let d = b.decider();
    let h = b.ask(&d, turn(1), write_req(&format!("{WS}/a.txt")));
    let id = b.approval(0).await;
    b.engine
        .decide(id, UserDecision::Deny, user())
        .await
        .unwrap();
    assert!(is_deny(&settle(h).await, MSG_USER_DENIED));
    // 跨回合、换写法
    for t in [2, 3] {
        let r = b.call(&d, turn(t), bash_req("printf x > a.txt")).await;
        assert!(is_deny(&r, MSG_USER_DENIED), "{r:?}");
    }
    // 续接（换一条会话、同一对话）
    let d2 = Arc::new(b.engine.decider_for(keys()));
    assert!(is_deny(
        &b.call(&d2, turn(4), write_req("a.txt")).await,
        MSG_USER_DENIED
    ));
    // 用户发新消息：变为询问，不放行
    b.user_message().await;
    let h = b.ask(&d, turn(5), write_req("a.txt"));
    let id2 = b.approval(1).await;
    let a = b.pending(id2).await.unwrap();
    assert_eq!(
        a.offered_remember,
        vec![RememberScope::Once],
        "强制询问只能允许一次"
    );
    b.engine
        .decide(id2, allow(RememberScope::Once), user())
        .await
        .unwrap();
    assert_eq!(settle(h).await, PermissionDecision::Allow);
    // 允许后恢复正常求值：手动档的写仍按默认询问（不是强制），对照自动档直接放行
    b.engine
        .switch_mode(
            ApprovalModeTarget::Conversation(conversation()),
            Some(Auto),
            user(),
        )
        .await
        .unwrap();
    assert_eq!(
        b.call(&d, turn(6), write_req("a.txt")).await,
        PermissionDecision::Allow
    );
}

/// F-3：拒绝组合命令后，单发其中任一子动作自动拒绝。
#[tokio::test]
async fn denied_compound_blocks_each_part() {
    let b = Bench::new(None).await;
    let d = b.decider();
    let h = b.ask(&d, turn(1), bash_req("touch x.txt && rm y.txt"));
    let id = b.approval(0).await;
    b.engine
        .decide(id, UserDecision::Deny, user())
        .await
        .unwrap();
    settle(h).await;
    assert!(is_deny(
        &b.call(&d, turn(2), bash_req("rm y.txt")).await,
        MSG_USER_DENIED
    ));
    assert!(is_deny(
        &b.call(&d, turn(2), write_req("x.txt")).await,
        MSG_USER_DENIED
    ));
}

/// 验收 6：AI 审查四种结果；通过放行，不通过 / 超时 / 崩溃转问用户且理由正确。
#[tokio::test]
async fn ai_review_four_outcomes() {
    let req = || read_req("/home/u/Documents/notes/x.txt");
    // 通过
    let b = Bench::new(Some(Auto)).await;
    b.reviewer.push(ReviewScript::Says(
        "{\"pass\": true, \"reason\": \"任务需要\"}".into(),
    ));
    let d = b.decider();
    assert_eq!(b.call(&d, turn(1), req()).await, PermissionDecision::Allow);
    assert_eq!(b.count(|e| matches!(e, DomainEvent::ApprovalDecided(x) if x.state == ApprovalState::Approved { decider: Decider::AiReview })), 1);
    let spec = b.reviewer.specs.lock().unwrap()[0].clone();
    assert!(
        spec.tool_set.enabled_groups.is_empty() && !spec.tool_set.include_always_on,
        "审查回合不开工具"
    );
    assert!(spec.mcp.is_empty());
    // 不通过、崩溃、超时
    for (script, fire, want) in [
        (
            ReviewScript::Says("{\"pass\": false, \"reason\": \"与任务无关\"}".into()),
            false,
            "Failed",
        ),
        (ReviewScript::Crashes, false, "Errored"),
        (ReviewScript::Hangs, true, "TimedOut"),
    ] {
        let b = Bench::new(Some(Auto)).await;
        b.reviewer.push(script);
        b.timer.fire.store(fire, Ordering::SeqCst);
        let d = b.decider();
        let h = b.ask(&d, turn(1), req());
        let id = b.approval(0).await;
        // 等审查结论落下
        for _ in 0..200 {
            if b.pending(id).await.is_some_and(|a| a.awaits_user()) {
                break;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        let a = b.pending(id).await.unwrap();
        assert!(a.awaits_user(), "{want}");
        let phase = format!("{:?}", a.review.phase());
        assert_eq!(phase, want);
        if let ReviewState::Failed { reason, .. } = &a.review {
            assert_eq!(reason, "与任务无关");
        }
        b.engine
            .decide(id, UserDecision::Deny, user())
            .await
            .unwrap();
        assert!(is_deny(&settle(h).await, MSG_USER_DENIED));
    }
}

/// F-4、2/F-4：A 被拒后 B 的迟到审查通过：B 仍待审批、转问用户，事件里没有 Approved。
#[tokio::test]
async fn review_recheck_fail_keeps_pending() {
    let b = Bench::new(Some(Auto)).await;
    let gate = Arc::new(tokio::sync::Notify::new());
    // A 的审查挂起；B 的审查等闸门打开才通过
    b.reviewer.push(ReviewScript::Hangs);
    b.reviewer.push(ReviewScript::SaysAfter(
        "{\"pass\": true, \"reason\": \"ok\"}".into(),
        gate.clone(),
    ));
    let d = b.decider();
    let req = || read_req("/home/u/Documents/notes/x.txt");
    let ha = b.ask(&d, turn(1), req());
    let a = b.approval(0).await;
    let hb = b.ask(&d, turn(1), req());
    let bid = b.approval(1).await;
    // 用户拒绝 A（审查进行中也可以直接决定）
    b.engine
        .decide(a, UserDecision::Deny, user())
        .await
        .unwrap();
    assert!(is_deny(&settle(ha).await, MSG_USER_DENIED));
    gate.notify_one();
    for _ in 0..200 {
        if b.pending(bid).await.is_some_and(|x| x.awaits_user()) {
            break;
        }
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    let pb = b.pending(bid).await.unwrap();
    assert_eq!(pb.state, ApprovalState::Pending);
    assert!(pb.awaits_user());
    assert!(
        matches!(&pb.review, ReviewState::Failed { reason, .. } if reason.contains("拒绝")),
        "{:?}",
        pb.review
    );
    assert_eq!(
        b.count(|e| matches!(e, DomainEvent::ApprovalDecided(x) if x.approval_id == bid)),
        0
    );
    b.engine
        .decide(bid, UserDecision::Deny, user())
        .await
        .unwrap();
    settle(hb).await;
}

/// 拒绝 → 新消息 → 旧的 AI 通过回来 → ForceAskOnce → 转问用户，不放行。
#[tokio::test]
async fn late_review_pass_after_user_message_asks_user() {
    let b = Bench::new(Some(Auto)).await;
    let gate = Arc::new(tokio::sync::Notify::new());
    b.reviewer.push(ReviewScript::Hangs);
    b.reviewer.push(ReviewScript::SaysAfter(
        "{\"pass\": true, \"reason\": \"ok\"}".into(),
        gate.clone(),
    ));
    let d = b.decider();
    let req = || read_req("/home/u/Documents/notes/x.txt");
    let ha = b.ask(&d, turn(1), req());
    let a = b.approval(0).await;
    let hb = b.ask(&d, turn(1), req());
    let bid = b.approval(1).await;
    b.engine
        .decide(a, UserDecision::Deny, user())
        .await
        .unwrap();
    settle(ha).await;
    b.user_message().await;
    gate.notify_one();
    for _ in 0..200 {
        if b.pending(bid).await.is_some_and(|x| x.awaits_user()) {
            break;
        }
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    let pb = b.pending(bid).await.unwrap();
    assert!(
        matches!(&pb.review, ReviewState::Failed { reason, .. } if reason.contains("重新确认")),
        "{:?}",
        pb.review
    );
    b.engine
        .decide(bid, allow(RememberScope::Once), user())
        .await
        .unwrap();
    assert_eq!(settle(hb).await, PermissionDecision::Allow);
}

/// 验收 2 + 6c：手动切自动只放行边界内待审批；切全开放行除 git push 外全部；组合命令带 push 保留；
/// 成员单独设置不受对话切换影响；项目默认不能全开；切回手动作废进行中的审查；卡片不动。
#[tokio::test]
async fn mode_switch_release_cases() {
    let b = Bench::new(None).await;
    let d = b.decider();
    let in_bounds = b.ask(&d, turn(1), write_req("src/new.rs"));
    let _ = b.approval(0).await;
    let oob = b.ask(&d, turn(1), write_req("/opt/x"));
    let oob_id = b.approval(1).await;
    let push = b.ask(&d, turn(1), bash_req("git push"));
    let push_id = b.approval(2).await;
    let combo = b.ask(&d, turn(1), bash_req("git pull && git push"));
    let combo_id = b.approval(3).await;
    let card = b.ask(&d, turn(1), bash_req("cat ~/.ssh/id_rsa"));
    // 手动 → 自动：只放行边界内
    b.engine
        .switch_mode(
            ApprovalModeTarget::Conversation(conversation()),
            Some(Auto),
            user(),
        )
        .await
        .unwrap();
    assert_eq!(settle(in_bounds).await, PermissionDecision::Allow);
    assert!(b.pending(oob_id).await.is_some());
    // 自动 → 全开：越界放行，push 与带 push 的组合保留
    b.engine
        .switch_mode(
            ApprovalModeTarget::Conversation(conversation()),
            Some(FullAccess),
            user(),
        )
        .await
        .unwrap();
    assert_eq!(settle(oob).await, PermissionDecision::Allow);
    assert!(b.pending(push_id).await.is_some());
    assert!(b.pending(combo_id).await.is_some());
    assert_eq!(b.count(|e| matches!(e, DomainEvent::ApprovalDecided(x) if x.state == ApprovalState::Approved { decider: Decider::ModeSwitch })), 2);
    assert!(!card.is_finished(), "卡片不随切档自动执行");
    // 项目默认不能全开
    assert!(matches!(
        b.engine
            .switch_mode(
                ApprovalModeTarget::Project(project()),
                Some(FullAccess),
                user()
            )
            .await,
        Err(ModeError::FullAccessNotAllowedAsDefault)
    ));
    // 收尾
    b.engine
        .decide(push_id, UserDecision::Deny, user())
        .await
        .unwrap();
    b.engine
        .decide(combo_id, UserDecision::Deny, user())
        .await
        .unwrap();
    settle(push).await;
    settle(combo).await;
    card.abort();
}

#[tokio::test]
async fn member_override_unaffected_by_supervisor_switch() {
    let b = Bench::new(None).await;
    b.engine
        .switch_mode(
            ApprovalModeTarget::Member {
                conversation_id: conversation(),
                assistant_id: assistant(),
            },
            Some(Manual),
            user(),
        )
        .await
        .unwrap();
    b.engine
        .switch_mode(
            ApprovalModeTarget::Conversation(conversation()),
            Some(FullAccess),
            user(),
        )
        .await
        .unwrap();
    assert_eq!(b.mode(conversation(), assistant()).await, Manual);
    let other = AssistantId::from_raw(RawUlid::from_u128(0x22));
    assert_eq!(b.mode(conversation(), other).await, FullAccess);
    // 恢复跟随
    b.engine
        .switch_mode(
            ApprovalModeTarget::Member {
                conversation_id: conversation(),
                assistant_id: assistant(),
            },
            None,
            user(),
        )
        .await
        .unwrap();
    assert_eq!(b.mode(conversation(), assistant()).await, FullAccess);
}

#[tokio::test]
async fn to_manual_abandons_review() {
    let b = Bench::new(Some(Auto)).await;
    b.reviewer.push(ReviewScript::Hangs);
    let d = b.decider();
    let h = b.ask(&d, turn(1), read_req("/home/u/Documents/notes/x.txt"));
    let id = b.approval(0).await;
    for _ in 0..100 {
        if matches!(
            b.pending(id).await.unwrap().review,
            ReviewState::InProgress { .. }
        ) {
            break;
        }
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    b.engine
        .switch_mode(
            ApprovalModeTarget::Conversation(conversation()),
            Some(Manual),
            user(),
        )
        .await
        .unwrap();
    let a = b.pending(id).await.unwrap();
    assert!(matches!(
        a.review,
        ReviewState::Abandoned {
            reason: AbandonReason::ModeChangedFromAuto,
            ..
        }
    ));
    assert!(a.awaits_user());
    b.engine
        .decide(id, UserDecision::Deny, user())
        .await
        .unwrap();
    settle(h).await;
}

/// 验收 1b 作废：取消回合、放弃、回合失败、删除对话、删除助手 → voided 且无法再处理；
/// hook 超时防护引起的中断后审批仍 pending，批准后续接执行。
#[tokio::test]
async fn void_and_wait_limit() {
    for reason in [
        VoidReason::TurnCancelled,
        VoidReason::TurnAbandonedAfterInterrupt,
        VoidReason::TurnFailed,
    ] {
        let b = Bench::new(None).await;
        let d = b.decider();
        let h = b.ask(&d, turn(1), bash_req("npm test"));
        let id = b.approval(0).await;
        let events = b.void_turn(&d, turn(1), reason).await;
        assert!(
            events
                .iter()
                .any(|e| matches!(e, DomainEvent::ApprovalVoided(v) if v.reason == reason))
        );
        assert!(is_deny(&settle(h).await, MSG_TURN_CANCELLED));
        assert_eq!(
            b.engine
                .decide(id, allow(RememberScope::Once), user())
                .await,
            Err(DecideError::NotPending(id))
        );
    }
    for (scope, event) in [
        (
            Scope::ConversationDeleting(conversation()),
            DomainEvent::from(ConversationDeleting {
                conversation_id: conversation(),
            }),
        ),
        (
            Scope::AssistantDeleted(assistant()),
            AssistantDeleted {
                assistant_id: assistant(),
                name_snapshot: "a".into(),
            }
            .into(),
        ),
    ] {
        let b = Bench::new(None).await;
        let d = b.decider();
        let h = b.ask(&d, turn(1), bash_req("npm test"));
        let id = b.approval(0).await;
        let events = b
            .engine
            .commit_with(scope, command(vec![event]))
            .await
            .unwrap()
            .attached;
        assert_eq!(events.len(), 1, "{events:?}");
        settle(h).await;
        assert!(
            b.engine
                .decide(id, UserDecision::Deny, user())
                .await
                .is_err()
        );
    }
    // 等待上限：回合把等审批的请求丢掉（hook 已自行拒绝），审批保持 pending
    let b = Bench::new(None).await;
    let d = b.decider();
    let h = b.ask(&d, turn(1), bash_req("rm a.txt && touch a.txt"));
    let id = b.approval(0).await;
    h.abort();
    let _ = h.await;
    b.interrupt(turn(1)).await;
    assert_eq!(b.pending(id).await.unwrap().state, ApprovalState::Pending);
    let out = b
        .engine
        .decide(id, allow(RememberScope::Once), user())
        .await
        .unwrap();
    assert_eq!(out.resume_turn, Some(turn(1)));
    assert!(b.grant(id).await.is_some());
    // 续接后：改写过的调用不命中（照常询问）；原样重发命中、只放一次
    for changed in [
        "touch a.txt && rm a.txt",
        "rm a.txt; touch a.txt",
        "rm a.txt && rm a.txt && touch a.txt",
        "rm a.txt > log && touch a.txt",
    ] {
        let n = b.count(|e| matches!(e, DomainEvent::ApprovalRequested(_)));
        let h2 = b.ask(&d, turn(1), bash_req(changed));
        let other = b.approval(n).await;
        assert_ne!(other, id, "{changed}");
        b.engine
            .decide(other, allow(RememberScope::Once), user())
            .await
            .unwrap();
        assert_eq!(settle(h2).await, PermissionDecision::Allow);
    }
    assert!(b.grant(id).await.is_some(), "改写过的调用不消费一次性放行");
    // 续接前用户拒绝过同一动作并发了新消息：一次性放行被强制询问挡住（grant_once_voided_by_force_ask）
    let n = b.count(|e| matches!(e, DomainEvent::ApprovalRequested(_)));
    let hx = b.ask(&d, turn(1), bash_req("rm a.txt"));
    let x = b.approval(n).await;
    b.engine
        .decide(x, UserDecision::Deny, user())
        .await
        .unwrap();
    settle(hx).await;
    b.user_message().await;
    let n = b.count(|e| matches!(e, DomainEvent::ApprovalRequested(_)));
    let h3 = b.ask(&d, turn(1), bash_req("rm a.txt && touch a.txt"));
    let again = b.approval(n).await;
    assert_ne!(again, id, "ForceAskOnce 不被一次性放行跳过");
    b.engine
        .decide(again, UserDecision::Deny, user())
        .await
        .unwrap();
    settle(h3).await;
}

/// 一次性放行：原样重发命中、只放一次（没有拒绝记忆干扰时）；别的回合不命中。
#[tokio::test]
async fn grant_once_exact_call_only() {
    let b = Bench::new(None).await;
    let d = b.decider();
    let h = b.ask(&d, turn(1), bash_req("rm a.txt && touch a.txt"));
    let id = b.approval(0).await;
    h.abort();
    let _ = h.await;
    b.interrupt(turn(1)).await;
    b.engine
        .decide(id, allow(RememberScope::Once), user())
        .await
        .unwrap();
    // 别的回合不命中
    let other_turn = b.ask(&d, turn(2), bash_req("rm a.txt && touch a.txt"));
    let o = b.approval(1).await;
    b.engine
        .decide(o, allow(RememberScope::Once), user())
        .await
        .unwrap();
    settle(other_turn).await;
    // 同一回合原样重发：命中、放行，审计层为记住
    assert_eq!(
        b.call(&d, turn(1), bash_req("rm a.txt && touch a.txt"))
            .await,
        PermissionDecision::Allow
    );
    assert!(b.grant(id).await.is_none(), "命中即消费");
    // 再发一次：照常询问
    let h = b.ask(&d, turn(1), bash_req("rm a.txt && touch a.txt"));
    let again = b.approval(2).await;
    b.engine
        .decide(again, UserDecision::Deny, user())
        .await
        .unwrap();
    settle(h).await;
}

/// 亲手执行卡片：三档都生成，回给 Agent 的只有退出码或「未在 iWork 中执行」。
#[tokio::test]
async fn user_action_card_round_trip() {
    let b = Bench::new(Some(FullAccess)).await;
    let d = b.decider();
    let h = b.ask(&d, turn(1), bash_req("cat ~/.ssh/id_rsa"));
    let card = loop {
        if let Some(c) = b.store.all().into_iter().find_map(|e| match e {
            DomainEvent::UserActionProposed(p) => Some(p.card),
            _ => None,
        }) {
            break c;
        }
        tokio::time::sleep(Duration::from_millis(5)).await;
    };
    // T10 的执行器写状态（Pending → Starting → Executing → Done）
    b.external(vec![
        card_state(card.id, UserActionState::Starting),
        card_state(card.id, UserActionState::Executing),
        card_state(
            card.id,
            UserActionState::Done {
                exit: ExitStatus::Code(3),
            },
        ),
    ])
    .await;
    assert_eq!(
        settle(h).await,
        PermissionDecision::Deny {
            message: "用户已执行，退出码 3".into()
        }
    );
    let h = b.ask(&d, turn(2), bash_req("cat ~/.ssh/config"));
    let card = loop {
        let cards: Vec<_> = b
            .store
            .all()
            .into_iter()
            .filter_map(|e| match e {
                DomainEvent::UserActionProposed(p) => Some(p.card),
                _ => None,
            })
            .collect();
        if cards.len() == 2 {
            break cards[1].clone();
        }
        tokio::time::sleep(Duration::from_millis(5)).await;
    };
    b.external(vec![card_state(card.id, UserActionState::Dismissed)])
        .await;
    assert!(is_deny(&settle(h).await, MSG_USER_NOT_EXECUTED));
}

/// 记住 → 规则：本项目允许后同类调用直接放行；不在 offered 里的范围被拒。
#[tokio::test]
async fn remember_creates_rule() {
    let b = Bench::new(None).await;
    let d = b.decider();
    let h = b.ask(&d, turn(1), write_req("src/a.rs"));
    let id = b.approval(0).await;
    b.engine
        .decide(id, allow(RememberScope::Project), user())
        .await
        .unwrap();
    settle(h).await;
    assert_eq!(b.count(|e| matches!(e, DomainEvent::RuleRemembered(_))), 1);
    assert_eq!(
        b.call(&d, turn(2), write_req("src/b.rs")).await,
        PermissionDecision::Allow
    );
    // 必问：只能允许一次
    let h = b.ask(&d, turn(3), bash_req("git push"));
    let id = b.approval(1).await;
    assert!(matches!(
        b.engine
            .decide(id, allow(RememberScope::Project), user())
            .await,
        Err(DecideError::RememberNotOffered { .. })
    ));
    b.engine
        .decide(id, UserDecision::Deny, user())
        .await
        .unwrap();
    settle(h).await;
}

/// E1、E4：后台服务崩溃后从事件重建：待审批（detached）、调用记录无损；进行中的审查放弃、转问用户；
/// 允许后发一次性放行；没有调用记录的旧审批只续接、不发放行。
#[tokio::test]
async fn engine_recovers_pending_from_events() {
    let b = Bench::new(Some(Auto)).await;
    b.reviewer.push(ReviewScript::Hangs);
    let d = b.decider();
    let h1 = b.ask(&d, turn(1), bash_req("rm a.txt && touch a.txt"));
    let plain = b.approval(0).await;
    let h2 = b.ask(&d, turn(1), read_req("/home/u/Documents/notes/x.txt"));
    let reviewing = b.approval(1).await;
    for _ in 0..100 {
        if matches!(
            b.pending(reviewing).await.unwrap().review,
            ReviewState::InProgress { .. }
        ) {
            break;
        }
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    let original = b.pending(plain).await.unwrap();
    h1.abort();
    h2.abort();
    // 丢掉全部内存状态，只凭事件重建；T04 把回合标成 Interrupted{Crash}
    let (fresh, abandoned) = b.restart(|_, _| Some(keys())).await;
    assert_eq!(abandoned.len(), 1);
    fresh
        .external(vec![state_changed(
            turn(1),
            TurnState::WaitingApproval,
            TurnState::Interrupted {
                reason: InterruptReason::Crash,
            },
        )])
        .await;
    let recovered = fresh.pending(reviewing).await.unwrap();
    assert!(matches!(
        recovered.review,
        ReviewState::Abandoned {
            reason: AbandonReason::ExecutorLost,
            ..
        }
    ));
    assert!(recovered.awaits_user());
    let p = fresh.pending(plain).await.unwrap();
    assert_eq!(p.call, original.call, "调用记录无损");
    let out = fresh
        .engine
        .decide(plain, allow(RememberScope::Once), user())
        .await
        .unwrap();
    assert_eq!(out.resume_turn, Some(turn(1)));
    assert_eq!(
        fresh.grant(plain).await.unwrap().record,
        *original.call.unwrap()
    );
    let fd = Arc::new(fresh.engine.decider_for(keys()));
    assert_eq!(
        fresh
            .call(&fd, turn(1), bash_req("rm a.txt && touch a.txt"))
            .await,
        PermissionDecision::Allow
    );
}

/// F-5：会话用启动时的快照；删了用户清单条目，旧会话仍判沙箱外；重建会话后判沙箱内，并提示要重建。
#[tokio::test]
async fn snapshot_pins_excluded_commands() {
    let b = Bench::new(None).await;
    let tf = RuleEntry {
        id: iwork_domain::id::RuleId::from_raw(RawUlid::from_u128(0x4444)),
        layer: iwork_domain::rule::RuleLayer::CredentialCommands,
        matcher: iwork_domain::rule::RuleMatcher::ProgramSubcommand {
            program: "terraform".into(),
            subcommand: Some("apply".into()),
        },
        effect: iwork_domain::rule::RuleEffect::OutsideSandbox,
        source: iwork_domain::rule::RuleSource::User,
        scope: iwork_domain::rule::RuleScope::Project(project()),
        created_at: Timestamp(1),
        label: "terraform apply".into(),
        applies_in: Default::default(),
    };
    let old = Arc::new(
        b.engine
            .decider_for(keys_with(std::slice::from_ref(&tf), assistant())),
    );
    let before = b.engine.snapshot_inputs_seq();
    b.external(vec![
        iwork_domain::event::SecurityListEntryRemoved { entry_id: tf.id }.into(),
    ])
    .await;
    assert!(
        b.engine.snapshot_inputs_seq() > before,
        "清单变了：生成于这之前的会话快照已过期"
    );
    let h = b.ask(&old, turn(1), bash_req("terraform apply"));
    let id = b.approval(0).await;
    assert_eq!(
        b.pending(id).await.unwrap().kind,
        ApprovalKind::OutsideSandbox
    );
    b.engine
        .decide(id, UserDecision::Deny, user())
        .await
        .unwrap();
    settle(h).await;
    let new = Arc::new(b.engine.decider_for(keys()));
    b.user_message().await;
    let h = b.ask(&new, turn(2), bash_req("terraform apply -x"));
    let id = b.approval(1).await;
    assert_eq!(b.pending(id).await.unwrap().kind, ApprovalKind::Ask);
    b.engine
        .decide(id, UserDecision::Deny, user())
        .await
        .unwrap();
    settle(h).await;
}

/// 验收 7：每条审计事件只凭事件日志（两个游标）就能重建当时的规则集，对子动作重新求值得到同一结果。
/// 含「撤销插在两次求值之间」：撤销后的求值用的是撤销后的活规则。
#[tokio::test]
async fn audit_snapshot_reconstructible() {
    use iwork_core::approval::denial::DenialMemory;
    use iwork_core::approval::evaluate::strictness::strictness;
    use iwork_core::approval::evaluate::{EvalContext, evaluate};
    use iwork_core::approval::rules::build_ruleset;
    let b = Bench::new(None).await;
    let d = b.decider();
    let h = b.ask(&d, turn(1), write_req("src/a.rs"));
    let id = b.approval(0).await;
    b.engine
        .decide(id, allow(RememberScope::Project), user())
        .await
        .unwrap();
    settle(h).await;
    assert_eq!(
        b.call(&d, turn(2), write_req("src/b.rs")).await,
        PermissionDecision::Allow
    );
    // 撤销：提交到日志并通知引擎
    let rule_id = b
        .store
        .all()
        .into_iter()
        .find_map(|e| match e {
            DomainEvent::RuleRemembered(r) => Some(r.rule.id),
            _ => None,
        })
        .unwrap();
    let revoke: DomainEvent = iwork_domain::event::RuleRevoked { rule_id }.into();
    {
        use iwork_ports::store::{CommitRequest, StoragePort};
        b.store
            .commit(CommitRequest {
                command_id: iwork_domain::id::CommandId::from_raw(RawUlid::from_u128(1)),
                actor: user(),
                events: vec![revoke.clone()],
                outbox: vec![],
            })
            .await
            .unwrap();
    }
    b.engine.sync().await.unwrap();
    let h = b.ask(&d, turn(3), write_req("src/c.rs"));
    let id = b.approval(1).await;
    b.engine
        .decide(id, UserDecision::Deny, user())
        .await
        .unwrap();
    settle(h).await;

    let stored = b.store.stored();
    let k = keys();
    let mut checked = 0;
    for ev in &stored {
        let DomainEvent::ApprovalEvaluated(a) = &ev.event else {
            continue;
        };
        // 只凭日志重建活规则：重放到 live_rules_seq
        let mut live: Vec<RuleEntry> = Vec::new();
        for e in stored.iter().filter(|e| e.seq <= a.live_rules_seq) {
            match &e.event {
                DomainEvent::RuleRemembered(r) => live.push(r.rule.clone()),
                DomainEvent::RuleRevoked(r) => live.retain(|x| x.id != r.rule_id),
                _ => {}
            }
        }
        assert_eq!(a.snapshot_lists_seq, k.snapshot.lists_seq);
        let rules = build_ruleset(&k.snapshot.rules, &live, &k.scope()).unwrap();
        let denial = DenialMemory::default();
        let ctx = EvalContext {
            mode: Manual,
            turn_kind: TurnKind::Assistant,
            tool_set: &k.tool_set,
            disabled_groups: &k.disabled_groups,
            denial: &denial,
        };
        let shape = iwork_core::approval::action::CallShape::Tool;
        let worst = a
            .sub_actions
            .iter()
            .map(|x| evaluate(x, &shape, &rules, &ctx, &fs()).0.verdict)
            .max_by_key(strictness)
            .unwrap();
        assert_eq!(strictness(&worst), strictness(&a.verdict), "{a:?}");
        checked += 1;
    }
    assert_eq!(checked, 3, "三次求值各一条审计");
}

/// 收口第 2 轮 closeout 1/F-4：规则路径的解析结果随审计落盘，重放只用事件里的映射、不读磁盘。
/// 会话启动时 `~/.npmrc` 链到 A，求值命中别名被拒；之后链接改到 B、再改回普通文件，丢掉引擎内存，
/// 换一个一调 `resolve` 就 panic 的解析器重放，重建出的别名仍指向 A、ID 与审计里记的相同、结果相同。
#[tokio::test]
async fn audit_replay_uses_recorded_rule_paths() {
    use iwork_core::approval::denial::DenialMemory;
    use iwork_core::approval::evaluate::strictness::strictness;
    use iwork_core::approval::evaluate::{EvalContext, evaluate};
    use iwork_core::approval::rules::build_ruleset;
    use iwork_core::approval::session::{rule_paths_for, snapshot_for};
    use iwork_domain::action::CanonicalPath;
    use iwork_ports::fs::{PathResolveError, PathResolver, ResolvedPath};

    /// 比较照常、一调解析就 panic：证明重放不读磁盘。
    struct NoResolve(FakeFs);
    impl PathResolver for NoResolve {
        fn resolve(&self, _: &CanonicalPath, raw: &str) -> Result<ResolvedPath, PathResolveError> {
            panic!("重放调用了 resolve：{raw}")
        }
        fn same_path<A: AsRef<std::path::Path> + ?Sized, B: AsRef<std::path::Path> + ?Sized>(
            &self,
            a: &A,
            b: &B,
        ) -> bool {
            self.0.same_path(a, b)
        }
        fn is_under<A: AsRef<std::path::Path> + ?Sized, B: AsRef<std::path::Path> + ?Sized>(
            &self,
            child: &A,
            dir: &B,
        ) -> bool {
            self.0.is_under(child, dir)
        }
    }

    let layout = |target: Option<&str>| {
        let mut f = fs();
        f.file("/home/u/dotfiles/a");
        f.file("/home/u/dotfiles/b");
        match target {
            Some(t) => {
                f.link("/home/u/.npmrc", t);
            }
            None => {
                f.file("/home/u/.npmrc");
            }
        }
        f
    };
    let fs_a = layout(Some("dotfiles/a"));
    let b = Bench::with_fs(None, fs_a.clone()).await;
    let mut k = keys();
    k.snapshot = Arc::new(snapshot_on(Manual, &[], &env(), Seq(7), &fs_a));
    let d = Arc::new(b.engine.decider_for(k.clone()));
    assert!(matches!(
        // Read 工具命中底线不出卡片，直接拒（单条 Bash 命令会出卡片、等用户）
        b.call(&d, turn(1), read_req("~/dotfiles/a")).await,
        PermissionDecision::Deny { .. }
    ));
    let audit = b
        .store
        .all()
        .into_iter()
        .find_map(|e| match e {
            DomainEvent::ApprovalEvaluated(a) => Some(a),
            _ => None,
        })
        .unwrap();
    let expected = vec![iwork_domain::event::RulePathResolution {
        path: "/home/u/.npmrc".into(),
        resolved: "/home/u/dotfiles/a".into(),
    }];
    assert_eq!(audit.snapshot_rule_paths, expected);

    for later in [layout(Some("dotfiles/b")), layout(None)] {
        // 对照：现在的磁盘给出的事实已经不同
        assert_ne!(rule_paths_for(&[], &scope(), &env(), &later), expected);
        let replayed = snapshot_for(
            Manual,
            &[],
            &scope(),
            &env(),
            audit.snapshot_lists_seq,
            &audit.snapshot_rule_paths,
        );
        assert_eq!(replayed.rules, k.snapshot.rules, "重建出的快照与当时相同");
        let alias_ids: Vec<_> = replayed
            .rules
            .iter()
            .filter(|r| r.label.contains("dotfiles/a"))
            .map(|r| r.id)
            .collect();
        assert!(!alias_ids.is_empty());
        assert!(
            audit
                .matched_rule_ids
                .iter()
                .any(|id| alias_ids.contains(id))
        );
        let rules = build_ruleset(&replayed.rules, &[], &k.scope()).unwrap();
        let denial = DenialMemory::default();
        let ctx = EvalContext {
            mode: Manual,
            turn_kind: TurnKind::Assistant,
            tool_set: &k.tool_set,
            disabled_groups: &k.disabled_groups,
            denial: &denial,
        };
        let shape = iwork_core::approval::action::CallShape::Tool;
        let no_resolve = NoResolve(later);
        let worst = audit
            .sub_actions
            .iter()
            .map(|x| evaluate(x, &shape, &rules, &ctx, &no_resolve).0.verdict)
            .max_by_key(strictness)
            .unwrap();
        assert_eq!(strictness(&worst), strictness(&audit.verdict));
    }
}

/// 三条内置拒绝条目回给运行时的话（方案 4.2、9/F-6）。
#[tokio::test]
async fn builtin_deny_messages() {
    let b = Bench::new(None).await;
    let d = b.decider();
    let big = format!("echo {}", "a".repeat(iwork_shell::TEXT_LIMIT));
    for (cmd, want) in [
        (
            big.as_str(),
            "被规则拒绝：命令太大或展开出的检查项过多，请拆成几条重发",
        ),
        (
            "echo \"abc",
            "被规则拒绝：命令有语法错误，无法检查，请修正后重发",
        ),
    ] {
        match b.call(&d, turn(1), bash_req(cmd)).await {
            PermissionDecision::Deny { message } => assert_eq!(message, want),
            other => panic!("{other:?}"),
        }
    }
    let mut f = fs();
    f.add(format!("{WS}/gate/root"));
    f.deny(format!("{WS}/gate"));
    let b = Bench::with_fs(None, f).await;
    let d = b.decider();
    match b.call(&d, turn(2), bash_req("rm -rf gate/root")).await {
        PermissionDecision::Deny { message } => assert_eq!(
            message,
            "被规则拒绝：路径查不清（权限不足等），无法检查；请先解决再重发"
        ),
        other => panic!("{other:?}"),
    }
    // 不出卡片、不出审批
    assert!(!b.store.all().iter().any(|e| matches!(
        e,
        DomainEvent::ApprovalRequested(_) | DomainEvent::UserActionProposed(_)
    )));
}

#[path = "engine/review_round1_b.rs"]
mod review_round1_b;

#[path = "engine/concurrency.rs"]
mod concurrency;
#[path = "engine/review_round2.rs"]
mod review_round2;
