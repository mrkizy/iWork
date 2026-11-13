//! 回合宿主（T10a 方案 D 组、A5、R 组）：真存储 + 真引擎 + 真观察者 + 假运行时。

mod support;

use std::sync::atomic::Ordering;

use iwork_domain::id::AssistantId;
use iwork_domain::turn::TurnState;
use support::turns::*;

/// D1–D4：同一个对话连发三条消息，回合一个接一个跑，任何时刻至多一趟；第二条在第一条结束之后才开始。
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn three_messages_run_in_order() {
    let mut h = harness(Opts::default()).await;
    h.agent.hold.store(true, Ordering::SeqCst);
    h.start_host();
    let a = h.send(1, "一").await;
    let b = h.send(2, "二").await;
    let c = h.send(3, "三").await;
    h.agent.wait_prompts(1).await;
    assert_eq!(h.turn_state(a).await, TurnState::Running);
    assert_eq!(h.turn_state(b).await, TurnState::Queued);
    assert_eq!(h.turn_state(c).await, TurnState::Queued);
    assert_eq!(h.agent.prompt_count(), 1, "同一时刻只有一趟");
    h.agent.finish_one();
    h.wait_state(a, |s| s == TurnState::Completed).await;
    h.wait_state(b, |s| s == TurnState::Running).await;
    assert_eq!(h.turn_state(c).await, TurnState::Queued);
    h.agent.finish_one();
    h.wait_state(b, |s| s == TurnState::Completed).await;
    h.agent.finish_one();
    h.wait_state(c, |s| s == TurnState::Completed).await;
    assert_eq!(h.agent.prompt_count(), 3);
    // 同一条会话线：后两个回合续着第一个回合的会话说
    let opened = h.agent.opened_with.lock().unwrap().clone();
    assert_eq!(opened, vec![None], "一条线一个会话：只开过一次");
    h.stop().await;
}

/// R8：项目对话的回合交给内置助手 iWork。
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn project_turn_goes_to_builtin_assistant() {
    let mut h = harness(Opts::default()).await;
    h.start_host();
    let t = h.send(1, "hi").await;
    h.wait_state(t, |s| s == TurnState::Completed).await;
    let turn = {
        use iwork_ports::read_model::ReadModelPort;
        support::fixtures::read_model(&h.raw)
            .turn_with_conversation(t)
            .await
            .unwrap()
            .unwrap()
            .0
    };
    assert_eq!(turn.assistant_id, AssistantId::BUILTIN_IWORK);
    h.stop().await;
}

use std::sync::Arc;

use iwork_domain::event::{
    AssistantDeleted, DomainEvent, MessageAppended, SecurityListEntryAdded, TurnIntentRecorded,
    TurnQueued, TurnStateChanged,
};
use iwork_domain::rule::{RuleLayer, RuleScope};
use iwork_domain::turn::{InterruptReason, TurnIntentKind};
use iwork_ports::store::StoragePort;
use support::fixtures::*;
use tokio::sync::mpsc;

/// 用户消息与排它的回合（同一笔，SendMessage 的形状），回合的助手是 `assistant`。
fn queued_with_request(msg: u128, turn_n: u128, assistant_id: AssistantId) -> Vec<DomainEvent> {
    let mut t = turn(turn_n, CONV);
    t.assistant_id = assistant_id;
    let mut m = user_message(msg, CONV, "请求");
    m.context_project_id = Some(pid(PROJECT));
    vec![
        MessageAppended { message: m }.into(),
        TurnQueued { turn: t }.into(),
    ]
}

/// 夹具里的回合都在对话 `CONV`。
fn changed(turn_n: u128, from: TurnState, to: TurnState) -> DomainEvent {
    TurnStateChanged {
        turn_id: tid(turn_n),
        conversation_id: Some(cid(CONV)),
        from,
        to,
        reason: None,
    }
    .into()
}

const CRASH: TurnState = TurnState::Interrupted {
    reason: InterruptReason::Crash,
};

/// 一个已中断的回合（崩溃恢复留下的）。
fn interrupted(turn_n: u128) -> Vec<DomainEvent> {
    let mut events = queued_with_request(900 + turn_n, turn_n, AssistantId::BUILTIN_IWORK);
    events.push(changed(turn_n, TurnState::Queued, TurnState::Running));
    events.push(changed(turn_n, TurnState::Running, CRASH));
    events
}

/// K5（D3、G8）：会话线任务压在准备的读上，宿主被叫醒两次：这条线上只有一份活——交活次数 1、准备里的读只发了一次。
/// 断言之前先确认宿主确实又跑过两轮（7.2a 表下第四条）。
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn two_wakes_one_job() {
    let mut h = harness(Opts::default()).await;
    let (hook, mut reached, release) = barrier();
    h.host.counters().hooks.lock().unwrap().before_prepare_read = Some(hook);
    h.start_host();
    let t = h.send(1, "hi").await;
    within("任务停在准备的读上", reached.recv()).await;
    let r0 = h.host.counters().rounds.load(Ordering::SeqCst);
    h.host.wake(); // 仿提交观察者
    h.wait_rounds(r0 + 1).await;
    h.host.wake(); // 仿 engine-sync
    h.wait_rounds(r0 + 2).await;
    assert_eq!(
        h.host.counters().handed.load(Ordering::SeqCst),
        1,
        "只交过一份活"
    );
    assert_eq!(
        h.host.counters().prepare_reads.load(Ordering::SeqCst),
        1,
        "准备里的读只发了一次"
    );
    h.host.counters().hooks.lock().unwrap().before_prepare_read = None;
    release.notify_waiters();
    h.wait_state(t, |s| s == TurnState::Completed).await;
    assert_eq!(h.agent.prompt_count(), 1);
    h.stop().await;
}

