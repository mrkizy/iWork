//! 审查 1/F-1：后台的允许已经交进连接任务的信箱、hook 也已经断开，两样同时就绪时要报 `AllowUndelivered`。
//!
//! 单独一个测试程序：断开靠的是测试关掉客户端的 fd。同一个进程里别的测试在起子进程时，fork 会把这个 fd 复制给
//! 子进程，要到它 exec 才关（CLOEXEC）；那段时间里连接其实没断，服务端写答复会成功，这条测试就假红
//! （和 hook_channel.rs 放在一起时 20 次里红了 7 次）。这个程序里没有别的线程起进程。
#![cfg(unix)]

mod support;

use std::time::Duration;

use iwork_bridge_claude::hook_server::HookEvent;
use iwork_bridge_claude::hook_server::pending::{BackendAnswer, Delivery};
use support::channel::*;

/// 审查 1/F-1：后台的允许已经交进连接任务的信箱、hook 也已经断开，两样同时就绪。不管先处理哪一样，
/// 这条允许都没送到，要报 `AllowUndelivered`。单线程运行时：关连接与答复都在服务端任务被轮询之前做完。
#[tokio::test(flavor = "current_thread")]
async fn eof_and_allow_ready_together_reports_undelivered_allow() {
    let mut b = Bridge::start(Duration::from_secs(60));
    let conn = sent_request(&b.sock);
    let (id, _) = b.next_call().await;
    drop(conn);
    assert_eq!(
        b.pending.answer(
            id,
            BackendAnswer::Allow {
                updated_input: None
            }
        ),
        Delivery::Delivered
    );
    assert!(matches!(b.next_event().await, HookEvent::AllowUndelivered { id: got } if got == id));
}
