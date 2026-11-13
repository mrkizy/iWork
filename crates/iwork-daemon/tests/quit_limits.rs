//! 退出与停机的时限与撞车（T10a 方案 E1–E4、G3）：读不出数目时怎么问、在跑的 5 秒内收掉、封口撞上宿主正在等读或
//! 等提交。判据都是同步可数的（交活、发取消、入队、库里的事件），不拿「没收到 prompt」当红灯。

mod support;

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use iwork_daemon::host::HOST_ACTOR;
use iwork_daemon::stop::StopSwitch;
use iwork_daemon::work::{RunningWork, ShutdownDecision};
use iwork_domain::event::DomainEvent;
use iwork_domain::turn::TurnState;
use iwork_ipc::rendezvous::StopReason;
use iwork_proto::pb;
use support::turns::*;

async fn shutdown(h: &H, stop: &StopSwitch, confirmed: bool) -> pb::Response {
    iwork_daemon::handler::lifecycle::shutdown(
        &ctx(),
        1,
        &pb::ShutdownDaemon {
            cancel_running: confirmed,
        },
        &h.host,
        stop,
    )
    .await
}

/// E1、G3：存储的读永不返回。没带确认：2 秒内回「要确认」并标明数目未知，宿主不关；再问一次立刻回、不发新的读。
/// 带确认：立刻受理、一次读都不发，宿主置成完全退出并触发停机。
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn quit_with_stuck_store_reads() {
    let mut h = harness(Opts::default()).await;
    h.agent.hold.store(true, Ordering::SeqCst);
    h.start_host();
    let t = h.send(1, "跑着").await;
    h.wait_state(t, |s| s == TurnState::Running).await;
    h.knobs.quit_counts.arm();
    let stop = StopSwitch::new();
    let started = Instant::now();
    let r = shutdown(&h, &stop, false).await;
    let took = started.elapsed();
    let Some(pb::response::Body::Error(e)) = &r.body else {
        panic!("应回要确认：{r:?}")
    };
    assert_eq!(e.code, pb::ErrorCode::PreconditionFailed as i32);
    assert_eq!(
        e.details
            .get(iwork_app::details::COUNTS_UNKNOWN)
            .map(String::as_str),
        Some("true")
    );
    assert!(took < Duration::from_millis(2500), "2 秒内回来：{took:?}");
    assert!(!h.host.is_closed(), "没确认，宿主照开");
    assert_eq!(stop.requested(), None);
    // 再问一次：上一次读还没回来，立刻回「不知道」，不发新的读
    let started = Instant::now();
    let _ = shutdown(&h, &stop, false).await;
    assert!(started.elapsed() < Duration::from_millis(500));
    assert_eq!(h.knobs.quit_counts.calls(), 1, "上一次读没回来，不发新的");
    // 带确认：不读
    let r = shutdown(&h, &stop, true).await;
    assert!(
        matches!(r.body, Some(pb::response::Body::Accepted(_))),
        "{r:?}"
    );
    assert_eq!(h.knobs.quit_counts.calls(), 1, "带确认不读库");
    assert!(h.host.is_closed());
    assert_eq!(stop.requested(), Some(StopReason::UserQuit));
    h.knobs.quit_counts.release();
    h.stop().await;
}

/// E2、E3：完全退出时在跑的回合 5 秒内到 `Cancelled`（取消信号恰好发了一次，运行时收到一次）。
/// 运行时不理取消、5 秒内没走完的：封口、重启之后是 `Cancelled`（停在取消中的由崩溃恢复收成已取消）。
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn quit_cancels_running_within_limit() {
    // 走得完的一格
    let mut h = harness(Opts::default()).await;
    h.agent.hold.store(true, Ordering::SeqCst);
    h.start_host();
    let t = h.send(1, "跑着").await;
    h.agent.wait_prompts(1).await;
    assert_eq!(
        h.host.try_quit(true, None, || {}),
        ShutdownDecision::Proceed
    );
    tokio::time::timeout(Duration::from_secs(5), h.host.cancel_all())
        .await
        .expect("5 秒内走完");
    assert_eq!(h.turn_state(t).await, TurnState::Cancelled);
    assert_eq!(h.host.counters().cancels_sent.load(Ordering::SeqCst), 1);
    assert_eq!(h.agent.cancels.load(Ordering::SeqCst), 1);
    h.stop().await;

    // 走不完的一格
    let mut h = harness(Opts::default()).await;
    h.agent.hold.store(true, Ordering::SeqCst);
    h.agent.ignore_cancel.store(true, Ordering::SeqCst);
    h.start_host();
    let t = h.send(1, "不理取消").await;
    h.agent.wait_prompts(1).await;
    h.host.try_quit(true, None, || {});
    assert!(
        tokio::time::timeout(Duration::from_secs(5), h.host.cancel_all())
            .await
            .is_err(),
        "运行时不理取消，5 秒内走不完"
    );
    assert_eq!(h.agent.cancels.load(Ordering::SeqCst), 1, "取消发过");
    h.host.abort_all().await;
    let env = h.stop().await;
    let h = restart(env, Opts::default()).await;
    assert_eq!(h.turn_state(t).await, TurnState::Cancelled);
    h.stop().await;
}

