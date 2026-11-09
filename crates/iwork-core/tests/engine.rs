//! 审批引擎（T07 第 6 步；验收 1b 作废与强制询问、2 档位、3 拒绝记忆、6 AI 审查、6c、7 审计、
//! T06 卡第 3 条一次性放行、崩溃恢复）。

mod support;

use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::Duration;

use ApprovalMode::{Auto, FullAccess, Manual};
use iwork_core::approval::engine::{
    ApprovalEngine, ConversationModes, DecideError, EnginePorts, MSG_TURN_CANCELLED,
    MSG_USER_DENIED, MSG_USER_NOT_EXECUTED, ModeError, SessionDecider, SessionKeys, UserDecision,
};
use iwork_core::approval::session::{snapshot_for, tool_set_for};
use iwork_core::turn::PermissionDecider;
use iwork_domain::approval::{
    AbandonReason, ApprovalKind, ApprovalState, Decider, ReviewState, VoidReason,
};
use iwork_domain::approval_mode::{ApprovalMode, ProjectDefaultMode, RememberScope};
use iwork_domain::common::{Actor, ModelId, ModelSelection, Timestamp, ToolGroups};
use iwork_domain::event::{
    ApprovalModeTarget, AssistantDeleted, ConversationDeleting, DomainEvent, MessageAppended,
};
use iwork_domain::id::{ApprovalId, AssistantId, MessageId, RawUlid, Seq, TurnId};
use iwork_domain::message::{Author, Message, MessageBody, MessageStatus};
use iwork_domain::project::AiReviewSettings;
use iwork_domain::rule::{RuleEntry, Verdict};
use iwork_domain::turn::TurnKind;
use iwork_domain::user_action::{ExitStatus, UserActionState};
use iwork_ports::agent::{PermissionDecision, PermissionRequest};
use support::engine_fakes::*;
use support::fake_fs::FakeFs;
use support::*;

type Engine = ApprovalEngine<TickClock, Ids, MemStore, FakeReviewer, FakeTimer, FakeFs>;
type Decider_ = SessionDecider<TickClock, Ids, MemStore, FakeReviewer, FakeTimer, FakeFs>;

struct Bench {
    engine: Arc<Engine>,
    store: MemStore,
    reviewer: FakeReviewer,
    timer: FakeTimer,
}

fn turn(n: u128) -> TurnId {
    TurnId::from_raw(RawUlid::from_u128(0x500 + n))
}

