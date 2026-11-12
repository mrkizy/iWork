//! 验收 3a 的 `token_never_logged`：开最详细的日志跑一遍 hook 通道（令牌对的与不对的），日志里没有令牌；
//! 对照：审计日志确实写了、对端进程号搜得到。
//!
//! 为什么单独一个测试程序、装全局订阅者：
//! - 服务端的连接任务跑在 Tokio 的工作线程上，线程局部的订阅者（`set_default`）只管装它的那个线程，传不到
//!   工作线程（审查 1 指出）。全局订阅者一个进程只能装一次。
//! - 换成单线程运行时也不行：这条测试最早就是单线程运行时、和 hook_channel.rs 的测试同在一个进程，单独跑能过、
//!   一起跑漏记。tracing 的 callsite 兴趣是全局缓存的，别的线程（没装订阅者）先碰到这些日志点，会把它们定成
//!   「没人要」。
#![cfg(unix)]

mod support;

use std::sync::Arc;
use std::time::Duration;

use iwork_bridge_claude::hook_server::AUDIT_TARGET;
use iwork_bridge_claude::hook_server::pending::BackendAnswer;
use iwork_claude_hook::client::ENV_TOKEN;
use iwork_claude_hook::wire;
use support::channel::*;

/// 验收 3a：开最详细的日志跑一遍（令牌对的与不对的），日志里没有令牌；对照：对端进程号搜得到。
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn token_never_logged() {
    let logs = Captured::default();
    let subscriber = tracing_subscriber::fmt()
        .with_max_level(tracing::Level::TRACE)
        .with_writer(logs.clone())
        .with_ansi(false)
        .finish();
    tracing::subscriber::set_global_default(subscriber).expect("这个测试程序里只装一次");

    let mut b = Bridge::start(Duration::from_secs(60));
    let good = spawn_hook("pre-tool-use", &b.env(), INPUT);
    let (id, _) = b.next_call().await;
    b.pending.answer(
        id,
        BackendAnswer::Allow {
            updated_input: None,
        },
    );
    good.await.unwrap();
    let wrong = "ab".repeat(32);
    let bad = spawn_hook("pre-tool-use", &with(&b.env(), ENV_TOKEN, &wrong), INPUT)
        .await
        .unwrap();
    bad.denied();
    let sock = b.sock.clone();
    let pid = std::process::id();
    tokio::task::spawn_blocking(move || {
        raw_request(&sock, &header(wire::token_hex(&TOKEN), 1), b"{}")
    })
    .await
    .unwrap();

    let text = logs.text();
    assert!(
        text.contains(AUDIT_TARGET),
        "对照：审计日志确实写了：{text}"
    );
    assert!(
        text.contains(&pid.to_string()),
        "对照：对端进程号搜得到：{text}"
    );
    for secret in [wire::token_hex(&TOKEN), wrong] {
        assert!(!text.contains(&secret), "日志里出现了令牌：{text}");
        assert!(!text.contains(&secret.to_uppercase()));
    }
}

#[derive(Clone, Default)]
struct Captured(Arc<std::sync::Mutex<Vec<u8>>>);

impl Captured {
    fn text(&self) -> String {
        String::from_utf8_lossy(&self.0.lock().unwrap()).into_owned()
    }
}

impl std::io::Write for Captured {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(buf);
        Ok(buf.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

impl<'a> tracing_subscriber::fmt::MakeWriter<'a> for Captured {
    type Writer = Captured;
    fn make_writer(&'a self) -> Self::Writer {
        self.clone()
    }
}