/// 封口之后从带闸存储入队的提交数（`on_enqueued` 只数真的入了队的）。
fn count_enqueues(h: &H) -> Arc<AtomicUsize> {
    let n = Arc::new(AtomicUsize::new(0));
    let c = n.clone();
    h.gate.on_enqueued(move |_| {
        c.fetch_add(1, Ordering::SeqCst);
    });
    n
}

/// E2–E4：封口撞上宿主正在等对账的读：放开之后这一轮不再交活、不发取消、不提交。
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn seal_during_host_read_wait() {
    let mut h = harness(Opts::default()).await;
    h.start_host();
    h.wait_rounds(1).await;
    h.knobs.turn_work.arm();
    h.host.wake();
    within("对账的读停住", h.knobs.turn_work.wait_stuck()).await;
    // 读停住期间来了一条消息：读要是放开后照常往下走，就会把它派出去
    let t = h.send(1, "排着").await;
    let handed = h.host.counters().handed.load(Ordering::SeqCst);
    tokio::time::timeout(Duration::from_secs(3), h.host.abort_all())
        .await
        .expect("封口 3 秒内回来");
    let enqueued = count_enqueues(&h);
    h.knobs.turn_work.release();
    within("停住的读真的返回了", async {
        while !h.knobs.turn_work.all_returned() {
            tokio::task::yield_now().await;
        }
    })
    .await;
    h.host.wake();
    tokio::time::sleep(Duration::from_millis(300)).await;
    let c = h.host.counters();
    assert_eq!(c.handed.load(Ordering::SeqCst), handed, "封口后不交活");
    assert_eq!(c.cancels_sent.load(Ordering::SeqCst), 0, "封口后不发取消");
    assert_eq!(enqueued.load(Ordering::SeqCst), 0, "封口后没有入队的提交");
    assert_eq!(h.turn_state(t).await, TurnState::Queued);
    h.stop().await;
}

/// E2–E4：封口撞上宿主正在等自己的提交（D5 补账那一笔停在入队之前）：放开之后那一笔入不了队，库里没有
/// `Failed`，之后也没有新发起的提交。
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn seal_during_host_commit_wait() {
    let mut h = harness(Opts::default()).await;
    h.gate.refuse_when(|r| finishes(r, TurnState::Completed));
    h.knobs.commit.select(|r| r.actor.0 == HOST_ACTOR);
    h.knobs.commit.stall.arm();
    h.start_host();
    let t = h.send(1, "hi").await;
    within("补账那一笔停住", h.knobs.commit.stall.wait_stuck()).await;
    tokio::time::timeout(Duration::from_secs(3), h.host.abort_all())
        .await
        .expect("封口 3 秒内回来");
    let enqueued = count_enqueues(&h);
    h.knobs.commit.stall.release();
    within("停住的那一笔真的返回了", async {
        while !h.knobs.commit.stall.all_returned() {
            tokio::task::yield_now().await;
        }
    })
    .await;
    h.host.wake();
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert_eq!(enqueued.load(Ordering::SeqCst), 0, "放开后那一笔入不了队");
    assert!(
        !events(&h.raw).await.iter().any(|(_, e)| matches!(
            e,
            DomainEvent::TurnStateChanged(s) if s.turn_id == t && s.to == TurnState::Failed
        )),
        "库里没有 Failed"
    );
    assert_eq!(h.host.counters().cancels_sent.load(Ordering::SeqCst), 0);
    h.stop().await;
}

