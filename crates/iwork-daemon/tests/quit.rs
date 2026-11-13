//! 退出与停机（T10a 方案 E1–E4、G3）：真存储 + 真引擎 + 假运行时上的回合宿主。

mod support;

use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::Duration;

use iwork_daemon::work::{QuitCounts, RunningWork, ShutdownDecision};
use iwork_domain::event::DomainEvent;
use iwork_domain::turn::TurnState;
use support::turns::*;
use tokio::sync::{Notify, mpsc};

fn barrier() -> (
    iwork_daemon::host::HostHook,
    mpsc::UnboundedReceiver<()>,
    Arc<Notify>,
) {
    let (tx, rx) = mpsc::unbounded_channel();
    let release = Arc::new(Notify::new());
    let r = release.clone();
    let hook: iwork_daemon::host::HostHook = Arc::new(move || {
        let (tx, r) = (tx.clone(), r.clone());
        Box::pin(async move {
            let go = r.notified();
            let _ = tx.send(());
            go.await;
        })
    });
    (hook, rx, release)
}

fn turn_events(evs: &[(u64, DomainEvent)]) -> usize {
    evs.iter()
        .filter(|(_, e)| {
            matches!(
                e,
                DomainEvent::TurnStateChanged(_)
                    | DomainEvent::TurnFinished(_)
                    | DomainEvent::TurnSessionBound(_)
            )
        })
        .count()
}

/// E2 ③（P2 a），演练「完全退出时一个在跑两个排着」：带确认退出 → 三个回合都是 Cancelled，用户消息都还在；
/// 重启后没有回合自己跑起来。
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn quit_cancels_running_and_queued() {
    let mut h = harness(Opts::default()).await;
    h.agent.hold.store(true, Ordering::SeqCst);
    h.start_host();
    let a = h.send(1, "跑着").await;
    let b = h.send(2, "排着一").await;
    let c = h.send(3, "排着二").await;
    h.wait_state(a, |s| s == TurnState::Running).await;
    let triggered = std::sync::atomic::AtomicBool::new(false);
    let d = h
        .host
        .try_quit(true, None, || triggered.store(true, Ordering::SeqCst));
    assert_eq!(d, ShutdownDecision::Proceed);
    assert!(triggered.load(Ordering::SeqCst));
    tokio::time::timeout(Duration::from_secs(5), h.host.cancel_all())
        .await
        .expect("cancel_all 5 秒内走完");
    for t in [a, b, c] {
        assert_eq!(h.turn_state(t).await, TurnState::Cancelled);
    }
    assert_eq!(h.agent.prompt_count(), 1, "排着的没被派发");
    let users = events(&h.raw)
        .await
        .iter()
        .filter(|(_, e)| matches!(e, DomainEvent::MessageAppended(m) if m.message.author == iwork_domain::message::Author::User))
        .count();
    assert_eq!(users, 3, "用户消息都还在对话里");
    let report = h.host.abort_all().await;
    assert_eq!(report.not_cleared, 0);
    let env = h.stop().await;
    let mut h = restart(env, Opts::default()).await;
    h.start_host();
    for _ in 0..2 {
        let r = h.host.counters().rounds.load(Ordering::SeqCst);
        h.host.wake();
        h.wait_rounds(r + 1).await;
    }
    assert_eq!(
        h.host.counters().handed.load(Ordering::SeqCst),
        0,
        "重启后没有回合自己跑起来"
    );
    h.stop().await;
}

/// E1：只有排着的回合（前面一个已中断）时不带确认退出：回「要确认」，数目里有排着的那一个。
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn quit_counts_include_queued() {
    let mut h = harness(Opts::default()).await;
    h.agent.ask.store(true, Ordering::SeqCst);
    *h.agent.wait_limit.lock().unwrap() = Some(Duration::from_millis(50));
    h.start_host();
    let t = h.send(1, "推一下").await;
    h.wait_state(t, |s| matches!(s, TurnState::Interrupted { .. }))
        .await;
    h.send(2, "排在中断的后面").await;
    let counts = h.host.quit_counts().await;
    assert_eq!(
        counts,
        Some(QuitCounts {
            running_turns: 0,
            queued_turns: 1,
            executing_cards: 0
        })
    );
    let d = h.host.try_quit(false, counts, || panic!("不该触发停机"));
    assert!(
        matches!(
            d,
            ShutdownDecision::NeedsConfirmation {
                counts_unknown: false,
                ..
            }
        ),
        "{d:?}"
    );
    assert!(!h.host.is_closed(), "要确认时宿主不关");
    h.stop().await;
}