/// 归属按回合判：一个项目对话里排着两个回合、助手不同。后一个的助手没了：前一个照常派发，轮到后一个时不派发、
/// 收成已取消。反过来的一格：前一个的助手没了 → 它收成已取消，后一个照常派发。
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn owner_is_judged_per_turn() {
    const HELPER: u128 = 40;
    for helper_first in [false, true] {
        let mut h = harness(Opts::default()).await;
        let mut c = 100;
        let mut commit = |events: Vec<DomainEvent>| {
            c += 1;
            let raw = h.raw.clone();
            let n = c;
            async move { raw.commit(cmd(n, events)).await.unwrap() }
        };
        commit(vec![other_assistant(HELPER)]).await;
        let (builtin_turn, helper_turn) = (60, 61);
        let order = if helper_first {
            [
                (helper_turn, aid(HELPER)),
                (builtin_turn, AssistantId::BUILTIN_IWORK),
            ]
        } else {
            [
                (builtin_turn, AssistantId::BUILTIN_IWORK),
                (helper_turn, aid(HELPER)),
            ]
        };
        for (n, a) in order {
            commit(queued_with_request(n + 500, n, a)).await;
        }
        commit(vec![
            AssistantDeleted {
                assistant_id: aid(HELPER),
                name_snapshot: "a40".into(),
            }
            .into(),
        ])
        .await;
        h.start_host();
        h.wait_state(tid(builtin_turn), |s| s == TurnState::Completed)
            .await;
        h.wait_state(tid(helper_turn), |s| s == TurnState::Cancelled)
            .await;
        assert_eq!(
            h.agent.prompt_count(),
            1,
            "助手没了的回合没有发 prompt（helper_first={helper_first}）"
        );
        h.stop().await;
    }
}

/// D10：探针关着（7/F-2 修好之前组装处就是这样）时，跑完一个回合，版本控制被调用 0 次。对照：开着时调过。
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn probe_disabled_makes_no_vcs_calls() {
    for enabled in [false, true] {
        let mut h = harness(Opts {
            probe_enabled: enabled,
            ..Opts::default()
        })
        .await;
        h.start_host();
        let t = h.send(1, "hi").await;
        h.wait_state(t, |s| s == TurnState::Completed).await;
        let calls = h.vcs.calls.load(Ordering::SeqCst);
        if enabled {
            assert!(calls > 0, "对照：开着时拍过改动");
        } else {
            assert_eq!(calls, 0, "关着时一次版本控制调用都不发");
        }
        h.stop().await;
    }
}

/// D7 ②：这次运行里用户点「继续」，宿主续接；续接说明发给运行时，接着原来的会话说。
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn resume_in_same_run_works() {
    let mut h = harness(Opts {
        extra: interrupted(70),
        ..Opts::default()
    })
    .await;
    h.start_host();
    let r = h
        .ask(
            fresh_id(1),
            iwork_proto::pb::command::Kind::ResumeTurn(iwork_proto::pb::ResumeTurn {
                turn_id: tid(70).to_string(),
            }),
        )
        .await;
    assert!(
        matches!(r.body, Some(iwork_proto::pb::response::Body::Accepted(_))),
        "{r:?}"
    );
    h.wait_state(tid(70), |s| s == TurnState::Completed).await;
    let prompts = h.agent.prompts.lock().unwrap().clone();
    assert_eq!(prompts.len(), 1);
    let iwork_ports::agent::ContentBlock::Text(note) = &prompts[0].1[0] else {
        panic!("{prompts:?}")
    };
    assert!(note.contains("上一轮被中断了"), "续接说明：{note}");
    h.stop().await;
}

/// R7（K10 的同一条规则）：重启之前落盘的继续意图不自己生效——回合留在已中断，没有 prompt。
/// 对照见 `resume_in_same_run_works`（意图比启动时的 head 大就续接）。
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn resume_intent_from_before_restart_is_ignored() {
    let mut events = interrupted(70);
    events.push(
        TurnIntentRecorded {
            turn_id: tid(70),
            conversation_id: Some(cid(CONV)),
            intent: TurnIntentKind::Resume,
        }
        .into(),
    );
    let mut h = harness(Opts {
        extra: events,
        ..Opts::default()
    })
    .await;
    h.start_host();
    for _ in 0..2 {
        let r = h.host.counters().rounds.load(Ordering::SeqCst);
        h.host.wake();
        h.wait_rounds(r + 1).await;
    }
    assert_eq!(h.turn_state(tid(70)).await, CRASH);
    assert_eq!(h.agent.prompt_count(), 0);
    assert_eq!(h.host.counters().handed.load(Ordering::SeqCst), 0);
    h.stop().await;
}

/// D8、D3a：清单没变时连跑两个回合沿用同一条线（只开过一次会话）；两轮之间加一条清单条目，下一个回合换新线，
/// 新线接着这个对话最后一条会话绑定说。
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn list_change_swaps_line_and_resumes_last_binding() {
    let mut h = harness(Opts::default()).await;
    h.start_host();
    let a = h.send(1, "一").await;
    h.wait_state(a, |s| s == TurnState::Completed).await;
    let b = h.send(2, "二").await;
    h.wait_state(b, |s| s == TurnState::Completed).await;
    assert_eq!(
        *h.agent.opened_with.lock().unwrap(),
        vec![None],
        "清单没变：沿用这条线"
    );
    let mut entry = rule(80, RuleLayer::SensitiveDirs, RuleScope::Global);
    entry.matcher = iwork_domain::rule::RuleMatcher::PathUnder("/secret-new".into());
    h.raw
        .commit(cmd(
            200,
            vec![SecurityListEntryAdded { rule: entry }.into()],
        ))
        .await
        .unwrap();
    let c = h.send(3, "三").await;
    h.wait_state(c, |s| s == TurnState::Completed).await;
    assert_eq!(
        *h.agent.opened_with.lock().unwrap(),
        vec![None, Some("rt-0".to_owned())],
        "换了线：新线带着最后一条绑定的编号"
    );
    h.stop().await;
}