fn keys_with(lists: &[RuleEntry], assistant: AssistantId) -> SessionKeys {
    SessionKeys {
        conversation: conversation(),
        assistant,
        project: Some(project()),
        turn_kind: TurnKind::Assistant,
        snapshot: Arc::new(snapshot_for(Manual, lists, &scope(), &env(), Seq(7))),
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

impl Bench {
    async fn new(mode: Option<ApprovalMode>) -> Self {
        let store = MemStore::default();
        let reviewer = FakeReviewer::default();
        let timer = FakeTimer::default();
        let engine = Arc::new(ApprovalEngine::new(
            EnginePorts {
                clock: TickClock::default(),
                ids: Ids::default(),
                store: store.clone(),
                reviewer: reviewer.clone(),
                timer: timer.clone(),
                fs: fs(),
            },
            Default::default(),
        ));
        engine
            .set_modes(
                conversation(),
                ConversationModes::Project {
                    project_default: ProjectDefaultMode::Manual,
                    conversation_setting: mode,
                    member_overrides: Default::default(),
                },
            )
            .await;
        Self {
            engine,
            store,
            reviewer,
            timer,
        }
    }

    fn decider(&self) -> Arc<Decider_> {
        Arc::new(self.engine.decider_for(keys()))
    }

    /// 在后台发起一次请求，返回答复句柄。
    fn ask(
        &self,
        d: &Arc<Decider_>,
        t: TurnId,
        req: PermissionRequest,
    ) -> tokio::task::JoinHandle<PermissionDecision> {
        let d = d.clone();
        tokio::spawn(async move { d.decide(t, &req).await })
    }

    async fn call(
        &self,
        d: &Arc<Decider_>,
        t: TurnId,
        req: PermissionRequest,
    ) -> PermissionDecision {
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
        self.engine
            .on_event(Seq(100), &MessageAppended { message: m }.into())
            .await;
    }

    fn count(&self, pred: impl Fn(&DomainEvent) -> bool) -> usize {
        self.store.all().iter().filter(|e| pred(e)).count()
    }
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
    let a = b.engine.pending_approval(id).await.unwrap();
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
    let a = b.engine.pending_approval(id2).await.unwrap();
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
            if b.engine
                .pending_approval(id)
                .await
                .is_some_and(|a| a.awaits_user())
            {
                break;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        let a = b.engine.pending_approval(id).await.unwrap();
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
        if b.engine
            .pending_approval(bid)
            .await
            .is_some_and(|x| x.awaits_user())
        {
            break;
        }
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    let pb = b.engine.pending_approval(bid).await.unwrap();
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
        if b.engine
            .pending_approval(bid)
            .await
            .is_some_and(|x| x.awaits_user())
        {
            break;
        }
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    let pb = b.engine.pending_approval(bid).await.unwrap();
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
    assert!(b.engine.pending_approval(oob_id).await.is_some());
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
    assert!(b.engine.pending_approval(push_id).await.is_some());
    assert!(b.engine.pending_approval(combo_id).await.is_some());
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
    assert_eq!(
        b.engine.effective_mode(conversation(), assistant()).await,
        Manual
    );
    let other = AssistantId::from_raw(RawUlid::from_u128(0x22));
    assert_eq!(
        b.engine.effective_mode(conversation(), other).await,
        FullAccess
    );
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
    assert_eq!(
        b.engine.effective_mode(conversation(), assistant()).await,
        FullAccess
    );
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
            b.engine.pending_approval(id).await.unwrap().review,
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
    let a = b.engine.pending_approval(id).await.unwrap();
    assert!(matches!(
        a.review,
        ReviewState::Abandoned {
            reason: AbandonReason::ModeChangedToManual,
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
        let events = d.void_pending(turn(1), reason).await;
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
    for event in [
        DomainEvent::from(ConversationDeleting {
            conversation_id: conversation(),
        }),
        AssistantDeleted {
            assistant_id: assistant(),
            name_snapshot: "a".into(),
        }
        .into(),
    ] {
        let b = Bench::new(None).await;
        let d = b.decider();
        let h = b.ask(&d, turn(1), bash_req("npm test"));
        let id = b.approval(0).await;
        let events = b.engine.void_for_event(&event).await;
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
    assert_eq!(
        b.engine.pending_approval(id).await.unwrap().state,
        ApprovalState::Pending
    );
    let out = b
        .engine
        .decide(id, allow(RememberScope::Once), user())
        .await
        .unwrap();
    assert_eq!(out.resume_turn, Some(turn(1)));
    assert!(b.engine.grant(conversation(), id).await.is_some());
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
    assert!(
        b.engine.grant(conversation(), id).await.is_some(),
        "改写过的调用不消费一次性放行"
    );
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
    assert!(
        b.engine.grant(conversation(), id).await.is_none(),
        "命中即消费"
    );
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
    b.engine
        .on_user_action_settled(
            card.id,
            &UserActionState::Done {
                exit: ExitStatus::Code(3),
            },
        )
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
    b.engine
        .on_user_action_settled(card.id, &UserActionState::Dismissed)
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
            b.engine.pending_approval(reviewing).await.unwrap().review,
            ReviewState::InProgress { .. }
        ) {
            break;
        }
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    let original = b.engine.pending_approval(plain).await.unwrap();
    h1.abort();
    h2.abort();
    // 丢掉全部内存状态，只凭事件重建
    let fresh = Bench::new(Some(Auto)).await;
    let abandoned = fresh
        .engine
        .recover(&b.store.stored(), |_| Some(keys()))
        .await;
    assert_eq!(abandoned.len(), 1);
    let recovered = fresh.engine.pending_approval(reviewing).await.unwrap();
    assert!(matches!(
        recovered.review,
        ReviewState::Abandoned {
            reason: AbandonReason::ExecutorLost,
            ..
        }
    ));
    assert!(recovered.awaits_user());
    let p = fresh.engine.pending_approval(plain).await.unwrap();
    assert_eq!(p.call, original.call, "调用记录无损");
    let out = fresh
        .engine
        .decide(plain, allow(RememberScope::Once), user())
        .await
        .unwrap();
    assert_eq!(out.resume_turn, Some(turn(1)));
    assert_eq!(
        fresh
            .engine
            .grant(conversation(), plain)
            .await
            .unwrap()
            .record,
        *original.call.unwrap()
    );
    let fd = Arc::new(fresh.engine.decider_for(keys()));
    assert_eq!(
        fd.decide(turn(1), &bash_req("rm a.txt && touch a.txt"))
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
    b.engine
        .on_event(
            Seq(50),
            &iwork_domain::event::SecurityListEntryRemoved { entry_id: tf.id }.into(),
        )
        .await;
    assert!(b.engine.rebuild_session_needed(conversation()).await);
    let h = b.ask(&old, turn(1), bash_req("terraform apply"));
    let id = b.approval(0).await;
    assert_eq!(
        b.engine.pending_approval(id).await.unwrap().kind,
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
    assert_eq!(
        b.engine.pending_approval(id).await.unwrap().kind,
        ApprovalKind::Ask
    );
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
    let seq = Seq(b.store.stored().len() as u64);
    b.engine.on_event(seq, &revoke).await;
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