/// G3 两种先后。① 回合先过了派发边界：读得 N = 0 也回去问（L ≥ 1）。② `try_quit` 先关了宿主：停在准备里的那一趟
/// 再也过不了边界，没有 prompt；`cancel_all` 把它取消。
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn quit_vs_begin_run_two_orders() {
    // ①
    let mut h = harness(Opts::default()).await;
    h.agent.hold.store(true, Ordering::SeqCst);
    h.start_host();
    let a = h.send(1, "跑着").await;
    h.wait_state(a, |s| s == TurnState::Running).await;
    let d = h
        .host
        .try_quit(false, Some(QuitCounts::default()), || panic!("不该触发"));
    assert!(
        matches!(d, ShutdownDecision::NeedsConfirmation { counts, .. } if counts.running_turns == 1)
    );
    h.stop().await;
    // ②
    let mut h = harness(Opts::default()).await;
    let (hook, mut reached, release) = barrier();
    h.host.counters().hooks.lock().unwrap().before_prepare_read = Some(hook);
    h.start_host();
    let t = h.send(1, "准备中").await;
    within("停在准备里", reached.recv()).await;
    let d = h.host.try_quit(false, Some(QuitCounts::default()), || {});
    assert_eq!(d, ShutdownDecision::Proceed, "还没过边界的不算 L");
    h.host.counters().hooks.lock().unwrap().before_prepare_read = None;
    release.notify_waiters();
    tokio::time::timeout(Duration::from_secs(5), h.host.cancel_all())
        .await
        .expect("cancel_all 5 秒内走完");
    assert_eq!(h.turn_state(t).await, TurnState::Cancelled);
    assert_eq!(h.agent.prompt_count(), 0, "关了以后过不了边界，没有 prompt");
    assert!(h.agent.opened_with.lock().unwrap().is_empty(), "也没开会话");
    h.stop().await;
}

/// E3、E4：信号停机（不走取消，直接封口）：没有新的回合事件落盘，回合留在库里的样子（重启后由崩溃恢复记成已中断），
/// 进程树清了，报告如实；封口后受理函数的提交入不了队，回可重试的错误。
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn signal_stop_writes_no_turn_events() {
    let mut h = harness(Opts::default()).await;
    h.agent.hold.store(true, Ordering::SeqCst);
    h.start_host();
    let a = h.send(1, "跑着").await;
    h.wait_state(a, |s| s == TurnState::Running).await;
    let before = turn_events(&events(&h.raw).await);
    let report = tokio::time::timeout(Duration::from_secs(3), h.host.abort_all())
        .await
        .expect("abort_all 3 秒内回来");
    assert_eq!(report.sessions, 1);
    assert_eq!(report.not_cleared, 0);
    h.agent.finish_one();
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert_eq!(
        turn_events(&events(&h.raw).await),
        before,
        "封口后没有新的回合事件"
    );
    assert_eq!(h.turn_state(a).await, TurnState::Running);
    let r = h
        .ask(
            fresh_id(9),
            iwork_proto::pb::command::Kind::SetPlayfulUi(iwork_proto::pb::SetPlayfulUi {
                enabled: false,
            }),
        )
        .await;
    match r.body {
        Some(iwork_proto::pb::response::Body::Error(e)) => {
            assert_eq!(e.code, iwork_proto::pb::ErrorCode::Unavailable as i32);
            assert!(e.retryable);
        }
        other => panic!("封口后入不了队：{other:?}"),
    }
    let env = h.stop().await;
    let h = restart(env, Opts::default()).await;
    assert!(
        matches!(h.turn_state(a).await, TurnState::Interrupted { .. }),
        "R1"
    );
    h.stop().await;
}

