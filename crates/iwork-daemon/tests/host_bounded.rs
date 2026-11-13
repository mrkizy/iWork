//! 回合宿主的读与提交「到时放手但不重发」（不变量 13）、唤醒全丢时的保底（D1–D4）、存储一直忙时的补账（D5）。
//!
//! 等 30 秒的格子用暂停的 tokio 时钟：前面的布置照真时间走完，到了只剩宿主在等时限的那一刻才 `pause`，再一段段
//! `advance`。暂停之后不用 `within` / `wait_state`（它们自己的时限也会被拨过去），改成数宿主的对账轮数。

mod support;

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::time::Duration;

use iwork_daemon::host::{BACKSTOP, HOST_ACTOR, HOST_IO_LIMIT, LINE_BACKOFF_MAX};
use iwork_domain::event::DomainEvent;
use iwork_domain::turn::TurnState;
use support::hooked::Stall;
use support::turns::*;

/// 暂停的时钟。暂停期间起一个一直让出执行权的任务：运行时从不空闲，tokio 就不会在宿主等真 IO（SQLite）时
/// 自己把钟拨到下一个时限上，时间只在 [`walk_past`] 里走。
struct PausedClock(tokio::task::JoinHandle<()>);

impl PausedClock {
    fn start() -> Self {
        tokio::time::pause();
        Self(tokio::spawn(async {
            loop {
                tokio::task::yield_now().await;
            }
        }))
    }

    fn resume(self) {
        self.0.abort();
        tokio::time::resume();
    }
}

/// 等一个条件成立，墙钟 20 秒为限（暂停时钟下不能用 tokio 的时限）。
async fn wait_for(what: &str, cond: impl Fn() -> bool) {
    let deadline = std::time::Instant::now() + Duration::from_secs(20);
    while !cond() {
        assert!(
            std::time::Instant::now() < deadline,
            "20 秒内没等到：{what}"
        );
        tokio::task::yield_now().await;
    }
}

/// 把暂停的时钟拨过一个时限，叫醒宿主，等它跑完至少一轮对账。
async fn walk_past(h: &H, limit: Duration) {
    let r = rounds(h);
    tokio::time::advance(limit + Duration::from_secs(1)).await;
    h.host.wake();
    wait_for("宿主又跑了一轮", || rounds(h) > r).await;
}

/// 放开停住的那一次，等它真的返回（在途标记随之放掉），再拨一次钟让宿主接上，最后恢复真时间。
/// 不等它返回就叫醒的话，宿主那一轮会看到「上一次还没回来」，之后要等保底（测试里就是 30 秒真时间）。
async fn release_and_recover(h: &H, stall: &Stall, clock: PausedClock, walk: Duration) {
    stall.release();
    wait_for("停住的那一次真的返回了", || stall.all_returned()).await;
    for _ in 0..10 {
        tokio::task::yield_now().await;
    }
    walk_past(h, walk).await;
    clock.resume();
}

fn rounds(h: &H) -> u64 {
    h.host.counters().rounds.load(Ordering::SeqCst)
}

fn handed(h: &H) -> u64 {
    h.host.counters().handed.load(Ordering::SeqCst)
}

/// 不变量 13（宿主对账的读）：读永不返回，时钟走过几个 30 秒：宿主照样一轮轮对账，但读只发过那一次；
/// 放开之后恢复，排着的消息照常跑完。
#[tokio::test(flavor = "current_thread")]
async fn abandoned_host_read_is_not_reissued() {
    let mut h = harness(Opts::default()).await;
    h.start_host();
    h.wait_rounds(1).await;
    h.knobs.turn_work.arm();
    let before = h.knobs.turn_work.calls();
    h.host.wake();
    within("对账的读停住", h.knobs.turn_work.wait_stuck()).await;
    let clock = PausedClock::start();
    let r0 = rounds(&h);
    for _ in 0..4 {
        walk_past(&h, HOST_IO_LIMIT.max(BACKSTOP)).await;
    }
    assert!(
        rounds(&h) >= r0 + 3,
        "宿主照样在对账：{} → {}",
        r0,
        rounds(&h)
    );
    assert_eq!(
        h.knobs.turn_work.calls(),
        before + 1,
        "上一次读没回来，之后的轮次不发新的读"
    );
    release_and_recover(&h, &h.knobs.turn_work, clock, BACKSTOP).await;
    let t = h.send(1, "放开之后").await;
    h.wait_state(t, |s| s == TurnState::Completed).await;
    h.stop().await;
}