/// 任务卡验收 6（需求 v1.78）：续接时运行时回「会话记录不存在」，回合照常完成；事件里有带 `replaced` 的绑定
/// （新旧编号都在）；新会话的第一条输入是摘要加这一轮的请求：`summary_source` 在真存储上取到的是这条会话线
/// 之前的消息，这一轮的请求不在摘要里（它和 `TurnQueued` 同一笔）。
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn resume_with_lost_session_uses_summary() {
    let mut h = harness(Opts::default()).await;
    h.start_host();
    let a = h.send(1, "第一句").await;
    h.wait_state(a, |s| s == TurnState::Completed).await;
    // 让下一个回合换新线（续接 rt-0），再让续接失败
    let mut entry = rule(80, RuleLayer::SensitiveDirs, RuleScope::Global);
    entry.matcher = iwork_domain::rule::RuleMatcher::PathUnder("/secret-new".into());
    h.raw
        .commit(cmd(
            200,
            vec![SecurityListEntryAdded { rule: entry }.into()],
        ))
        .await
        .unwrap();
    h.agent.lose_on_resume.store(true, Ordering::SeqCst);
    let b = h.send(2, "第二句").await;
    h.wait_state(b, |s| s == TurnState::Completed).await;
    let replaced = support::turns::events(&h.raw)
        .await
        .into_iter()
        .find_map(|(_, e)| match e {
            DomainEvent::TurnSessionBound(s) if s.turn_id == b => s
                .replaced
                .map(|r| (s.runtime_session_id, r.old_runtime_session_id)),
            _ => None,
        });
    assert_eq!(
        replaced,
        Some(("rt-1".to_owned(), "rt-0".to_owned())),
        "审计里有新旧编号"
    );
    let prompts = h.agent.prompts.lock().unwrap().clone();
    let (session, blocks) = prompts.last().unwrap();
    assert_eq!(session, "rt-1");
    let text: String = blocks
        .iter()
        .map(|b| match b {
            iwork_ports::agent::ContentBlock::Text(t) => t.clone(),
            other => panic!("{other:?}"),
        })
        .collect();
    assert!(text.contains(iwork_core::turn::SUMMARY_HEADER), "{text}");
    assert!(text.contains("第一句"), "摘要里有这条线之前的消息：{text}");
    assert_eq!(
        text.matches("第二句").count(),
        1,
        "这一轮的请求只出现一次（不在摘要里）：{text}"
    );
    h.stop().await;
}

/// R6、D7 ①：等审批到期中断后用户批准，引擎折叠里它在 `turns_to_resume()`：宿主自动续接，续接说明点名批准的那次调用。
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn resume_from_grant() {
    let mut h = harness(Opts::default()).await;
    h.start_host();
    let t = interrupted_by_wait_limit(&h, 1).await;
    let approval = support::turns::events(&h.raw)
        .await
        .into_iter()
        .find_map(|(_, e)| match e {
            DomainEvent::ApprovalRequested(a) if a.approval.turn_id == t => Some(a.approval.id),
            _ => None,
        });
    h.agent.ask.store(false, Ordering::SeqCst);
    let r = h
        .ask(
            fresh_id(2),
            iwork_proto::pb::command::Kind::DecideApproval(iwork_proto::pb::DecideApproval {
                approval_id: approval.expect("有审批").to_string(),
                decision: iwork_proto::pb::ApprovalDecision::Allow.into(),
                remember: 0,
            }),
        )
        .await;
    assert!(
        matches!(r.body, Some(iwork_proto::pb::response::Body::Accepted(_))),
        "{r:?}"
    );
    h.wait_state(t, |s| s == TurnState::Completed).await;
    let prompts = h.agent.prompts.lock().unwrap().clone();
    assert_eq!(prompts.len(), 2, "第一趟 + 续接一趟");
    let iwork_ports::agent::ContentBlock::Text(note) = &prompts[1].1[0] else {
        panic!()
    };
    assert!(
        note.contains("已批准") && note.contains("git push"),
        "续接说明：{note}"
    );
    h.stop().await;
}

