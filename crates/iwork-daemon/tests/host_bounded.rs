//! 回合宿主的读与提交「到时放手但不重发」（不变量 13）、唤醒全丢时的保底（D1–D4）、存储一直忙时的补账（D5）、
//! 在途额度按「一份被放手的工作」划分（b45 第 2 轮）。
//!
//! 「到时放手」的格子用真时间跑，宿主换一套毫秒级的时限（`Opts::limits`，正式路径的值见 `HostLimits::default`）。
//! **不要改回暂停的 tokio 时钟**：宿主和会话线等的是真 IO（SQLite 的读写线程），暂停的时钟和真 IO 混在一起时，
//! 拨钟那一下正好有一次真读在途，它的时限就被拨过去了，这一趟以「没开始」收场、退避翻倍——测出来的是调度的
//! 巧合，Linux CI 上红法每次都不一样（b45 第 2 轮实测）。

mod support;

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::time::Duration;

use iwork_daemon::host::{HOST_ACTOR, HostLimits};
use iwork_domain::event::DomainEvent;
use iwork_domain::turn::TurnState;
use support::hooked::Stall;
use support::turns::*;

/// 毫秒级的时限：一次真读远在它之内，停住的读几轮就到时。
fn fast() -> HostLimits {
    HostLimits {
        io: Duration::from_millis(800),
        backstop: Duration::from_millis(800),
        catch_up: Duration::from_millis(400),
        line_backoff_start: Duration::from_millis(50),
        line_backoff_max: Duration::from_millis(400),
        abort: Duration::from_secs(3),
    }
}

fn fast_opts() -> Opts {
    Opts {
        limits: Some(fast()),
        ..Opts::default()
    }
}

