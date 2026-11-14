//! 直接起桥接二进制、用原始的 ACP 行驱动它（不经 `iwork-agent-acp` 客户端：客户端关会话时会给进程组发信号、
//! 清树，测不到「对端正常断开」，也拿不到桥接自己的 stderr）。
//!
//! - 审查 S6-3 1/F-4：对端正常断开（关掉桥接的 stdin），CLI 照样结束；会话目录要么桥接当场删了，要么（收尾线程
//!   先把进程组杀了，方案 3.3、5.4a 第 5 条第二种）留下、由下一个桥接开会话时清扫掉，要么删到一半留下一个空目录
//!   （已知限制）。留下的文件里没有令牌。
//! - 审查 S6-3 1/F-7：二进制装了日志订阅器，诊断写在 stderr；最详细的级别下 stderr 里也没有令牌。
#![cfg(unix)]

mod support;

use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
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

/// 唯一的那个会话目录。
fn the_session_dir(rig: &Rig) -> PathBuf {
    let mut dirs: Vec<PathBuf> = std::fs::read_dir(rig.data.path().join("run").join("h"))
        .unwrap()
        .map(|e| e.unwrap().path())
        .collect();
    assert_eq!(dirs.len(), 1, "{dirs:?}");
    dirs.pop().unwrap()
}

/// 这次会话的令牌（假 CLI 把拿到的环境记在 env.json 里）。
fn session_token(rig: &Rig) -> String {
    let env: Value =
        serde_json::from_str(&std::fs::read_to_string(rig.record.path().join("env.json")).unwrap())
            .unwrap();
    let token = env["IWORK_HOOK_TOKEN"].as_str().unwrap().to_owned();
    assert_eq!(token.len(), 64, "对照：拿到的是真令牌");
    token
}

/// 会话目录下（含子目录）每个普通文件都不含令牌。返回读过的文件名，给调用方做对照。socket 不是普通文件，读不了，跳过。
fn assert_no_token_in(dir: &Path, token: &str) -> Vec<String> {
    let mut read = Vec::new();
    let mut todo = vec![dir.to_path_buf()];
    while let Some(d) = todo.pop() {
        for entry in std::fs::read_dir(&d).unwrap() {
            let path = entry.unwrap().path();
            let meta = std::fs::symlink_metadata(&path).unwrap();
            if meta.is_dir() {
                todo.push(path);
            } else if meta.is_file() {
                let bytes = std::fs::read(&path).unwrap();
                let text = String::from_utf8_lossy(&bytes).to_lowercase();
                assert!(!text.contains(token), "{} 里有令牌", path.display());
                read.push(path.file_name().unwrap().to_string_lossy().into_owned());
            }
        }
    }
    read
}

/// 进程号还在不在（只用于桥接：它是测试的子进程，`hang_up` 已经回收过，不会是僵尸）。
fn pid_alive(pid: i32) -> bool {
    // SAFETY: 信号 0 只探测这个进程号在不在
    unsafe { libc::kill(pid, 0) == 0 }
}

/// F-4：会话开着时对端断开 → CLI 结束；会话目录的三种结果见函数里的注释，哪一种都不留令牌。
#[test]
fn peer_hang_up_removes_the_session_dir() {
    let mut steps = opening();
    steps.extend([read_user(), wait_eof()]);
    let rig = Rig::new(Value::Array(steps));
    let mut raw = Raw::start(&rig, "info");
    assert!(raw.open(&rig)["result"]["sessionId"].is_string());
    assert_eq!(session_dirs(&rig), 1, "会话开着时目录在");
    let dir = the_session_dir(&rig);
    let token = session_token(&rig);
    let read = assert_no_token_in(&dir, &token);
    for name in ["settings.json", "mcp.json", "owner"] {
        assert!(
            read.iter().any(|r| r == name),
            "对照：读到了 {name}：{read:?}"
        );
    }
    let cli_pid = rig
        .log()
        .into_iter()
        .find(|v| v["kind"] == "started")
        .unwrap()["pid"]
        .as_i64()
        .unwrap() as i32;
    raw.hang_up();
    // 断开后会话目录有三种结果（桥接删目录和收尾线程连组 SIGKILL 之间没有先后，方案 3.3、5.4a 第 5 条第二种）：
    // 1. 删完了；
    // 2. 还没开始删、或删到 owner 之前就被杀：目录留着、owner 在，它记的桥接已经不在，下一个桥接开会话时清扫掉；
    // 3. 删到 owner 之后、rmdir 之前被杀（owner 是最后删的文件）：留下一个空目录、没有 owner。清扫读不到 owner
    //    就跳过，这个空目录不会被删（已知限制，2026-11-14 supervisor 定 A'；要不要让清扫也删放久了的空目录待
    //    产品负责人定）。留下的只是空目录，没有任何文件。
    let swept = if !dir.exists() {
        false
    } else if dir.join("owner").exists() {
        let owner: i32 = std::fs::read_to_string(dir.join("owner"))
            .unwrap()
            .trim()
            .parse()
            .unwrap();
        assert!(!pid_alive(owner), "留下的目录记的桥接 {owner} 已经不在");
        assert_no_token_in(&dir, &token);
        true
    } else {
        let left: Vec<_> = std::fs::read_dir(&dir).unwrap().collect();
        assert!(left.is_empty(), "没有 owner 的残留目录是空的：{left:?}");
        false
    };
    // 下一个桥接开会话：第 2 种留下的目录被清扫掉
    let mut next = Raw::start(&rig, "info");
    assert!(next.open(&rig)["result"]["sessionId"].is_string());
    if swept {
        assert!(!dir.exists(), "上一个会话的目录被清扫掉了");
    }
    next.hang_up();
    let started = Instant::now();
    // SAFETY: 信号 0 只探测这个进程号在不在
    while unsafe { libc::kill(cli_pid, 0) } == 0 {
        assert!(started.elapsed() < Duration::from_secs(5), "CLI 也结束了");
        std::thread::sleep(Duration::from_millis(20));
    }
}

/// 方案 3.3 的清扫：上次被 SIGKILL 的桥接留下的会话目录（属主进程已经不在），下一个桥接开会话时删掉，自己的照常建。
#[test]
fn opening_a_session_sweeps_a_dead_bridges_dir() {
    use std::os::unix::fs::DirBuilderExt;
    let mut steps = opening();
    steps.extend([read_user(), wait_eof()]);
    let rig = Rig::new(Value::Array(steps));
    let root = rig.data.path().join("run").join("h");
    std::fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(&root)
        .unwrap();
    // 一个已经结束、回收过的进程号
    let mut gone = Command::new("true").spawn().unwrap();
    let dead = gone.id();
    gone.wait().unwrap();
    let left = root.join("0123456789ab");
    std::fs::DirBuilder::new()
        .mode(0o700)
        .create(&left)
        .unwrap();
    std::fs::DirBuilder::new()
        .mode(0o700)
        .create(left.join("m"))
        .unwrap();
    std::fs::write(left.join("owner"), dead.to_string()).unwrap();
    std::fs::write(left.join("settings.json"), "{}").unwrap();
    std::fs::write(left.join("mcp.json"), r#"{"mcpServers":{}}"#).unwrap();
    assert_eq!(session_dirs(&rig), 1, "对照：留下的目录在");
    let mut raw = Raw::start(&rig, "info");
    assert!(raw.open(&rig)["result"]["sessionId"].is_string());
    assert!(!left.exists(), "属主已经不在的目录被清扫掉");
    assert_eq!(session_dirs(&rig), 1, "只剩这一个会话的目录");
    raw.hang_up();
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