/// K6（D3、G8，4/F-1 的次序）：继续意图落盘 → 交活、压在准备里 → 再叫醒 → 放开 → 这一趟再次因等待上限中断 →
/// 之后没有新的继续意图：没有第二趟续接，回合停在已中断。
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn one_resume_click_one_run() {
    let mut h = harness(Opts::default()).await;
    h.start_host();
    let t = interrupted_by_wait_limit(&h, 1).await;
    let (hook, mut reached, release) = barrier();
    h.host.counters().hooks.lock().unwrap().before_prepare_read = Some(hook);
    let handed0 = h.host.counters().handed.load(Ordering::SeqCst);
    let finished0 = h.host.counters().finished.load(Ordering::SeqCst);
    assert!(matches!(
        h.ask(fresh_id(2), resume(t)).await.body,
        Some(iwork_proto::pb::response::Body::Accepted(_))
    ));
    within("续接压在准备里", reached.recv()).await;
    let r0 = h.host.counters().rounds.load(Ordering::SeqCst);
    h.host.wake();
    h.wait_rounds(r0 + 1).await;
    h.host.counters().hooks.lock().unwrap().before_prepare_read = None;
    release.notify_waiters();
    // 续接的这一趟又问了一次、又到期中断
    within("续接的一趟结束", async {
        while h.host.counters().finished.load(Ordering::SeqCst) < finished0 + 1 {
            tokio::task::yield_now().await;
        }
    })
    .await;
    assert_eq!(h.turn_state(t).await, WAIT_LIMIT);
    // 再叫醒两次，每次都等那一轮跑完
    for _ in 0..2 {
        let r = h.host.counters().rounds.load(Ordering::SeqCst);
        h.host.wake();
        h.wait_rounds(r + 1).await;
    }
    assert_eq!(
        h.host.counters().handed.load(Ordering::SeqCst),
        handed0 + 1,
        "一次「继续」只变成一趟续接"
    );
    assert_eq!(h.agent.prompt_count(), 2, "第一趟 + 续接一趟");
    assert_eq!(h.turn_state(t).await, WAIT_LIMIT);
    h.stop().await;
}

/// K7（G8）：第一趟还在准备时宿主读库（读到「已中断且有继续意图」、线占着），屏障把宿主停在读完之后 → 第一趟跑完、
/// 再次中断、线空出来 → 放宿主：趟数和读库之前记下的不一样，这一轮不交活；下一轮读到新的中断，不续接。
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn stale_read_does_not_hand_over() {
    let mut h = harness(Opts::default()).await;
    h.start_host();
    let t = interrupted_by_wait_limit(&h, 1).await;
    let (prep_hook, mut prep_reached, prep_release) = barrier();
    h.host.counters().hooks.lock().unwrap().before_prepare_read = Some(prep_hook);
    let handed0 = h.host.counters().handed.load(Ordering::SeqCst);
    let finished0 = h.host.counters().finished.load(Ordering::SeqCst);
    assert!(matches!(
        h.ask(fresh_id(2), resume(t)).await.body,
        Some(iwork_proto::pb::response::Body::Accepted(_))
    ));
    within("续接压在准备里", prep_reached.recv()).await;
    // 宿主下一轮读完库就停住
    let (read_hook, mut read_reached, read_release) = barrier();
    h.host.counters().hooks.lock().unwrap().after_round_read = Some(read_hook);
    h.host.wake();
    within("宿主停在读完之后", read_reached.recv()).await;
    h.host.counters().hooks.lock().unwrap().after_round_read = None;
    // 放第一趟：跑完、再次中断、线空出来
    h.host.counters().hooks.lock().unwrap().before_prepare_read = None;
    prep_release.notify_waiters();
    within("续接的一趟结束", async {
        while h.host.counters().finished.load(Ordering::SeqCst) < finished0 + 1 {
            tokio::task::yield_now().await;
        }
    })
    .await;
    assert_eq!(h.turn_state(t).await, WAIT_LIMIT);
    let r0 = h.host.counters().rounds.load(Ordering::SeqCst);
    read_release.notify_waiters();
    h.wait_rounds(r0 + 2).await;
    assert_eq!(
        h.host.counters().handed.load(Ordering::SeqCst),
        handed0 + 1,
        "拿着旧读的那一轮没交活"
    );
    assert_eq!(h.agent.prompt_count(), 2);
    h.stop().await;
}

/// D6：在跑的回合落一条取消意图，宿主给这一趟发取消，回合经「取消中」到「已取消」；排着的回合直接撤回、
/// 不派发、没有 prompt。之后的消息照常跑。
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn cancel_running_and_withdraw_queued() {
    let mut h = harness(Opts::default()).await;
    h.agent.hold.store(true, Ordering::SeqCst);
    h.start_host();
    let a = h.send(1, "跑着").await;
    let b = h.send(2, "排着").await;
    h.wait_state(a, |s| s == TurnState::Running).await;
    assert!(matches!(
        h.ask(fresh_id(3), cancel(b)).await.body,
        Some(iwork_proto::pb::response::Body::Accepted(_))
    ));
    assert_eq!(
        h.turn_state(b).await,
        TurnState::Cancelled,
        "排着的直接撤回"
    );
    assert!(matches!(
        h.ask(fresh_id(4), cancel(a)).await.body,
        Some(iwork_proto::pb::response::Body::Accepted(_))
    ));
    h.wait_state(a, |s| s == TurnState::Cancelled).await;
    assert_eq!(h.agent.cancels.load(Ordering::SeqCst), 1);
    assert_eq!(h.agent.prompt_count(), 1, "撤回的那个没发 prompt");
    h.agent.hold.store(false, Ordering::SeqCst);
    let c = h.send(5, "之后").await;
    h.wait_state(c, |s| s == TurnState::Completed).await;
    h.stop().await;
}

/// D5：编排在「已进 Running、提交 Completed 时存储报错」处以错误收场：宿主把回合记成 Failed，流式消息收尾。
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn settle_after_broke_run() {
    let mut h = harness(Opts::default()).await;
    h.gate.refuse_when(|r| finishes(r, TurnState::Completed));
    h.start_host();
    let t = h.send(1, "hi").await;
    h.wait_state(t, |s| s == TurnState::Failed).await;
    let evs = support::turns::events(&h.raw).await;
    assert!(
        evs.iter().any(|(_, e)| matches!(e, DomainEvent::MessageCompleted(m) if m.status != iwork_domain::message::MessageStatus::Streaming)),
        "流式中的助手消息收了尾"
    );
    assert!(
        evs.iter()
            .any(|(_, e)| matches!(e, DomainEvent::TurnFinished(f) if f.turn_id == t))
    );
    h.stop().await;
}