/// E3：会话线停在准备的读上时封口：3 秒内报告回来，不等那次读；之后没有 prompt。
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn seal_does_not_wait_for_stuck_prepare_read() {
    let mut h = harness(Opts::default()).await;
    let (hook, mut reached, _release) = barrier();
    h.host.counters().hooks.lock().unwrap().before_prepare_read = Some(hook);
    h.start_host();
    h.send(1, "准备中").await;
    within("停在准备里", reached.recv()).await;
    tokio::time::timeout(Duration::from_secs(3), h.host.abort_all())
        .await
        .expect("不等准备里那次读");
    assert_eq!(h.agent.prompt_count(), 0);
    h.stop().await;
}

fn resume(t: iwork_domain::id::TurnId) -> iwork_proto::pb::command::Kind {
    iwork_proto::pb::command::Kind::ResumeTurn(iwork_proto::pb::ResumeTurn {
        turn_id: t.to_string(),
    })
}

/// K10（D7、R7）：中断 → 继续意图落盘 → 续接过了 `begin_run`、停在开会话里、第一笔还没提交 → 带确认退出 → 放开：
/// 没有 prompt，回合还是已中断；正常退出后重启，回合不自己续接。
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn resume_cancelled_before_start_does_not_rerun_after_restart() {
    let mut h = harness(Opts::default()).await;
    h.agent.ask.store(true, Ordering::SeqCst);
    *h.agent.wait_limit.lock().unwrap() = Some(Duration::from_millis(50));
    h.start_host();
    let t = h.send(1, "推一下").await;
    let interrupted = |s| matches!(s, TurnState::Interrupted { .. });
    h.wait_state(t, interrupted).await;
    // 等第一趟在锁里记完结束
    within("第一趟记完结束", async {
        while h.host.counters().finished.load(Ordering::SeqCst) < 1 {
            tokio::task::yield_now().await;
        }
    })
    .await;
    h.agent.open_hold.store(true, Ordering::SeqCst);
    let opening = h.agent.opening.notified();
    let r = h.ask(fresh_id(2), resume(t)).await;
    assert!(matches!(
        r.body,
        Some(iwork_proto::pb::response::Body::Accepted(_))
    ));
    within("续接停在开会话里", opening).await;
    assert_eq!(
        h.host.try_quit(true, None, || {}),
        ShutdownDecision::Proceed
    );
    let cancel = tokio::spawn({
        let host = h.host.clone();
        async move { host.cancel_all().await }
    });
    // 取消已经发给这一趟（它过了边界）；放开开会话
    within("取消发给了停在开会话里的那一趟", async {
        while h.host.counters().cancels_sent.load(Ordering::SeqCst) < 1 {
            tokio::task::yield_now().await;
        }
    })
    .await;
    h.agent.open_release.notify_waiters();
    within("cancel_all 走完", cancel).await.unwrap();
    assert!(
        interrupted(h.turn_state(t).await),
        "续接的第一笔没提交：原样已中断"
    );
    assert_eq!(h.agent.prompt_count(), 1, "续接那一趟没有 prompt");
    h.host.abort_all().await;
    let env = h.stop().await;
    let mut h = restart(env, Opts::default()).await;
    h.start_host();
    for _ in 0..2 {
        let r = h.host.counters().rounds.load(Ordering::SeqCst);
        h.host.wake();
        h.wait_rounds(r + 1).await;
    }
    assert!(interrupted(h.turn_state(t).await), "重启后不自己续接（R7）");
    // 交活是对账那一轮里同步做的；prompt 要等会话线准备、开会话之后才发，两轮之后不一定已经发出，单看它证伪不了
    assert_eq!(
        h.host.counters().handed.load(Ordering::SeqCst),
        0,
        "重启后没有交活"
    );
    assert_eq!(h.agent.prompt_count(), 0);
    h.stop().await;
}