/// 等一个条件成立（真时间，20 秒为限）。
async fn wait_for(what: &str, cond: impl Fn() -> bool) {
    within(what, async {
        while !cond() {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
}

/// 放开停住的那一次，等它真的返回（在途标记、许可随之放掉），再叫醒宿主。
async fn release(h: &H, stall: &Stall) {
    stall.release();
    wait_for("停住的那一次真的返回了", || stall.all_returned()).await;
    h.host.wake();
}

fn rounds(h: &H) -> u64 {
    h.host.counters().rounds.load(Ordering::SeqCst)
}

fn handed(h: &H) -> u64 {
    h.host.counters().handed.load(Ordering::SeqCst)
}

/// 不变量 13（宿主对账的读）：读永不返回，走过几个时限：宿主照样一轮轮对账，但读只发过那一次；放开之后恢复，
/// 排着的消息照常跑完。
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn abandoned_host_read_is_not_reissued() {
    let mut h = harness(fast_opts()).await;
    h.start_host();
    h.wait_rounds(1).await;
    h.knobs.turn_work.arm();
    let before = h.knobs.turn_work.calls();
    h.host.wake();
    within("对账的读停住", h.knobs.turn_work.wait_stuck()).await;
    let r0 = rounds(&h);
    wait_for("宿主照样又对了三轮", || rounds(&h) >= r0 + 3).await;
    assert_eq!(
        h.knobs.turn_work.calls(),
        before + 1,
        "上一次读没回来，之后的轮次不发新的读"
    );
    release(&h, &h.knobs.turn_work).await;
    let t = h.send(1, "放开之后").await;
    h.wait_state(t, |s| s == TurnState::Completed).await;
    h.stop().await;
}

/// 不变量 13（宿主自己的提交）：补账那一笔（D5）永不返回，走过几个时限：宿主照样对账，补账没有再发；
/// 放开之后那一笔落盘，回合到 `Failed`，库里只有一条。
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn abandoned_host_commit_is_not_reissued() {
    let mut h = harness(fast_opts()).await;
    h.gate.refuse_when(|r| finishes(r, TurnState::Completed));
    h.knobs.commit.select(|r| r.actor.0 == HOST_ACTOR);
    h.knobs.commit.stall.arm();
    h.start_host();
    let t = h.send(1, "hi").await;
    within("补账那一笔停住", h.knobs.commit.stall.wait_stuck()).await;
    let r0 = rounds(&h);
    wait_for("宿主照样又对了三轮", || rounds(&h) >= r0 + 3).await;
    assert_eq!(h.knobs.commit.stall.calls(), 1, "上一笔没回来，补账不再发");
    assert_eq!(h.knobs.commit.passed_in.load(Ordering::SeqCst), 0);
    release(&h, &h.knobs.commit.stall).await;
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

/// 不变量 13（会话线准备的读）：准备的读永不返回：宿主一次次交活（退避后），每一趟都以「上一次的读还没回来」
/// 没开始，准备的读只发过那一次；放开之后这个回合跑完。
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn abandoned_line_read_is_not_reissued() {
    let mut h = harness(fast_opts()).await;
    h.knobs.turn_with_conversation.arm();
    h.start_host();
    let t = h.send(1, "hi").await;
    within("准备的读停住", h.knobs.turn_with_conversation.wait_stuck()).await;
    let reads = h.knobs.turn_with_conversation.calls();
    let handed0 = handed(&h);
    wait_for("宿主照样又交了三次活", || {
        handed(&h) >= handed0 + 3
    })
    .await;
    assert_eq!(
        h.knobs.turn_with_conversation.calls(),
        reads,
        "上一次准备的读没回来，之后的每一趟都不发新的读"
    );
    release(&h, &h.knobs.turn_with_conversation).await;
    h.wait_state(t, |s| s == TurnState::Completed).await;
    h.stop().await;
}

/// D1–D4：提交观察者、`engine-sync` 的唤醒全都到不了宿主时，保底照样把排着的回合派出去。
/// 判据是交活次数（同步），不是 prompt。保底给 3 秒：「之前不交活」那一下有足够的余量。
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn backstop_dispatches_when_wakes_are_suppressed() {
    let mut h = harness(Opts {
        detached_wake: true,
        limits: Some(HostLimits {
            backstop: Duration::from_secs(3),
            ..fast()
        }),
        ..Opts::default()
    })
    .await;
    h.start_host_only();
    h.wait_rounds(1).await;
    let after_round = std::time::Instant::now();
    let t = h.send(1, "没人叫醒").await;
    tokio::time::sleep(Duration::from_millis(300)).await;
    if after_round.elapsed() < Duration::from_millis(2500) {
        assert_eq!(handed(&h), 0, "唤醒压住了，保底之前不交活");
        assert_eq!(h.turn_state(t).await, TurnState::Queued);
    }
    wait_for("保底那一轮交了活", || handed(&h) == 1).await;
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

/// b45 1/F-2（宿主补账前的读）：补账要先读流式消息，这次读永不返回：宿主照样一轮轮对账，这次读没有再发；
/// 放开之后补账落盘，回合到 `Failed`。
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn unsettled_read_is_bounded() {
    let mut h = harness(fast_opts()).await;
    h.gate.refuse_when(|r| finishes(r, TurnState::Completed));
    h.knobs.unsettled.arm();
    h.start_host();
    let t = h.send(1, "hi").await;
    within("补账前的读停住", h.knobs.unsettled.wait_stuck()).await;
    let r0 = rounds(&h);
    wait_for("宿主照样又对了三轮", || rounds(&h) >= r0 + 3).await;
    assert_eq!(h.knobs.unsettled.calls(), 1, "上一次读没回来，不发新的");
    release(&h, &h.knobs.unsettled).await;
    h.wait_state(t, |s| s == TurnState::Failed).await;
    h.stop().await;
}

/// b45 1/F-2（会话线取最后一条会话绑定）：这次读永不返回：这一趟以「没开始」结束、宿主之后照样交活，这次读
/// 只发过一次；放开之后回合跑完。
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn last_binding_read_is_bounded() {
    let mut h = harness(fast_opts()).await;
    h.knobs.last_session_binding.arm();
    h.start_host();
    let t = h.send(1, "hi").await;
    within(
        "取最后一条绑定停住",
        h.knobs.last_session_binding.wait_stuck(),
    )
    .await;
    let handed0 = handed(&h);
    wait_for(
        "这条线没被占死，宿主照样又交了三次活",
        || handed(&h) >= handed0 + 3,
    )
    .await;
    assert_eq!(
        h.knobs.last_session_binding.calls(),
        1,
        "上一次读没回来，之后的每一趟都不发新的"
    );
    release(&h, &h.knobs.last_session_binding).await;
    h.wait_state(t, |s| s == TurnState::Completed).await;
    h.stop().await;
}

/// b45 2/F-1：一个对话补账前的读永不返回，别的对话排着的消息照样被派发（判据：这条消息的回合被交了活，放开
/// 之后跑完）。补账的读属于「宿主提交」这件活（B3），不占对账的读（B1）。
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn unsettled_read_does_not_block_other_conversations() {
    use iwork_ports::store::StoragePort;
    use support::fixtures::{cid, cmd, project_conversation};
    const OTHER: u128 = 999;
    let mut h = harness(fast_opts()).await;
    h.gate.refuse_when(|r| finishes(r, TurnState::Completed));
    h.knobs.unsettled.arm();
    h.start_host();
    h.send(1, "出错收场").await;
    within("补账前的读停住", h.knobs.unsettled.wait_stuck()).await;
    h.gate.stop_refusing();
    h.raw
        .commit(cmd(
            800,
            vec![
                iwork_domain::event::ConversationCreated {
                    conversation: project_conversation(OTHER, PROJECT),
                }
                .into(),
            ],
        ))
        .await
        .unwrap();
    let handed0 = handed(&h);
    let r = h
        .ask(
            fresh_id(801),
            iwork_proto::pb::command::Kind::SendMessage(iwork_proto::pb::SendMessage {
                conversation_id: cid(OTHER).to_string(),
                text: "别的对话".into(),
                attachment_ids: vec![],
            }),
        )
        .await;
    let a = accepted(&r);
    let other = events(&h.raw)
        .await
        .into_iter()
        .filter(|(s, _)| *s >= a.first_seq && *s <= a.last_seq)
        .find_map(|(_, e)| match e {
            DomainEvent::TurnQueued(q) => Some(q.turn.id),
            _ => None,
        })
        .unwrap();
    // 旧写法下对账读一直回「上一次没回来」，等多久都交不出活
    wait_for("别的对话被派发了", || handed(&h) > handed0).await;
    h.wait_state(other, |s| s == TurnState::Completed).await;
    assert_eq!(h.knobs.unsettled.calls(), 1, "补账前的读不重发");
    release(&h, &h.knobs.unsettled).await;
    h.stop().await;
}
