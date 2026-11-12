//! hook 通道测试共用：起桥接服务端、起真的 hook 进程、直接对 socket 发请求。

use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::Arc;
use std::time::Duration;

use iwork_bridge_claude::hook_server::pending::{CallId, Pending};
use iwork_bridge_claude::hook_server::{HookEvent, HookServer, Token};
use iwork_claude_hook::client::{ENV_BRIDGE_PID, ENV_DEADLINE, ENV_SOCK, ENV_TOKEN};
use iwork_claude_hook::wire::{self, HookKind, RequestHeader};
use serde_json::Value;
use tokio::io::AsyncWriteExt;
use tokio::sync::mpsc::UnboundedReceiver;
use tokio::task::JoinHandle;

pub const TOKEN: [u8; wire::TOKEN_BYTES] = [0x3c; wire::TOKEN_BYTES];
pub const INPUT: &str =
    r#"{"tool_name":"Bash","tool_use_id":"toolu_1","tool_input":{"command":"ls"}}"#;

pub struct Bridge {
    pub _dir: tempfile::TempDir,
    pub sock: PathBuf,
    pub pending: Arc<Pending>,
    pub events: UnboundedReceiver<HookEvent>,
    pub server: JoinHandle<()>,
}

impl Bridge {
    pub fn start(backstop: Duration) -> Self {
        let dir = tempfile::tempdir().unwrap();
        let sock = dir.path().join("s");
        let (server, events) =
            HookServer::bind(&sock, Token::from_bytes(TOKEN), backstop).expect("绑定成功");
        let pending = server.pending();
        Self {
            _dir: dir,
            sock,
            pending,
            events,
            server: tokio::spawn(server.serve()),
        }
    }

    pub fn env(&self) -> Vec<(&'static str, String)> {
        vec![
            (ENV_SOCK, self.sock.display().to_string()),
            (ENV_TOKEN, wire::token_hex(&TOKEN)),
            (ENV_BRIDGE_PID, std::process::id().to_string()),
            (ENV_DEADLINE, "60".into()),
        ]
    }

    pub async fn next_event(&mut self) -> HookEvent {
        tokio::time::timeout(Duration::from_secs(10), self.events.recv())
            .await
            .expect("10 秒内有事件")
            .expect("服务端还在")
    }

    pub async fn next_call(&mut self) -> (CallId, Vec<u8>) {
        match self.next_event().await {
            HookEvent::Call { id, input } => (id, input),
            other => panic!("要的是工具调用，来的是 {other:?}"),
        }
    }

    pub async fn no_event(&mut self) -> bool {
        tokio::time::timeout(Duration::from_millis(300), self.events.recv())
            .await
            .is_err()
    }
}

/// hook 跑完的结果。
pub struct Ran {
    pub code: Option<i32>,
    pub stdout: String,
}

impl Ran {
    pub fn decision(&self) -> (String, String) {
        let v: Value = serde_json::from_str(self.stdout.trim())
            .unwrap_or_else(|e| panic!("stdout 是合法 JSON（{e}）：{:?}", self.stdout));
        let out = &v["hookSpecificOutput"];
        assert_eq!(out["hookEventName"], "PreToolUse", "{v}");
        (
            out["permissionDecision"].as_str().unwrap().to_owned(),
            out["permissionDecisionReason"]
                .as_str()
                .unwrap_or_default()
                .to_owned(),
        )
    }

    /// 合法的拒绝 JSON、退出码 0（验收 3 的共同断言），返回原因。
    pub fn denied(&self) -> String {
        assert_eq!(self.code, Some(0), "stdout={:?}", self.stdout);
        let (d, reason) = self.decision();
        assert_eq!(d, "deny", "{:?}", self.stdout);
        reason
    }
}

/// 起一个 hook 进程（环境只给这几个变量），写 stdin，等它退出。
pub fn spawn_hook(sub: &str, env: &[(&'static str, String)], stdin: &str) -> JoinHandle<Ran> {
    let mut cmd = tokio::process::Command::new(super::hook_program());
    cmd.arg(sub)
        .env_clear()
        .envs(env.iter().map(|(k, v)| (*k, v.as_str())))
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .kill_on_drop(true);
    let mut child = cmd.spawn().expect("起得了 hook");
    let mut input = child.stdin.take().unwrap();
    let stdin = stdin.to_owned();
    tokio::spawn(async move {
        input.write_all(stdin.as_bytes()).await.unwrap();
        drop(input);
        let out = tokio::time::timeout(Duration::from_secs(30), child.wait_with_output())
            .await
            .expect("hook 30 秒内退出")
            .unwrap();
        Ran {
            code: out.status.code(),
            stdout: String::from_utf8_lossy(&out.stdout).into_owned(),
        }
    })
}

pub fn with(
    env: &[(&'static str, String)],
    name: &str,
    value: &str,
) -> Vec<(&'static str, String)> {
    env.iter()
        .map(|(k, v)| {
            if *k == name {
                (*k, value.to_owned())
            } else {
                (*k, v.clone())
            }
        })
        .collect()
}

/// 直接对着 socket 发一个请求（不经 hook 程序），读回答复。答复读不到（连接被关）返回 `None`。
pub fn raw_request(sock: &Path, header: &RequestHeader, body: &[u8]) -> Option<wire::Response> {
    let mut conn = std::os::unix::net::UnixStream::connect(sock).unwrap();
    conn.set_read_timeout(Some(Duration::from_secs(10)))
        .unwrap();
    wire::write_request(&mut conn, header, body).ok()?;
    wire::read_response(&mut conn).ok()
}

pub fn header(token: String, pid: u32) -> RequestHeader {
    RequestHeader {
        kind: HookKind::PreToolUse,
        token,
        pid,
    }
}
