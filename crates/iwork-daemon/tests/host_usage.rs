//! 用量口径 b（步 7）：结束事件与快照里回合的用量是累计。续接的回合由宿主把上一次记下的累计填进
//! `TurnEntry::Resume.recorded_usage`，这一趟报的用量接着加。

mod support;

use std::sync::atomic::Ordering;

use iwork_domain::common::Usage;
use iwork_domain::event::DomainEvent;
use iwork_domain::turn::TurnState;
use iwork_ports::read_model::ConversationPaging;
use support::turns::*;

fn tokens(n: u64) -> Usage {
    Usage {
        input_tokens: n,
        output_tokens: n,
        ..Usage::default()
    }
}

/// 中断前这一趟报了用量（记下累计 10）→ 用户点继续 → 续接的一趟又报 10 → 结束：结束事件带 20，快照的
/// 本对话用量也是 20。
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn resumed_turn_carries_cumulative_usage() {
    let mut h = harness(Opts::default()).await;
    *h.agent.usage_per_prompt.lock().unwrap() = Some(tokens(10));
    h.start_host();
    let t = interrupted_by_wait_limit(&h, 1).await;
    h.agent.ask.store(false, Ordering::SeqCst);
    accepted(&h.ask(fresh_id(2), resume(t)).await);
    h.wait_state(t, |s| s == TurnState::Completed).await;
    let finished: Vec<Usage> = events(&h.raw)
        .await
        .into_iter()
        .filter_map(|(_, e)| match e {
            DomainEvent::TurnFinished(f) if f.turn_id == t => Some(f.usage),
            _ => None,
        })
        .collect();
    assert_eq!(finished, [tokens(20)], "结束事件带回合累计");
    let page = support::fixtures::read_model(&h.raw)
        .view(|v| Ok(v.conversation_page(conversation(), ConversationPaging::Latest { limit: 50 })))
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    assert_eq!(page.usage_total, Some(tokens(20)), "快照的本对话用量是累计");
    h.stop().await;
}
