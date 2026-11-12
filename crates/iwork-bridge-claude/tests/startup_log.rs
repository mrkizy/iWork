//! 方案 3.5、15.3：`initialize_response_account_never_logged`。假 CLI 的 `initialize` 响应里带一个假邮箱
//! （和真 CLI 一样放在 `account` 下），开最详细的日志跑一遍开会话，日志里搜不到它；对照：权限模式的值搜得到。
//!
//! 单独一个测试程序、装全局订阅者，原因同 hook_token_log.rs。
#![cfg(unix)]

mod support;

use std::sync::{Arc, Mutex};

use serde_json::json;
use support::startup::World;

const EMAIL: &str = "t06-startup@example.invalid";

#[derive(Clone, Default)]
struct Captured(Arc<Mutex<Vec<u8>>>);

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

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn initialize_response_account_never_logged() {
    let logs = Captured::default();
    tracing::subscriber::set_global_default(
        tracing_subscriber::fmt()
            .with_max_level(tracing::Level::TRACE)
            .with_writer(logs.clone())
            .with_ansi(false)
            .finish(),
    )
    .expect("这个测试程序里只装一次");

    let w = World::new(json!([
        { "step": "session_start" },
        { "step": "read_line" },
        { "step": "answer_initialize", "mode": "dontAsk",
          "extra": { "account": { "email": EMAIL, "organization": "Fake Org" } } },
        { "step": "wait_stdin_eof" },
    ]));
    let mut s = w.open().await.expect("开会话成功");
    s.cli.kill().await.unwrap();
    s.dir.remove().unwrap();

    let text = String::from_utf8_lossy(&logs.0.lock().unwrap()).into_owned();
    assert!(text.contains("dontAsk"), "对照：权限模式的值搜得到：{text}");
    assert!(!text.contains(EMAIL), "日志里出现了邮箱：{text}");
    assert!(!text.contains("Fake Org"), "日志里出现了组织：{text}");
    assert!(
        s.early_lines.iter().all(|l| !l.contains(EMAIL)),
        "initialize 响应不进留给映射的行"
    );
}