/// D5a：第一笔就写不进去（回合还是 Queued）：退避后再派，假运行时收到的 prompt 恰好一次。
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn queued_after_broke_is_redispatched_once() {
    let mut h = harness(Opts::default()).await;
    let refused = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let n = refused.clone();
    h.gate.refuse_when(move |r| {
        finishes(r, TurnState::Running) && n.fetch_add(1, Ordering::SeqCst) == 0
    });
    h.start_host();
    let t = h.send(1, "hi").await;
    h.wait_state(t, |s| s == TurnState::Completed).await;
    assert_eq!(
        refused.load(Ordering::SeqCst),
        2,
        "第一次被拒，退避后再派一次"
    );
    assert_eq!(h.agent.prompt_count(), 1, "prompt 恰好一次");
    h.stop().await;
}

/// D10（4/F-3）：拍改动的读一直不返回，连着跑 6 个回合：都照常到 Completed、都不带改动摘要；挂着的读始终不超过 2 个，
/// 第 3 个回合起没有起新的读。放开挂着的读之后，下一个回合又带得出摘要。
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn hung_change_reads_stay_bounded() {
    let mut h = harness(Opts {
        probe_enabled: true,
        ..Opts::default()
    })
    .await;
    h.vcs.hang.store(true, Ordering::SeqCst);
    h.start_host();
    for n in 0..6 {
        let t = h.send(n + 1, "改点东西").await;
        h.wait_state(t, |s| s == TurnState::Completed).await;
        assert!(h.budget.in_flight() <= 2, "挂着的读不超过 2 个");
    }
    assert_eq!(
        h.vcs.baselines.load(Ordering::SeqCst),
        2,
        "第 3 个回合起没有起新的读"
    );
    assert_eq!(h.vcs.hanging.load(Ordering::SeqCst), 2);
    let summaries = |evs: &[(u64, DomainEvent)]| {
        evs.iter()
            .filter(
                |(_, e)| matches!(e, DomainEvent::TurnFinished(f) if f.change_summary.is_some()),
            )
            .count()
    };
    assert_eq!(
        summaries(&support::turns::events(&h.raw).await),
        0,
        "都不带摘要"
    );
    // 放开：读结束、额度还回来；下一个回合拍得出、收尾时带摘要
    h.vcs.hang.store(false, Ordering::SeqCst);
    h.vcs.unhang.notify_waiters();
    within("额度还回来", async {
        while h.budget.in_flight() > 0 {
            tokio::task::yield_now().await;
        }
    })
    .await;
    let t = h.send(10, "再改").await;
    h.wait_state(t, |s| s == TurnState::Completed).await;
    assert_eq!(summaries(&support::turns::events(&h.raw).await), 1);
    h.stop().await;
}

fn intents(evs: &[(u64, DomainEvent)], t: iwork_domain::id::TurnId) -> usize {
    evs.iter()
        .filter(|(_, e)| matches!(e, DomainEvent::TurnIntentRecorded(i) if i.turn_id == t))
        .count()
}

/// A5、G7：同一个 `command_id` 的「继续」跨两次中断重发：回 `replayed`，回合不动，库里只有一条继续意图。
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn resume_replay_across_two_interruptions() {
    let mut h = harness(Opts::default()).await;
    h.start_host();
    let t = interrupted_by_wait_limit(&h, 1).await;
    let finished = h.host.counters().finished.load(Ordering::SeqCst);
    let first = accepted(&h.ask(fresh_id(2), resume(t)).await);
    // 续接的一趟又问、又到期中断
    within("续接的一趟结束", async {
        while h.host.counters().finished.load(Ordering::SeqCst) <= finished {
            tokio::task::yield_now().await;
        }
    })
    .await;
    assert_eq!(h.turn_state(t).await, WAIT_LIMIT);
    let again = accepted(&h.ask(fresh_id(2), resume(t)).await);
    assert_eq!(
        again,
        iwork_proto::pb::CommandAccepted {
            replayed: true,
            ..first
        }
    );
    for _ in 0..2 {
        let r = h.host.counters().rounds.load(Ordering::SeqCst);
        h.host.wake();
        h.wait_rounds(r + 1).await;
    }
    assert_eq!(h.turn_state(t).await, WAIT_LIMIT, "回合不动");
    assert_eq!(intents(&support::turns::events(&h.raw).await, t), 1);
    assert_eq!(h.agent.prompt_count(), 2);
    h.stop().await;
}

/// G7：库里留着一条旧的取消意图（比这一趟的起点小），回合中断后续接，新一趟不被取消。
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn stale_intents_ignored_without_replay() {
    let mut events = queued_with_request(970, 70, AssistantId::BUILTIN_IWORK);
    events.push(changed(70, TurnState::Queued, TurnState::Running));
    events.push(
        TurnIntentRecorded {
            turn_id: tid(70),
            conversation_id: Some(cid(CONV)),
            intent: TurnIntentKind::Cancel,
        }
        .into(),
    );
    events.push(changed(70, TurnState::Running, CRASH));
    let mut h = harness(Opts {
        extra: events,
        ..Opts::default()
    })
    .await;
    h.start_host();
    accepted(&h.ask(fresh_id(1), resume(tid(70))).await);
    h.wait_state(tid(70), |s| s == TurnState::Completed).await;
    assert_eq!(
        h.agent.cancels.load(Ordering::SeqCst),
        0,
        "旧的取消意图不打到新一趟"
    );
    h.stop().await;
}