/// b45 2/F-3：宿主自己的提交合计至多一笔在途（方案 0a.8：补账、删除的最后一笔、退出时撤排队）。补账那一笔停在入队
/// 之前时完全退出：撤排队的那一笔不同时发出（同时停着的宿主提交始终只有 1 笔）；补账那一笔回来以后它照常提交，
/// 排着的回合是 `Cancelled`。
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn quit_withdraw_shares_host_commit_budget() {
    let mut h = harness(Opts::default()).await;
    h.gate.refuse_when(|r| finishes(r, TurnState::Completed));
    h.knobs.commit.select(|r| r.actor.0 == HOST_ACTOR);
    h.knobs.commit.stall.arm();
    h.start_host();
    h.send(1, "出错收场").await;
    within("补账那一笔停住", h.knobs.commit.stall.wait_stuck()).await;
    h.gate.stop_refusing();
    let queued = h.send(2, "排着").await;
    h.host.try_quit(true, None, || {});
    let quit = {
        let host = h.host.clone();
        tokio::spawn(async move { host.cancel_all().await })
    };
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert_eq!(
        h.knobs.commit.stall.stuck.load(Ordering::SeqCst),
        1,
        "同时在途的宿主提交只有 1 笔（停着的数）：调用 {} 次",
        h.knobs.commit.stall.calls()
    );
    h.knobs.commit.stall.release();
    within("cancel_all 走完", quit).await.unwrap();
    h.wait_state(queued, |s| s == TurnState::Cancelled).await;
    h.stop().await;
}

/// b45 2/F-2 的一种交错：一条线清完树、报回了「清空了」，还没退出时读快照：这条线只算一次、算清空了。
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn abort_snapshot_counts_reported_line_once() {
    let mut h = harness(Opts::default()).await;
    h.start_host();
    let t = h.send(1, "跑完").await;
    h.wait_state(t, |s| s == TurnState::Completed).await;
    let (hook, mut reached, release) = barrier();
    h.host.counters().hooks.lock().unwrap().after_abort_report = Some(hook);
    let abort = {
        let host = h.host.clone();
        tokio::spawn(async move { host.abort_all().await })
    };
    within("报回之后、退出之前", reached.recv()).await;
    let snapshot = h.host.abort_snapshot();
    release.notify_waiters();
    let report = within("封口回来", abort).await.unwrap();
    assert_eq!(snapshot.sessions, 1, "{snapshot:?}");
    assert_eq!(
        snapshot.not_cleared, 0,
        "已报清空的不再算成没清空：{snapshot:?}"
    );
    assert_eq!(report.not_cleared, 0);
    h.stop().await;
}

/// b45 2/F-2 的另一种交错：快照读到一半时，一条线报回「没清空」并退出：快照要么还把它算作没报回的（没清空），
/// 要么算进它报回的失败——都是没清空 1 条，不会两头都没数到。
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn abort_snapshot_does_not_miss_failure_reported_mid_read() {
    let mut h = harness(Opts::default()).await;
    h.agent.fail_terminate.store(true, Ordering::SeqCst);
    h.start_host();
    let t = h.send(1, "跑完").await;
    h.wait_state(t, |s| s == TurnState::Completed).await;
    let (entered_tx, entered_rx) = std::sync::mpsc::channel::<()>();
    let (go_tx, go_rx) = std::sync::mpsc::channel::<()>();
    *h.host.counters().during_snapshot.lock().unwrap() = Some(Box::new(move || {
        let _ = entered_tx.send(());
        let _ = go_rx.recv();
    }));
    let snapshot = {
        let host = h.host.clone();
        tokio::task::spawn_blocking(move || host.abort_snapshot())
    };
    tokio::task::spawn_blocking(move || entered_rx.recv().unwrap())
        .await
        .unwrap();
    // 快照读到一半：这时封口，这条线清树失败、报回、退出
    let report = tokio::time::timeout(Duration::from_secs(3), h.host.abort_all())
        .await
        .expect("封口 3 秒内回来");
    assert_eq!(report.not_cleared, 1, "前提：这条线没清空：{report:?}");
    go_tx.send(()).unwrap();
    let snapshot = snapshot.await.unwrap();
    assert_eq!(
        snapshot.not_cleared, 1,
        "读到一半时报回的失败没有漏掉：{snapshot:?}"
    );
    h.stop().await;
}
