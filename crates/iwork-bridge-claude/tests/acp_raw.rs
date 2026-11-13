//! 直接起桥接二进制、用原始的 ACP 行驱动它（不经 `iwork-agent-acp` 客户端：客户端关会话时会给进程组发信号、
//! 清树，测不到「对端正常断开」，也拿不到桥接自己的 stderr）。
//!
//! - 审查 S6-3 1/F-4：对端正常断开（关掉桥接的 stdin），会话任务被丢掉，会话目录照样删掉、CLI 照样结束。
//! - 审查 S6-3 1/F-7：二进制装了日志订阅器，诊断写在 stderr；最详细的级别下 stderr 里也没有令牌。
#![cfg(unix)]

mod support;

use std::io::{BufRead, BufReader, Write};
use std::process::{Child, ChildStdin, ChildStdout, Command, Stdio};
use std::time::{Duration, Instant};

use serde_json::{Value, json};
use support::acp::{BRIDGE, Rig, opening, read_user, wait_eof};

/// 起好的桥接：写请求、按 id 读响应。
struct Raw {
    child: Child,
    stdin: Option<ChildStdin>,
    stdout: BufReader<ChildStdout>,
}

impl Raw {
    fn start(rig: &Rig, log_level: &str) -> Self {
        let mut cmd = Command::new(BRIDGE);
        cmd.arg("acp")
            .envs(rig.launch.env.iter().map(|(k, v)| (k, v)))
            .env("IWORK_BRIDGE_LOG", log_level)
            .env("IWORK_SUPERVISOR_PID", std::process::id().to_string())
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        let mut child = cmd.spawn().expect("起得了桥接");
        let stdin = child.stdin.take();
        let stdout = BufReader::new(child.stdout.take().unwrap());
        Self {
            child,
            stdin,
            stdout,
        }
    }

    fn send(&mut self, v: Value) {
        let stdin = self.stdin.as_mut().unwrap();
        writeln!(stdin, "{v}").unwrap();
        stdin.flush().unwrap();
    }

    /// 读到 id 对得上的那条响应（跳过通知）。
    fn response(&mut self, id: u64) -> Value {
        loop {
            let mut line = String::new();
            assert!(
                self.stdout.read_line(&mut line).unwrap() > 0,
                "桥接提前关了 stdout"
            );
            let v: Value = serde_json::from_str(&line).unwrap();
            if v["id"] == id {
                return v;
            }
        }
    }

    fn open(&mut self, rig: &Rig) -> Value {
        self.send(json!({ "jsonrpc": "2.0", "id": 1, "method": "initialize",
            "params": { "protocolVersion": 1, "clientCapabilities": {} } }));
        assert!(self.response(1)["result"].is_object());
        let meta = iwork_agent_acp::convert::session_setup_meta(&rig.spec()).unwrap();
        self.send(json!({ "jsonrpc": "2.0", "id": 2, "method": "session/new",
            "params": { "cwd": rig.work.path(), "mcpServers": [], "_meta": meta } }));
        self.response(2)
    }

    /// 关掉 stdin（对端正常断开），等桥接退出，返回它的 stderr。
    fn hang_up(mut self) -> String {
        drop(self.stdin.take());
        let started = Instant::now();
        while self.child.try_wait().unwrap().is_none() {
            assert!(started.elapsed() < Duration::from_secs(10), "10 秒内退出");
            std::thread::sleep(Duration::from_millis(20));
        }
        let mut err = String::new();
        std::io::Read::read_to_string(self.child.stderr.as_mut().unwrap(), &mut err).unwrap();
        err
    }
}

fn session_dirs(rig: &Rig) -> usize {
    std::fs::read_dir(rig.data.path().join("run").join("h"))
        .map(|d| d.count())
        .unwrap_or(0)
}

/// F-4：会话开着时对端断开 → 会话目录删掉、CLI 结束。修之前会话 future 被丢掉，目录留着。
#[test]
fn peer_hang_up_removes_the_session_dir() {
    let mut steps = opening();
    steps.extend([read_user(), wait_eof()]);
    let rig = Rig::new(Value::Array(steps));
    let mut raw = Raw::start(&rig, "info");
    assert!(raw.open(&rig)["result"]["sessionId"].is_string());
    assert_eq!(session_dirs(&rig), 1, "会话开着时目录在");
    let cli_pid = rig
        .log()
        .into_iter()
        .find(|v| v["kind"] == "started")
        .unwrap()["pid"]
        .as_i64()
        .unwrap() as i32;
    raw.hang_up();
    assert_eq!(session_dirs(&rig), 0, "断开后目录删了");
    let started = Instant::now();
    // SAFETY: 信号 0 只探测这个进程号在不在
    while unsafe { libc::kill(cli_pid, 0) } == 0 {
        assert!(started.elapsed() < Duration::from_secs(5), "CLI 也结束了");
        std::thread::sleep(Duration::from_millis(20));
    }
}

/// F-7：诊断写在 stderr（对照：开会话失败的那条 warn 在），最详细的级别下也没有令牌。
#[test]
fn diagnostics_go_to_stderr_without_the_token() {
    // 假 CLI 报到之后、答 initialize 之前退出：开会话失败，桥接记一条 warn
    let rig = Rig::new(json!([
        { "step": "session_start" },
        { "step": "read_line" },
        { "step": "exit", "code": 3 },
    ]));
    let mut raw = Raw::start(&rig, "trace");
    let resp = raw.open(&rig);
    assert_eq!(
        resp["error"]["data"]["iwork"]["code"], "cli_exited",
        "{resp}"
    );
    let stderr = raw.hang_up();
    assert!(stderr.contains("开会话失败"), "诊断写在了 stderr：{stderr}");
    assert!(stderr.contains("cli_exited"), "{stderr}");
    let env: Value =
        serde_json::from_str(&std::fs::read_to_string(rig.record.path().join("env.json")).unwrap())
            .unwrap();
    let token = env["IWORK_HOOK_TOKEN"].as_str().unwrap();
    assert_eq!(token.len(), 64);
    assert!(!stderr.contains(token), "令牌不进日志");
    assert!(
        !stderr
            .to_lowercase()
            .contains(&token.to_uppercase().to_lowercase())
    );
}