/// A5：意图落盘后受理函数被中止、客户端不再来：续接照样发生（提交入队之后就中止受理函数）。
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn intent_survives_handler_abort() {
    let mut h = harness(Opts {
        extra: interrupted(70),
        ..Opts::default()
    })
    .await;
    h.start_host();
    let (tx, mut enq) = mpsc::unbounded_channel();
    let id = fresh_id(1);
    h.gate.on_enqueued(move |c| {
        if c == id {
            let _ = tx.send(());
        }
    });
    let d = h.d.clone();
    let task = tokio::spawn(async move {
        use iwork_ipc::server::RequestHandler;
        d.handle(
            &support::turns::ctx(),
            iwork_proto::pb::Request {
                request_id: 1,
                body: Some(iwork_proto::pb::request::Body::Command(
                    iwork_proto::pb::Command {
                        command_id: id.to_string(),
                        kind: Some(resume(tid(70))),
                    },
                )),
            },
        )
        .await
    });
    within("继续意图入队", enq.recv()).await;
    task.abort();
    let _ = task.await;
    h.wait_state(tid(70), |s| s == TurnState::Completed).await;
    h.stop().await;
}

/// A5（`CancelAll`）：在跑的、排着的一笔里都取消：在跑的到 Cancelled，排着的 Cancelled 且没被派发；
/// 那笔落盘之后才受理的新消息照常派发。
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn cancel_all_running_and_queued() {
    let mut h = harness(Opts::default()).await;
    h.agent.hold.store(true, Ordering::SeqCst);
    h.start_host();
    let a = h.send(1, "跑着").await;
    let b = h.send(2, "排着一").await;
    let c = h.send(3, "排着二").await;
    h.wait_state(a, |s| s == TurnState::Running).await;
    accepted(
        &h.ask(
            fresh_id(4),
            iwork_proto::pb::command::Kind::CancelAll(iwork_proto::pb::CancelAll {
                conversation_id: support::turns::conversation().to_string(),
            }),
        )
        .await,
    );
    for t in [a, b, c] {
        h.wait_state(t, |s| s == TurnState::Cancelled).await;
    }
    assert_eq!(h.agent.prompt_count(), 1, "排着的没被派发");
    h.agent.hold.store(false, Ordering::SeqCst);
    let d = h.send(5, "之后").await;
    h.wait_state(d, |s| s == TurnState::Completed).await;
    h.stop().await;
}

/// R1、D3a：回合在跑时进程没了 → 重启后是已中断（可继续）→ 用户继续：新建的会话线接着这个对话最后一条绑定说。
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn crash_mid_run_then_resume_continues_last_session() {
    let mut h = harness(Opts::default()).await;
    h.agent.hold.store(true, Ordering::SeqCst);
    h.start_host();
    let t = h.send(1, "跑着").await;
    h.wait_state(t, |s| s == TurnState::Running).await;
    let env = h.stop().await;
    let mut h = restart(env, Opts::default()).await;
    assert_eq!(h.turn_state(t).await, CRASH, "R1：记成已中断");
    h.start_host();
    accepted(&h.ask(fresh_id(2), resume(t)).await);
    h.wait_state(t, |s| s == TurnState::Completed).await;
    assert_eq!(
        *h.agent.opened_with.lock().unwrap(),
        vec![Some("rt-0".to_owned())]
    );
    h.stop().await;
}

/// R4、D3a（1/F-6）：A 跑完、会话编号落在 A 上；B 排着、派发之前进程没了 → 重启后 B 开会话带的是 A 的那个编号。
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn rebuilt_line_resumes_last_binding() {
    let mut h = harness(Opts::default()).await;
    h.start_host();
    let a = h.send(1, "一").await;
    h.wait_state(a, |s| s == TurnState::Completed).await;
    // B 交到会话线上、停在准备里，还没开会话
    let (hook, mut reached, _release) = barrier();
    h.host.counters().hooks.lock().unwrap().before_prepare_read = Some(hook);
    let b = h.send(2, "二").await;
    within("B 停在准备里", reached.recv()).await;
    let env = h.stop().await;
    let mut h = restart(env, Opts::default()).await;
    assert_eq!(
        h.turn_state(b).await,
        TurnState::Queued,
        "D2：没发过 prompt 的还是排着"
    );
    h.start_host();
    h.wait_state(b, |s| s == TurnState::Completed).await;
    assert_eq!(
        *h.agent.opened_with.lock().unwrap(),
        vec![Some("rt-0".to_owned())]
    );
    h.stop().await;
}

/// G7：取消意图落盘后进程没了 → 重启后回合是已中断 → 用户继续 → 迟到的原取消命令重发：回 `replayed`，新的一趟照跑。
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn cancel_replay_does_not_hit_later_run() {
    let mut h = harness(Opts::default()).await;
    h.agent.hold.store(true, Ordering::SeqCst);
    h.start_host();
    let t = h.send(1, "跑着").await;
    h.wait_state(t, |s| s == TurnState::Running).await;
    // 宿主下一轮读完库就停住：取消意图落了盘，信号还没发
    let (hook, mut reached, _release) = barrier();
    h.host.counters().hooks.lock().unwrap().after_round_read = Some(hook);
    let first = accepted(&h.ask(fresh_id(9), cancel(t)).await);
    within("宿主停在读完之后", reached.recv()).await;
    assert_eq!(h.agent.cancels.load(Ordering::SeqCst), 0);
    let env = h.stop().await;
    let mut h = restart(env, Opts::default()).await;
    assert_eq!(h.turn_state(t).await, CRASH);
    h.agent.hold.store(true, Ordering::SeqCst);
    h.start_host();
    accepted(&h.ask(fresh_id(10), resume(t)).await);
    h.wait_state(t, |s| s == TurnState::Running).await;
    let again = accepted(&h.ask(fresh_id(9), cancel(t)).await);
    assert_eq!(
        again,
        iwork_proto::pb::CommandAccepted {
            replayed: true,
            ..first
        }
    );
    h.agent.finish_one();
    h.wait_state(t, |s| s == TurnState::Completed).await;
    assert_eq!(
        h.agent.cancels.load(Ordering::SeqCst),
        0,
        "新的一趟没被取消"
    );
    h.stop().await;
}

