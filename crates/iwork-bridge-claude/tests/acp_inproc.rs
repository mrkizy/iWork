//! 在测试进程里直接起桥接的 ACP 端（[`acp::serve`]，传输是一对内存管道），用原始 JSON-RPC 行驱动：
//! - 开会话的时限可以调短（经客户端起二进制时写死 20 秒）；
//! - 几条消息可以一口气写进去、中间不等回应（经客户端做不到这种次序）。
//!
//! 审查 S6-3 2/F-1：握手时等不到 `initialize` 的响应，开会话失败的种类是 `permission_mode`。
//! 审查 S6-4 1/F-1：同一批里 prompt 后面紧跟 cancel，取消不能丢。
#![cfg(unix)]

mod support;

use std::ffi::OsString;
use std::time::Duration;

use iwork_bridge_claude::acp::{self, BridgeConfig};
use iwork_bridge_claude::launch::BridgeEnv;
use iwork_bridge_claude::launch::env::LaunchEnv;
use serde_json::{Value, json};
use support::acp::Rig;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader, WriteHalf};
use tokio::sync::mpsc;

/// 起在测试进程里的桥接 ACP 端。
struct Inproc {
    write: WriteHalf<tokio::io::DuplexStream>,
    from_bridge: mpsc::UnboundedReceiver<String>,
    serving: tokio::task::JoinHandle<Result<(), String>>,
}

impl Inproc {
    fn start(rig: &Rig, startup_limit: Duration) -> Self {
        let config = BridgeConfig {
            data_dir: rig.data.path().to_path_buf(),
            bridge: BridgeEnv {
                cli: support::fake_claude().to_path_buf(),
                hook_program: support::hook_program().display().to_string(),
                inherited: LaunchEnv::from_vars(
                    rig.launch
                        .env
                        .iter()
                        .cloned()
                        .chain([(OsString::from("PATH"), OsString::from("/usr/bin:/bin"))])
                        .collect(),
                ),
            },
            hook_timeout: Duration::from_secs(60),
            startup_limit,
        };
        let (client, bridge) = tokio::io::duplex(1 << 20);
        let (bridge_read, _bridge_write) = tokio::io::split(bridge);
        let incoming =
            futures::stream::unfold(BufReader::new(bridge_read).lines(), |mut l| async {
                match l.next_line().await {
                    Ok(Some(line)) => Some((Ok(line), l)),
                    Ok(None) => None,
                    Err(e) => Some((Err(e), l)),
                }
            });
        let (to_test, from_bridge) = mpsc::unbounded_channel::<String>();
        let outgoing = futures::sink::unfold(to_test, |tx, line: String| async move {
            tx.send(line).map_err(std::io::Error::other)?;
            Ok::<_, std::io::Error>(tx)
        });
        let serving = tokio::spawn(acp::serve(
            config,
            agent_client_protocol::Lines::new(Box::pin(outgoing), Box::pin(incoming)),
        ));
        let (_read, write) = tokio::io::split(client);
        Self {
            write,
            from_bridge,
            serving,
        }
    }

    /// 一次写进去（几条消息之间不等任何回应）。
    async fn send(&mut self, lines: &[Value]) {
        let batch: String = lines.iter().map(|v| format!("{v}\n")).collect();
        self.write.write_all(batch.as_bytes()).await.unwrap();
    }

    async fn response(&mut self, id: u64) -> Value {
        loop {
            let line = tokio::time::timeout(Duration::from_secs(15), self.from_bridge.recv())
                .await
                .expect("15 秒内有回应")
                .expect("桥接还在");
            let v: Value = serde_json::from_str(&line).unwrap();
            if v["id"] == id {
                return v;
            }
        }
    }

    /// initialize（等它完成）+ session/new，返回 session/new 的响应。
    async fn open(&mut self, rig: &Rig) -> Value {
        self.send(&[json!({ "jsonrpc": "2.0", "id": 1, "method": "initialize",
            "params": { "protocolVersion": 1, "clientCapabilities": {} } })])
            .await;
        assert!(self.response(1).await["result"].is_object());
        let meta = iwork_agent_acp::convert::session_setup_meta(&rig.spec()).unwrap();
        self.send(
            &[json!({ "jsonrpc": "2.0", "id": 2, "method": "session/new",
            "params": { "cwd": rig.work.path(), "mcpServers": [], "_meta": meta } })],
        )
        .await;
        self.response(2).await
    }
}

impl Drop for Inproc {
    fn drop(&mut self) {
        self.serving.abort();
    }
}

#[tokio::test(flavor = "current_thread")]
async fn startup_initialize_timeout_is_permission_mode() {
    // 报到、读到 initialize 之后一直不答
    let rig = Rig::new(json!([
        { "step": "session_start" },
        { "step": "read_line" },
        { "step": "wait_stdin_eof" },
    ]));
    let mut b = Inproc::start(&rig, Duration::from_secs(1));
    let response = b.open(&rig).await;
    assert_eq!(
        response["error"]["data"]["iwork"],
        json!({ "code": "isolation_broken", "reason": "permission_mode", "session_ended": true }),
        "{response}"
    );
}

/// 审查 S6-4 1/F-1：prompt 与紧跟的 cancel 在同一批里到（中间不等回应）。会话任务得先看到 prompt、再看到
/// cancel：CLI 收到用户消息之后收到 interrupt，这一轮以 `cancelled` 收尾。修之前 prompt 在任务里才入队，
/// cancel 先到、被当成空闲时的取消丢掉，CLI 一直等不到 interrupt。
#[tokio::test(flavor = "current_thread")]
async fn prompt_then_cancel_in_one_batch_is_cancelled() {
    let rig = Rig::new(json!([
        { "step": "session_start" },
        { "step": "read_line" },
        { "step": "answer_initialize", "mode": "dontAsk" },
        { "step": "read_line" },
        { "step": "emit", "line": { "type": "system", "subtype": "init", "session_id": "s", "model": "m",
            "permissionMode": "dontAsk", "tools": [], "plugins": [] } },
        { "step": "read_line" },
        { "step": "emit", "line": { "type": "result", "subtype": "error_during_execution", "is_error": true,
            "terminal_reason": "aborted_streaming", "usage": { "input_tokens": 1, "output_tokens": 1 } } },
        { "step": "wait_stdin_eof" },
    ]));
    let mut b = Inproc::start(&rig, Duration::from_secs(5));
    let opened = b.open(&rig).await;
    let session = opened["result"]["sessionId"].clone();
    assert!(session.is_string(), "{opened}");
    b.send(&[
        json!({ "jsonrpc": "2.0", "id": 3, "method": "session/prompt",
            "params": { "sessionId": session, "prompt": [{ "type": "text", "text": "hi" }] } }),
        json!({ "jsonrpc": "2.0", "method": "session/cancel", "params": { "sessionId": session } }),
    ])
    .await;
    let response = b.response(3).await;
    assert_eq!(response["result"]["stopReason"], "cancelled", "{response}");
    let stdin: Vec<Value> = rig
        .log()
        .iter()
        .filter(|v| v["kind"] == "stdin")
        .map(|v| serde_json::from_str(v["line"].as_str().unwrap()).unwrap())
        .collect();
    assert_eq!(stdin[1]["type"], "user", "{stdin:?}");
    assert_eq!(stdin[2]["request"]["subtype"], "interrupt", "{stdin:?}");
}