/// 不变量 13（宿主自己的提交）：补账那一笔（D5）永不返回，时钟走过几个 30 秒：宿主照样对账，补账没有再发；
/// 放开之后那一笔落盘，回合到 `Failed`，库里只有一条。
#[tokio::test(flavor = "current_thread")]
async fn abandoned_host_commit_is_not_reissued() {
    let mut h = harness(Opts::default()).await;
    h.gate.refuse_when(|r| finishes(r, TurnState::Completed));
    h.knobs.commit.select(|r| r.actor.0 == HOST_ACTOR);
    h.knobs.commit.stall.arm();
    h.start_host();
    let t = h.send(1, "hi").await;
    within("补账那一笔停住", h.knobs.commit.stall.wait_stuck()).await;
    let clock = PausedClock::start();
    let r0 = rounds(&h);
    for _ in 0..4 {
        walk_past(&h, HOST_IO_LIMIT.max(BACKSTOP)).await;
    }
    assert!(
        rounds(&h) >= r0 + 3,
        "宿主照样在对账：{} → {}",
        r0,
        rounds(&h)
    );
    assert_eq!(h.knobs.commit.stall.calls(), 1, "上一笔没回来，补账不再发");
    assert_eq!(h.knobs.commit.passed_in.load(Ordering::SeqCst), 0);
    release_and_recover(&h, &h.knobs.commit.stall, clock, BACKSTOP).await;
    h.wait_state(t, |s| s == TurnState::Failed).await;
    let failed = events(&h.raw)
        .await
        .iter()
        .filter(|(_, e)| {
            matches!(e, DomainEvent::TurnStateChanged(s) if s.turn_id == t && s.to == TurnState::Failed)
        })
        .count();
    assert_eq!(failed, 1, "放开之后只落了那一笔");
    h.stop().await;
}

/// 不变量 13（会话线准备的读）：准备的读永不返回，时钟走过几个 30 秒：宿主一次次交活（退避后），每一趟都以
/// 「上一次的读还没回来」没开始，准备的读只发过那一次；放开之后这个回合跑完。
#[tokio::test(flavor = "current_thread")]
async fn abandoned_line_read_is_not_reissued() {
    let mut h = harness(Opts::default()).await;
    h.knobs.turn_with_conversation.arm();
    h.start_host();
    let t = h.send(1, "hi").await;
    within("准备的读停住", h.knobs.turn_with_conversation.wait_stuck()).await;
    let reads = h.knobs.turn_with_conversation.calls();
    let clock = PausedClock::start();
    let handed0 = handed(&h);
    // 每一趟没开始之后有退避，线报回结局常在拨钟之后，紧接着那一轮退避还没到点：大约两步交一次活，走到又交了
    // 3 次为止
    for _ in 0..30 {
        if handed(&h) >= handed0 + 3 {
            break;
        }
        walk_past(&h, HOST_IO_LIMIT.max(BACKSTOP).max(LINE_BACKOFF_MAX)).await;
    }
    assert!(
        handed(&h) >= handed0 + 3,
        "宿主照样在交活：{} → {}",
        handed0,
        handed(&h)
    );
    assert_eq!(
        h.knobs.turn_with_conversation.calls(),
        reads,
        "上一次准备的读没回来，之后的每一趟都不发新的读"
    );
    release_and_recover(&h, &h.knobs.turn_with_conversation, clock, LINE_BACKOFF_MAX).await;
    h.wait_state(t, |s| s == TurnState::Completed).await;
    h.stop().await;
}

/// D1–D4：提交观察者、`engine-sync` 的唤醒全都到不了宿主时，保底的 30 秒照样把排着的回合派出去。
/// 判据是交活次数（同步），不是 prompt。
#[tokio::test(flavor = "current_thread")]
async fn backstop_dispatches_when_wakes_are_suppressed() {
    let mut h = harness(Opts {
        detached_wake: true,
        ..Opts::default()
    })
    .await;
    h.start_host_only();
    h.wait_rounds(1).await;
    let t = h.send(1, "没人叫醒").await;
    // 真时间里等一小会儿：没有唤醒，宿主不动
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert_eq!(handed(&h), 0, "唤醒压住了，保底之前不交活");
    assert_eq!(h.turn_state(t).await, TurnState::Queued);
    let clock = PausedClock::start();
    tokio::time::advance(BACKSTOP).await;
    wait_for("保底那一轮交了活", || handed(&h) == 1).await;
    clock.resume();
    h.wait_state(t, |s| s == TurnState::Completed).await;
    h.stop().await;
}

