//! 在测试进程里直接起桥接的 ACP 端（[`acp::serve`]，传输是一对内存管道），开会话的时限可以调短：经客户端
//! 起二进制时它是写死的 20 秒（`STARTUP_LIMIT`）。用原始 JSON-RPC 行驱动，断言错误响应里的 `error.data.iwork`。
//!
//! 审查 S6-3 2/F-1：握手时等不到 `initialize` 的响应，开会话失败的种类是 `permission_mode`（不符与缺键两格经
//! 客户端测在 `acp.rs` 的 `startup_permission_mismatch_preserves_kind`）。
#![cfg(unix)]

mod support;

use std::ffi::OsString;
use std::time::Duration;

use iwork_bridge_claude::acp::{self, BridgeConfig};
use iwork_bridge_claude::launch::BridgeEnv;
use iwork_bridge_claude::launch::env::LaunchEnv;
use serde_json::{Value, json};
use support::acp::Rig;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

#[tokio::test(flavor = "current_thread")]
async fn startup_initialize_timeout_is_permission_mode() {
    // 报到、读到 initialize 之后一直不答
    let rig = Rig::new(json!([
        { "step": "session_start" },
        { "step": "read_line" },
        { "step": "wait_stdin_eof" },
    ]));
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
        startup_limit: Duration::from_secs(1),
    };
    let (client, bridge) = tokio::io::duplex(1 << 20);
    let (bridge_read, mut bridge_write) = tokio::io::split(bridge);
    let incoming = futures::stream::unfold(BufReader::new(bridge_read).lines(), |mut l| async {
        match l.next_line().await {
            Ok(Some(line)) => Some((Ok(line), l)),
            Ok(None) => None,
            Err(e) => Some((Err(e), l)),
        }
    });
    let (to_bridge, from_client) = tokio::sync::mpsc::unbounded_channel::<String>();
    let outgoing = futures::sink::unfold(to_bridge, |tx, line: String| async move {
        tx.send(line).map_err(std::io::Error::other)?;
        Ok::<_, std::io::Error>(tx)
    });
    let serving = tokio::spawn(acp::serve(
        config,
        agent_client_protocol::Lines::new(Box::pin(outgoing), Box::pin(incoming)),
    ));
    // 桥接写出的行：从 mpsc 里取；发给桥接的行：写进内存管道
    let mut from_bridge = from_client;
    let (_client_read, mut client_write) = tokio::io::split(client);
    let mut send = async |v: Value| {
        client_write
            .write_all(format!("{v}\n").as_bytes())
            .await
            .unwrap();
    };
    let meta = iwork_agent_acp::convert::session_setup_meta(&rig.spec()).unwrap();
    send(json!({ "jsonrpc": "2.0", "id": 1, "method": "initialize",
        "params": { "protocolVersion": 1, "clientCapabilities": {} } }))
    .await;
    let mut response_to = async |id: u64| loop {
        let line = tokio::time::timeout(Duration::from_secs(15), from_bridge.recv())
            .await
            .expect("15 秒内有回应")
            .expect("桥接还在");
        let v: Value = serde_json::from_str(&line).unwrap();
        if v["id"] == id {
            break v;
        }
    };
    // initialize 完成之后才能开会话
    assert!(response_to(1).await["result"].is_object());
    send(json!({ "jsonrpc": "2.0", "id": 2, "method": "session/new",
        "params": { "cwd": rig.work.path(), "mcpServers": [], "_meta": meta } }))
    .await;
    let response = response_to(2).await;
    assert_eq!(
        response["error"]["data"]["iwork"],
        json!({ "code": "isolation_broken", "reason": "permission_mode" }),
        "{response}"
    );
    let _ = bridge_write.shutdown().await;
    serving.abort();
}