/// D7：同一个回合两个续接来源同时有（批准已落盘没交付、用户也点了继续），只续接一次。
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn both_sources_resume_once() {
    let mut h = harness(Opts::default()).await;
    h.start_host();
    let t = interrupted_by_wait_limit(&h, 1).await;
    h.agent.ask.store(false, Ordering::SeqCst);
    // 先压住派发，让两个来源都落盘
    let (hook, mut reached, release) = barrier();
    h.host.counters().hooks.lock().unwrap().before_prepare_read = Some(hook);
    let approval = support::turns::events(&h.raw)
        .await
        .into_iter()
        .find_map(|(_, e)| match e {
            DomainEvent::ApprovalRequested(a) if a.approval.turn_id == t => Some(a.approval.id),
            _ => None,
        });
    accepted(&h.ask(fresh_id(2), resume(t)).await);
    within("续接交到线上", reached.recv()).await;
    accepted(
        &h.ask(
            fresh_id(3),
            iwork_proto::pb::command::Kind::DecideApproval(iwork_proto::pb::DecideApproval {
                approval_id: approval.unwrap().to_string(),
                decision: iwork_proto::pb::ApprovalDecision::Allow.into(),
                remember: 0,
            }),
        )
        .await,
    );
    h.host.counters().hooks.lock().unwrap().before_prepare_read = None;
    release.notify_waiters();
    h.wait_state(t, |s| s == TurnState::Completed).await;
    for _ in 0..2 {
        let r = h.host.counters().rounds.load(Ordering::SeqCst);
        h.host.wake();
        h.wait_rounds(r + 1).await;
    }
    assert_eq!(h.agent.prompt_count(), 2, "第一趟 + 只续接一次");
    h.stop().await;
}

/// D7、T06 5.2a：续接的请求转不出来（正文存成了制品、读取未接上）又碰上会话记录不在、要用摘要新开时：回合失败、
/// 原因点名，不发残缺的请求。
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn resume_request_unavailable_fails_turn() {
    let mut events = queued_with_request(970, 70, AssistantId::BUILTIN_IWORK);
    if let DomainEvent::MessageAppended(m) = &mut events[0] {
        m.message.body = iwork_domain::message::MessageBody::Artifact(artifact(
            1,
            iwork_domain::common::ArtifactArea::Artifacts,
        ));
    }
    events.push(changed(70, TurnState::Queued, TurnState::Running));
    events.push(
        iwork_domain::event::TurnSessionBound {
            turn_id: tid(70),
            conversation_id: Some(cid(CONV)),
            runtime_session_id: "rt-old".into(),
            replaced: None,
        }
        .into(),
    );
    events.push(changed(70, TurnState::Running, CRASH));
    let mut h = harness(Opts {
        extra: events,
        ..Opts::default()
    })
    .await;
    h.agent.lose_on_resume.store(true, Ordering::SeqCst);
    h.start_host();
    accepted(&h.ask(fresh_id(1), resume(tid(70))).await);
    h.wait_state(tid(70), |s| s == TurnState::Failed).await;
    let reason = support::turns::events(&h.raw)
        .await
        .into_iter()
        .find_map(|(_, e)| match e {
            DomainEvent::TurnStateChanged(s)
                if s.turn_id == tid(70) && s.to == TurnState::Failed =>
            {
                s.reason
            }
            _ => None,
        });
    assert!(
        reason.as_deref().is_some_and(|r| r.contains("制品")),
        "原因点名：{reason:?}"
    );
    assert_eq!(h.agent.prompt_count(), 0, "不发残缺的请求");
    h.stop().await;
}

/// 2.4「早到的取消认趟」宿主这一半（supervisor 2026-11-13 定的布置）：宿主发的取消带的是**当前这一趟**的趟次。
/// 第 1 趟（等审批到期中断）结束之前取消 → 收到 (T, 1)；续接的第 2 趟里取消 → 收到 (T, 2)，不是 1。
/// 乱序与过期由 iwork-core 那一层的三格证（步 2）；「只在锁里同步调用」靠 grep 与审查。
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn host_cancel_carries_run_tag() {
    use iwork_core::turn::RunTag;
    // 第 1 趟里取消
    let mut h = harness(Opts::default()).await;
    h.agent.hold.store(true, Ordering::SeqCst);
    h.start_host();
    let t = h.send(1, "跑着").await;
    h.wait_state(t, |s| s == TurnState::Running).await;
    accepted(&h.ask(fresh_id(2), cancel(t)).await);
    h.wait_state(t, |s| s == TurnState::Cancelled).await;
    assert_eq!(
        *h.host.counters().cancels_log.lock().unwrap(),
        vec![(t, RunTag(1))]
    );
    h.stop().await;
    // 第 2 趟（续接）里取消
    let mut h = harness(Opts::default()).await;
    h.start_host();
    let t = interrupted_by_wait_limit(&h, 1).await;
    // 续接的一趟又在等审批（这次不设等待上限，停在等审批上）
    *h.agent.wait_limit.lock().unwrap() = None;
    accepted(&h.ask(fresh_id(2), resume(t)).await);
    h.wait_state(t, |s| s == TurnState::WaitingApproval).await;
    accepted(&h.ask(fresh_id(3), cancel(t)).await);
    h.wait_state(t, |s| s == TurnState::Cancelled).await;
    assert_eq!(
        *h.host.counters().cancels_log.lock().unwrap(),
        vec![(t, RunTag(2))]
    );
    h.stop().await;
}

