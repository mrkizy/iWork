//! D7 ①：宿主读引擎的续接名单之前先让引擎追平（7.2a 的 K11）。
//!
//! 真接线下折叠很难停在旧的地方：编排每笔提交之后都让引擎追平一次，权限请求进引擎时也先追平。这里用夹具的
//! 引擎外壳 `FoldShim` 把宿主看到的名单停在一份旧的上，直到宿主自己调 `catch_up`——验的是宿主这一侧的规矩：
//! 不追平就读名单的写法，会把已经用掉的放行再拿出来续接一次。

mod support;

use std::sync::atomic::Ordering;

use iwork_daemon::host::ports::HostEngine;
use iwork_domain::turn::TurnState;
use support::turns::*;

/// K11：回合中断着 → 用户批准 → 宿主续接（放行在这一趟里用掉）→ 这一趟又因另一条审批到期中断。宿主看到的名单
/// 还停在「放行没用掉、回合已中断」那一份：先追平就看得到放行已交付，不再续接；交活次数停在续接的那一次。
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn stale_fold_does_not_resume_twice() {
    let mut h = harness(Opts::default()).await;
    h.agent.asks_per_prompt.store(2, Ordering::SeqCst);
    h.start_host();
    let t = interrupted_by_wait_limit(&h, 1).await;
    let approval = approval_of(&events(&h.raw).await, t)[0];
    // 批准落盘、宿主还没续接时的名单：里面有这个回合
    let (hook, mut reached, release) = barrier();
    h.host.counters().hooks.lock().unwrap().after_round_read = Some(hook);
    h.host.wake();
    within("宿主停在读完之后", reached.recv()).await;
    accepted(&h.ask(fresh_id(2), allow(approval)).await);
    let stale = HostEngine::turns_to_resume(&h.engine);
    assert!(stale.contains_key(&t), "前提：批准之后它在续接名单上");
    let handed0 = h.host.counters().handed.load(Ordering::SeqCst);
    h.host.counters().hooks.lock().unwrap().after_round_read = None;
    release.notify_waiters();
    within("宿主续接", async {
        while h.host.counters().handed.load(Ordering::SeqCst) == handed0 {
            tokio::task::yield_now().await;
        }
    })
    .await;
    // 从这里起宿主看到的名单停在旧的那份上，直到它调 catch_up
    *h.frozen_resume.lock().unwrap() = Some(stale);
    let finished = h.host.counters().finished.load(Ordering::SeqCst);
    within("续接的一趟又到期中断、记完结束", async {
        while h.host.counters().finished.load(Ordering::SeqCst) <= finished {
            tokio::task::yield_now().await;
        }
    })
    .await;
    assert!(
        matches!(h.turn_state(t).await, TurnState::Interrupted { .. }),
        "续接的一趟又中断了"
    );
    for _ in 0..2 {
        let r = h.host.counters().rounds.load(Ordering::SeqCst);
        h.host.wake();
        h.wait_rounds(r + 1).await;
    }
    assert_eq!(
        h.host.counters().handed.load(Ordering::SeqCst),
        handed0 + 1,
        "放行已经用掉，不再续接"
    );
    h.stop().await;
}