/// D5、D5a：编排以错误收场后，宿主补账的那一笔一直回 `Busy`：登记留着（回合不落终态、宿主一直在重试），
/// 存储恢复后一笔补齐，库里只有一条 `Failed` 与一条 `TurnFinished`。
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn settle_converges_after_busy() {
    let mut h = harness(Opts::default()).await;
    let busy = Arc::new(AtomicBool::new(true));
    let refused = Arc::new(AtomicUsize::new(0));
    {
        let (busy, refused) = (busy.clone(), refused.clone());
        h.gate.refuse_when(move |r| {
            if finishes(r, TurnState::Completed) {
                return true;
            }
            let host = r.actor.0 == HOST_ACTOR && busy.load(Ordering::SeqCst);
            if host {
                refused.fetch_add(1, Ordering::SeqCst);
            }
            host
        });
    }
    h.start_host();
    let t = h.send(1, "hi").await;
    within("补账被拒了几次", async {
        while refused.load(Ordering::SeqCst) < 3 {
            tokio::task::yield_now().await;
        }
    })
    .await;
    assert!(
        !h.turn_state(t).await.is_terminal(),
        "补账没落盘之前回合不落终态"
    );
    busy.store(false, Ordering::SeqCst);
    h.wait_state(t, |s| s == TurnState::Failed).await;
    let evs = events(&h.raw).await;
    let count = |f: &dyn Fn(&DomainEvent) -> bool| evs.iter().filter(|(_, e)| f(e)).count();
    assert_eq!(
        count(
            &|e| matches!(e, DomainEvent::TurnStateChanged(s) if s.turn_id == t && s.to == TurnState::Failed)
        ),
        1
    );
    assert_eq!(
        count(&|e| matches!(e, DomainEvent::TurnFinished(f) if f.turn_id == t)),
        1
    );
    h.stop().await;
}

/// b45 1/F-2（宿主补账前的读）：补账要先读流式消息，这次读永不返回，时钟走过几个 30 秒：宿主照样一轮轮对账，
/// 这次读没有再发；放开之后补账落盘，回合到 `Failed`。
#[tokio::test(flavor = "current_thread")]
async fn unsettled_read_is_bounded() {
    let mut h = harness(Opts::default()).await;
    h.gate.refuse_when(|r| finishes(r, TurnState::Completed));
    h.knobs.unsettled.arm();
    h.start_host();
    let t = h.send(1, "hi").await;
    within("补账前的读停住", h.knobs.unsettled.wait_stuck()).await;
    let clock = PausedClock::start();
    let r0 = rounds(&h);
    for _ in 0..4 {
        walk_past(&h, HOST_IO_LIMIT.max(BACKSTOP)).await;
    }
    assert!(
        rounds(&h) >= r0 + 3,
        "宿主照样在对账：{} → {}",
        r0,
        rounds(&h)
    );
    assert_eq!(h.knobs.unsettled.calls(), 1, "上一次读没回来，不发新的");
    release_and_recover(&h, &h.knobs.unsettled, clock, BACKSTOP).await;
    h.wait_state(t, |s| s == TurnState::Failed).await;
    h.wait_state(t, |s| s == TurnState::Failed).await;
    h.stop().await;
}

/// b45 1/F-2（会话线取最后一条会话绑定）：这次读永不返回，时钟走过几个 30 秒：这一趟以「没开始」结束、
/// 宿主之后照样交活，这次读只发过一次；放开之后回合跑完。
#[tokio::test(flavor = "current_thread")]
async fn last_binding_read_is_bounded() {
    let mut h = harness(Opts::default()).await;
    h.knobs.last_session_binding.arm();
    h.start_host();
    let t = h.send(1, "hi").await;
    within(
        "取最后一条绑定停住",
        h.knobs.last_session_binding.wait_stuck(),
    )
    .await;
    let clock = PausedClock::start();
    let handed0 = handed(&h);
    for _ in 0..30 {
        if handed(&h) >= handed0 + 3 {
            break;
        }
        walk_past(&h, HOST_IO_LIMIT.max(BACKSTOP).max(LINE_BACKOFF_MAX)).await;
    }
    assert!(
        handed(&h) >= handed0 + 3,
        "这条线没被占死，宿主照样在交活：{} → {}",
        handed0,
        handed(&h)
    );
    assert_eq!(
        h.knobs.last_session_binding.calls(),
        1,
        "上一次读没回来，之后的每一趟都不发新的"
    );
    release_and_recover(&h, &h.knobs.last_session_binding, clock, LINE_BACKOFF_MAX).await;
    h.wait_state(t, |s| s == TurnState::Completed).await;
    h.stop().await;
}