/// G8 的另一半（实现时并发负载下撞出来的）：宿主读库时回合还在跑、线占着；读完到进锁之间那一趟结束了（库里已是终态、
/// 线空出来）。拿旧读判这条线会把「占着进程却没有出错收场的编排」当成不可能、宿主致命退出。趟数变了就这一轮不动它。
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn stale_read_after_run_end_is_not_judged() {
    let mut h = harness(Opts::default()).await;
    h.agent.hold.store(true, Ordering::SeqCst);
    h.start_host();
    let a = h.send(1, "跑着").await;
    h.agent.wait_prompts(1).await;
    let (hook, mut reached, release) = barrier();
    h.host.counters().hooks.lock().unwrap().after_round_read = Some(hook);
    h.host.wake();
    within("宿主停在读完之后（读到的是 Running）", reached.recv()).await;
    h.host.counters().hooks.lock().unwrap().after_round_read = None;
    let finished = h.host.counters().finished.load(Ordering::SeqCst);
    h.agent.finish_one();
    within("那一趟结束、线空出来", async {
        while h.host.counters().finished.load(Ordering::SeqCst) <= finished {
            tokio::task::yield_now().await;
        }
    })
    .await;
    let r = h.host.counters().rounds.load(Ordering::SeqCst);
    release.notify_waiters();
    h.wait_rounds(r + 1).await;
    assert!(
        !h.host_task.as_ref().unwrap().is_finished(),
        "宿主没有因为旧读退出"
    );
    assert_eq!(h.turn_state(a).await, TurnState::Completed);
    h.agent.hold.store(false, Ordering::SeqCst);
    let b = h.send(2, "之后").await;
    h.wait_state(b, |s| s == TurnState::Completed).await;
    h.stop().await;
}

/// K17（R6、D7 ①，产品负责人 2026-11-13 定）：回合中断着 → 用户批准 → 续接之前后台没了（崩溃一格、完全退出一格）→
/// 重启：没有 prompt，回合仍是已中断；用户点「继续」→ 续接，同一次调用命中那条放行，没有再出审批卡片。
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn grant_from_previous_run_does_not_auto_resume() {
    for quit in [false, true] {
        let mut h = harness(Opts::default()).await;
        h.start_host();
        let t = interrupted_by_wait_limit(&h, 1).await;
        let approval = approval_of(&support::turns::events(&h.raw).await, t)[0];
        // 宿主下一轮读完库就停住：批准落盘了，续接还没交出去
        let (hook, mut reached, _release) = barrier();
        h.host.counters().hooks.lock().unwrap().after_round_read = Some(hook);
        accepted(&h.ask(fresh_id(2), allow(approval)).await);
        within("宿主停在读完之后", reached.recv()).await;
        assert_eq!(h.agent.prompt_count(), 1);
        if quit {
            use iwork_daemon::work::RunningWork;
            h.host.try_quit(true, None, || {});
        }
        let env = h.stop().await;
        let mut h = restart(env, Opts::default()).await;
        h.agent.ask.store(true, Ordering::SeqCst);
        h.start_host();
        for _ in 0..2 {
            let r = h.host.counters().rounds.load(Ordering::SeqCst);
            h.host.wake();
            h.wait_rounds(r + 1).await;
        }
        // 交活在对账那一轮里同步做；prompt 要等会话线准备、开会话之后才发，单看它证伪不了
        assert_eq!(
            h.host.counters().handed.load(Ordering::SeqCst),
            0,
            "重启后没有交活（quit={quit}）"
        );
        assert_eq!(h.agent.prompt_count(), 0, "重启后不自己续接（quit={quit}）");
        assert_eq!(h.turn_state(t).await, WAIT_LIMIT);
        accepted(&h.ask(fresh_id(3), resume(t)).await);
        h.wait_state(t, |s| s == TurnState::Completed).await;
        let evs = support::turns::events(&h.raw).await;
        assert_eq!(
            approval_of(&evs, t).len(),
            1,
            "同一次调用命中那条放行，没有再出审批卡片"
        );
        assert_eq!(h.agent.decisions.lock().unwrap().len(), 1);
        h.stop().await;
    }
}

/// R6 的对照：中断在重启之前、批准在重启之后 → 这次运行里落盘的批准，自动续接。
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn grant_decided_after_restart_auto_resumes() {
    let mut h = harness(Opts::default()).await;
    h.start_host();
    let t = interrupted_by_wait_limit(&h, 1).await;
    let approval = approval_of(&support::turns::events(&h.raw).await, t)[0];
    let env = h.stop().await;
    let mut h = restart(env, Opts::default()).await;
    h.start_host();
    accepted(&h.ask(fresh_id(2), allow(approval)).await);
    h.wait_state(t, |s| s == TurnState::Completed).await;
    assert_eq!(h.agent.prompt_count(), 1, "重启后批准、自动续接了一趟");
    h.stop().await;
}