/// KA1（E2 ③ 守不住的一角，照实断言）：完全退出时存储一直忙，排着的回合留在排队；`cancel_all` 照样返回；
/// 重启后它们被派发。在跑的照样被取消（取消信号不读库）。
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn queued_survive_quit_when_store_is_stuck() {
    let mut h = harness(Opts::default()).await;
    h.agent.hold.store(true, Ordering::SeqCst);
    h.start_host();
    let a = h.send(1, "跑着").await;
    let b = h.send(2, "排着").await;
    h.wait_state(a, |s| s == TurnState::Running).await;
    // 撤回排着的那一笔一直记不下
    h.gate.refuse_when(|r| {
        r.events.iter().any(|e| {
            matches!(e, DomainEvent::TurnStateChanged(s) if s.from == TurnState::Queued && s.to == TurnState::Cancelled)
        })
    });
    assert_eq!(
        h.host.try_quit(true, None, || {}),
        ShutdownDecision::Proceed
    );
    tokio::time::timeout(Duration::from_secs(5), h.host.cancel_all())
        .await
        .expect("cancel_all 照样返回");
    assert_eq!(
        h.turn_state(a).await,
        TurnState::Cancelled,
        "在跑的照样被取消"
    );
    assert_eq!(
        h.turn_state(b).await,
        TurnState::Queued,
        "排着的留在排队（KA1）"
    );
    h.host.abort_all().await;
    let env = h.stop().await;
    let mut h = restart(env, Opts::default()).await;
    h.start_host();
    h.wait_state(b, |s| s == TurnState::Completed).await;
    h.stop().await;
}

/// A6：宿主是「完全退出」之后，会起新活的命令（`SendMessage`、`ResumeTurn`）提前回可重试的 `UNAVAILABLE`，什么都不提交；
/// 不起新活的照常受理（对照）。
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn closed_host_refuses_new_work() {
    let h = harness(Opts::default()).await;
    assert_eq!(
        h.host.try_quit(true, None, || {}),
        ShutdownDecision::Proceed
    );
    let head = h.raw.head_seq().await.unwrap();
    for kind in [
        iwork_proto::pb::command::Kind::SendMessage(iwork_proto::pb::SendMessage {
            conversation_id: conversation().to_string(),
            text: "晚到".into(),
            attachment_ids: vec![],
        }),
        resume(iwork_domain::id::TurnId::from_raw(
            iwork_domain::id::RawUlid::from_u128(5),
        )),
    ] {
        match h.ask(fresh_id(1), kind).await.body {
            Some(iwork_proto::pb::response::Body::Error(e)) => {
                assert_eq!(e.code, iwork_proto::pb::ErrorCode::Unavailable as i32);
                assert!(e.retryable);
            }
            other => panic!("{other:?}"),
        }
    }
    use iwork_ports::store::StoragePort;
    assert_eq!(h.raw.head_seq().await.unwrap(), head, "什么都没提交");
    let ok = h
        .ask(
            fresh_id(2),
            iwork_proto::pb::command::Kind::SetPlayfulUi(iwork_proto::pb::SetPlayfulUi {
                enabled: true,
            }),
        )
        .await;
    assert!(matches!(
        ok.body,
        Some(iwork_proto::pb::response::Body::Accepted(_))
    ));
    h.stop().await;
}

/// E4（提交路径表第 2 条）：封口之后，经引擎提交的受理（`DecideApproval`）入不了队，回可重试的 `UNAVAILABLE`；
/// 库里没有新的审批决定，审批还在待审批。
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn sealed_decide_returns_retryable() {
    let mut h = harness(Opts::default()).await;
    h.agent.ask.store(true, Ordering::SeqCst);
    *h.agent.wait_limit.lock().unwrap() = Some(Duration::from_millis(50));
    h.start_host();
    let t = h.send(1, "推一下").await;
    h.wait_state(t, |s| matches!(s, TurnState::Interrupted { .. }))
        .await;
    let approval = events(&h.raw)
        .await
        .into_iter()
        .find_map(|(_, e)| match e {
            DomainEvent::ApprovalRequested(a) => Some(a.approval.id),
            _ => None,
        })
        .unwrap();
    h.host.abort_all().await;
    let r = h
        .ask(
            fresh_id(2),
            iwork_proto::pb::command::Kind::DecideApproval(iwork_proto::pb::DecideApproval {
                approval_id: approval.to_string(),
                decision: iwork_proto::pb::ApprovalDecision::Deny.into(),
                remember: 0,
            }),
        )
        .await;
    match r.body {
        Some(iwork_proto::pb::response::Body::Error(e)) => {
            assert_eq!(
                e.code,
                iwork_proto::pb::ErrorCode::Unavailable as i32,
                "{}",
                e.message
            );
            assert!(e.retryable);
        }
        other => panic!("封口后入不了队：{other:?}"),
    }
    assert!(
        !events(&h.raw)
            .await
            .iter()
            .any(|(_, e)| matches!(e, DomainEvent::ApprovalDecided(_))),
        "没有新的审批决定"
    );
    h.stop().await;
}
