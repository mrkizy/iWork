//! 「这一趟的取消算不算数」只有一个判断（b45 1/F-1）：补账（D5）和发取消（D6）共用。依据四样，任一成立就算：
//! 已落盘的有效取消意图、对话在删、宿主完全退出、本趟已发过取消信号。
//!
//! 三格都让编排在宿主发出取消信号**之前**以错误收场（`Completed` 那一笔回 `Busy`），再让宿主补账：
//! 只看「发没发过信号」的写法会记成 `Failed`。

mod support;

use std::sync::atomic::Ordering;

use iwork_daemon::work::RunningWork;
use iwork_domain::event::DomainEvent;
use iwork_domain::turn::TurnState;
use iwork_proto::pb;
use support::turns::*;

#[derive(Debug, Clone, Copy)]
enum Basis {
    /// 用户的取消意图落盘了，宿主还没来得及发信号
    Intent,
    /// 对话进了删除
    Deleting,
    /// 带确认的完全退出
    Quit,
}

async fn broke_before_cancel_signal(basis: Basis) {
    let mut h = harness(Opts::default()).await;
    h.agent.hold.store(true, Ordering::SeqCst);
    h.gate.refuse_when(|r| finishes(r, TurnState::Completed));
    h.start_host();
    let t = h.send(1, "跑着").await;
    h.agent.wait_prompts(1).await;
    // 宿主停在读完库之后：这期间落盘的意图它这一轮看不到，信号也就发不出去
    let (hook, mut reached, release) = barrier();
    h.host.counters().hooks.lock().unwrap().after_round_read = Some(hook);
    h.host.wake();
    within("宿主停在读完之后", reached.recv()).await;
    if let Basis::Intent = basis {
        accepted(&h.ask(fresh_id(2), cancel(t)).await);
    }
    // 编排收尾：Completed 那一笔回 Busy，以错误收场
    h.agent.finish_one();
    within("编排以错误收场", async {
        while h.host.counters().finished.load(Ordering::SeqCst) == 0 {
            tokio::task::yield_now().await;
        }
    })
    .await;
    match basis {
        Basis::Intent => {}
        Basis::Deleting => {
            accepted(
                &h.ask(
                    fresh_id(3),
                    pb::command::Kind::DeleteConversation(pb::DeleteConversation {
                        conversation_id: conversation().to_string(),
                    }),
                )
                .await,
            );
        }
        Basis::Quit => {
            h.host.try_quit(true, None, || {});
        }
    }
    assert_eq!(
        h.host.counters().cancels_sent.load(Ordering::SeqCst),
        0,
        "{basis:?}：信号确实没发过"
    );
    h.host.counters().hooks.lock().unwrap().after_round_read = None;
    release.notify_waiters();
    let end = h.wait_state(t, |s| s.is_terminal()).await;
    assert_eq!(end, TurnState::Cancelled, "{basis:?}：补账按取消收");
    let finished: Vec<TurnState> = events(&h.raw)
        .await
        .into_iter()
        .filter_map(|(_, e)| match e {
            DomainEvent::TurnFinished(f) if f.turn_id == t => Some(f.state),
            _ => None,
        })
        .collect();
    assert_eq!(finished, [TurnState::Cancelled], "{basis:?}");
    h.stop().await;
}

/// 1/F-1 原格：取消意图落盘、宿主还没发信号，编排先以错误收场 → `Cancelled`。
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn settle_counts_committed_cancel_intent() {
    broke_before_cancel_signal(Basis::Intent).await;
}

/// 编排以错误收场之后对话进了删除 → 补账按取消收。
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn settle_counts_deleting_conversation() {
    broke_before_cancel_signal(Basis::Deleting).await;
}

/// 编排以错误收场之后带确认完全退出 → 补账按取消收。
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn settle_counts_quit() {
    broke_before_cancel_signal(Basis::Quit).await;
}
