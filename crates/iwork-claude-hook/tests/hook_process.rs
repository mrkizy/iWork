//! 真的 hook 程序对着一个假桥接（同一测试进程里的本机 socket 服务端）：T06 方案 4.6 里 hook 一侧的
//! 每一行，以及允许、改写、报到。每条都断言 hook 的输出是合法的结论 JSON。
//!
//! 第 15 行（hook panic）在 `tests/panic_denies.rs`。
//! 「别的用户连进来」（第 5 行）是桥接一侧的检查，随桥接在 S6 测。
#![cfg(unix)]

use std::io::{Read, Write};
use std::os::unix::net::UnixListener;
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::sync::mpsc;
use std::time::{Duration, Instant};

use iwork_claude_hook::client::{ENV_BRIDGE_PID, ENV_DEADLINE, ENV_SOCK, ENV_TOKEN};
use iwork_claude_hook::output::DENIED_WITHOUT_REASON;
use iwork_claude_hook::peer;
use iwork_claude_hook::wire::{self, Decision, HookKind, RequestHeader, Response};
use serde_json::{Value, json};

const HOOK: &str = env!("CARGO_BIN_EXE_iwork-claude-hook");
const TOKEN: [u8; wire::TOKEN_BYTES] = [0x5a; wire::TOKEN_BYTES];

/// 假桥接收到的一次请求。
#[derive(Debug)]
struct Seen {
    header: RequestHeader,
    body: Vec<u8>,
    /// 对端已经走了时取不到
    peer: Option<peer::PeerCred>,
}

/// 假桥接怎么答。
enum Reply {
    Respond(Response),
    /// 原样写这些字节（造坏帧）
    Raw(Vec<u8>),
    /// 读完请求就关连接（桥接崩溃）
    Close,
    /// 读完请求就一直不答
    Hang,
}

struct Bridge {
    _dir: tempfile::TempDir,
    sock: PathBuf,
    /// 每条连接一项：读到了请求是 `Some`，对端没发请求就关了是 `None`
    seen: mpsc::Receiver<Option<Seen>>,
}

impl Bridge {
    fn start(reply: impl Fn(&RequestHeader) -> Reply + Send + 'static) -> Self {
        let dir = tempfile::tempdir().unwrap();
        let sock = dir.path().join("s");
        let listener = UnixListener::bind(&sock).unwrap();
        let (tx, seen) = mpsc::channel();
        std::thread::spawn(move || {
            for conn in listener.incoming() {
                let Ok(mut conn) = conn else { return };
                // 对端可能已经关了连接（对端校验不过的 hook 连上就走）
                let peer = peer::peer_cred(&conn).ok();
                let header = match wire::read_request_header(&mut conn) {
                    Ok(h) => h,
                    Err(_) => {
                        let _ = tx.send(None);
                        continue;
                    }
                };
                let body = wire::read_request_body(&mut conn).unwrap();
                let action = reply(&header);
                let _ = tx.send(Some(Seen { header, body, peer }));
                match action {
                    Reply::Respond(r) => {
                        let _ = wire::write_response(&mut conn, &r);
                    }
                    Reply::Raw(bytes) => {
                        let _ = conn.write_all(&bytes);
                    }
                    Reply::Close => drop(conn),
                    // 攥着连接不答；这个假桥接只服务这一次
                    Reply::Hang => std::thread::sleep(Duration::from_secs(120)),
                }
            }
        });
        Self {
            _dir: dir,
            sock,
            seen,
        }
    }

    fn answering(r: Response) -> Self {
        Self::start(move |_| Reply::Respond(r.clone()))
    }

    fn env(&self) -> Vec<(&'static str, String)> {
        vec![
            (ENV_SOCK, self.sock.display().to_string()),
            (ENV_TOKEN, wire::token_hex(&TOKEN)),
            (ENV_BRIDGE_PID, std::process::id().to_string()),
            (ENV_DEADLINE, "60".into()),
        ]
    }

    fn next(&self) -> Option<Seen> {
        self.seen
            .recv_timeout(Duration::from_secs(5))
            .expect("假桥接 5 秒内有连接")
    }

    fn nothing_arrived(&self) -> bool {
        self.seen.recv_timeout(Duration::from_millis(300)).is_err()
    }
}

struct Ran {
    code: Option<i32>,
    stdout: String,
    stderr: String,
    pid: u32,
    took: Duration,
}

impl Ran {
    fn decision(&self) -> Value {
        let line = self.stdout.trim_end();
        assert!(!line.contains('\n'), "只输出一行：{line:?}");
        serde_json::from_str(line).unwrap_or_else(|e| panic!("输出不是 JSON（{e}）：{line:?}"))
    }

    /// 合法的拒绝：退出码 0、一行 JSON、原因里有 `reason`。
    fn assert_denied(&self, reason: &str) {
        assert_eq!(self.code, Some(0), "stderr: {}", self.stderr);
        let d = &self.decision()["hookSpecificOutput"];
        assert_eq!(d["hookEventName"], "PreToolUse");
        assert_eq!(d["permissionDecision"], "deny", "{d}");
        let why = d["permissionDecisionReason"].as_str().unwrap();
        assert!(why.contains(reason), "{why:?} 里没有 {reason:?}");
        assert!(d.get("updatedInput").is_none());
    }
}

fn run_with(args: &[&str], env: &[(&'static str, String)], stdin: &[u8], close_stdin: bool) -> Ran {
    let start = Instant::now();
    let mut child = Command::new(HOOK)
        .args(args)
        .env_clear()
        .envs(env.iter().map(|(k, v)| (*k, v.as_str())))
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let pid = child.id();
    let mut input = child.stdin.take().unwrap();
    let body = stdin.to_vec();
    let writer = std::thread::spawn(move || {
        // hook 可能不读完就退出（超上限、环境变量缺），写失败不算测试失败
        let _ = input.write_all(&body);
        if close_stdin {
            drop(input);
        } else {
            std::thread::sleep(Duration::from_secs(30));
        }
    });
    let out = child.wait_with_output().unwrap();
    if close_stdin {
        writer.join().unwrap();
    }
    Ran {
        code: out.status.code(),
        stdout: String::from_utf8(out.stdout).unwrap(),
        stderr: String::from_utf8(out.stderr).unwrap(),
        pid,
        took: start.elapsed(),
    }
}

fn run(env: &[(&'static str, String)], stdin: &[u8]) -> Ran {
    run_with(&["pre-tool-use"], env, stdin, true)
}

const INPUT: &[u8] =
    br#"{"session_id":"s","tool_name":"Bash","tool_input":{"command":"ls"},"cwd":"/work"}"#;

/// 允许：原样转交 stdin，请求头带着环境变量里的令牌与 hook 自己的进程号。
#[test]
fn allow_relays_and_request_carries_token_pid_and_stdin() {
    let bridge = Bridge::answering(Response::allow());
    let ran = run(&bridge.env(), INPUT);
    assert_eq!(ran.code, Some(0), "stderr: {}", ran.stderr);
    assert_eq!(
        ran.decision(),
        json!({ "hookSpecificOutput": {
            "hookEventName": "PreToolUse", "permissionDecision": "allow"
        }})
    );
    let seen = bridge.next().expect("收到了请求");
    assert_eq!(seen.header.kind, HookKind::PreToolUse);
    assert!(wire::token_matches(&TOKEN, &seen.header.token));
    assert_eq!(seen.header.pid, ran.pid);
    assert_eq!(
        seen.peer.map(|p| p.pid),
        Some(ran.pid),
        "对端就是 hook 进程"
    );
    assert_eq!(seen.body, INPUT);
}

#[test]
fn allow_with_rewrite_outputs_updated_input() {
    let bridge = Bridge::answering(Response {
        decision: Decision::Allow,
        message: String::new(),
        updated_input: Some(json!({ "command": "wrapped ls" })),
    });
    let ran = run(&bridge.env(), INPUT);
    assert_eq!(ran.code, Some(0));
    let d = &ran.decision()["hookSpecificOutput"];
    assert_eq!(d["permissionDecision"], "allow");
    assert_eq!(d["updatedInput"], json!({ "command": "wrapped ls" }));
}

/// 第 11 行：后台答拒绝，原因原样回给模型；没给原因时用默认的话。
#[test]
fn bridge_deny_is_relayed() {
    let bridge = Bridge::answering(Response::deny("用户拒绝了：rm -rf build"));
    run(&bridge.env(), INPUT).assert_denied("用户拒绝了：rm -rf build");
    let bridge = Bridge::answering(Response::deny(""));
    run(&bridge.env(), INPUT).assert_denied(DENIED_WITHOUT_REASON);
}

/// 第 4 行（hook 一侧）：桥接核对令牌不符时答拒绝，hook 照转。令牌不符的判定在桥接，随 S6 测。
#[test]
fn token_rejected_by_bridge_is_relayed_as_deny() {
    let bridge = Bridge::start(|h| {
        if wire::token_matches(&[0u8; wire::TOKEN_BYTES], &h.token) {
            Reply::Respond(Response::allow())
        } else {
            Reply::Respond(Response::deny("审批通道校验失败"))
        }
    });
    run(&bridge.env(), INPUT).assert_denied("审批通道校验失败");
}

/// 第 1 行：四个环境变量缺任何一个都拒绝，而且不去连接。
#[test]
fn missing_env_denies() {
    let bridge = Bridge::answering(Response::allow());
    for name in [ENV_SOCK, ENV_TOKEN, ENV_BRIDGE_PID, ENV_DEADLINE] {
        let env: Vec<_> = bridge
            .env()
            .into_iter()
            .filter(|(k, _)| *k != name)
            .collect();
        run(&env, INPUT).assert_denied("审批通道未配置");
    }
    assert!(bridge.nothing_arrived());
}

/// 第 2 行：socket 不存在、存在但没人监听（连接被拒），都拒绝。
#[test]
fn no_socket_denies() {
    let bridge = Bridge::answering(Response::allow());
    let mut env = bridge.env();
    env[0].1 = bridge.sock.with_file_name("missing").display().to_string();
    run(&env, INPUT).assert_denied("连不上 iWork");

    let dir = tempfile::tempdir().unwrap();
    let stale = dir.path().join("s");
    drop(UnixListener::bind(&stale).unwrap());
    env[0].1 = stale.display().to_string();
    run(&env, INPUT).assert_denied("连不上 iWork");
}

/// 第 3 行：对端进程号不是给定的桥接进程号 → 拒绝，而且一个字节都没发（令牌不交给冒充者）。
#[test]
fn wrong_peer_pid_denies_before_sending_token() {
    let bridge = Bridge::answering(Response::allow());
    let mut env = bridge.env();
    env[2].1 = "1".into();
    run(&env, INPUT).assert_denied("对端校验失败");
    assert!(
        bridge.next().is_none(),
        "连接上了，但没有收到请求头（令牌没发出去）"
    );
}

/// 第 7 行：桥接读完请求就断了（崩溃、被杀）→ 拒绝。
#[test]
fn bridge_dies_mid_wait_denies() {
    let bridge = Bridge::start(|_| Reply::Close);
    run(&bridge.env(), INPUT).assert_denied("审批通道中断");
}

fn raw(magic: &[u8], len: u32, body: &[u8]) -> Vec<u8> {
    let mut v = magic.to_vec();
    v.extend_from_slice(&len.to_le_bytes());
    v.extend_from_slice(body);
    v
}

/// 第 8、9 行：魔数错、长度超、JSON 坏、`decision` 取值怪、改写不是对象，都拒绝。
#[test]
fn garbage_response_denies() {
    let allow = br#"{"decision":"allow"}"#;
    let weird = br#"{"decision":"allow_always"}"#;
    let not_obj = br#"{"decision":"allow","updated_input":"rm -rf /"}"#;
    for (name, bytes) in [
        ("魔数", raw(b"XXXX", allow.len() as u32, allow)),
        ("长度", raw(&wire::MAGIC, wire::MAX_BODY_BYTES + 1, allow)),
        ("JSON", raw(&wire::MAGIC, 6, b"{nope}")),
        ("取值", raw(&wire::MAGIC, weird.len() as u32, weird)),
        ("改写", raw(&wire::MAGIC, not_obj.len() as u32, not_obj)),
    ] {
        let bridge = Bridge::start(move |_| Reply::Raw(bytes.clone()));
        let ran = run(&bridge.env(), INPUT);
        assert!(ran.stdout.contains("deny"), "{name}: {}", ran.stdout);
        ran.assert_denied("无法识别");
    }
}

/// 第 10 行：工具输入超过 4 MiB → 拒绝，不去连接。
#[test]
fn input_too_large_denies_without_connecting() {
    let bridge = Bridge::answering(Response::allow());
    let big = vec![b'x'; wire::MAX_BODY_BYTES as usize + 1];
    run(&bridge.env(), &big).assert_denied("超过 4 MiB");
    assert!(bridge.nothing_arrived());
    // 对照：恰好 4 MiB 的照常转交
    let edge = vec![b'y'; wire::MAX_BODY_BYTES as usize];
    let ran = run(&bridge.env(), &edge);
    assert_eq!(
        ran.decision()["hookSpecificOutput"]["permissionDecision"],
        "allow"
    );
    assert_eq!(bridge.next().unwrap().body.len(), edge.len());
}

/// 第 14 行：桥接一直不答，hook 到自己的时限就拒绝，不等 Claude Code 的超时。
#[test]
fn hook_deadline_denies_when_bridge_hangs() {
    let bridge = Bridge::start(|_| Reply::Hang);
    let mut env = bridge.env();
    env[3].1 = "1".into();
    let ran = run(&env, INPUT);
    ran.assert_denied("时限");
    assert!(ran.took < Duration::from_secs(5), "{:?}", ran.took);
}

/// 时限从进程启动算，管到每一步：stdin 一直不关也一样到时拒绝。
#[test]
fn hook_deadline_covers_stdin_that_never_closes() {
    let bridge = Bridge::answering(Response::allow());
    let mut env = bridge.env();
    env[3].1 = "1".into();
    let ran = run_with(&["pre-tool-use"], &env, INPUT, false);
    ran.assert_denied("时限");
    assert!(ran.took < Duration::from_secs(5), "{:?}", ran.took);
    assert!(bridge.nothing_arrived());
}

/// 第 16 行：stdout 写不出去 → 退出码 2（Claude Code 按拦下处理），原因写 stderr。
#[test]
fn stdout_write_failure_exits_2() {
    let bridge = Bridge::start(|_| {
        std::thread::sleep(Duration::from_millis(300));
        Reply::Respond(Response::allow())
    });
    let mut child = Command::new(HOOK)
        .arg("pre-tool-use")
        .env_clear()
        .envs(bridge.env().iter().map(|(k, v)| (*k, v.as_str())))
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    child.stdin.take().unwrap().write_all(INPUT).unwrap();
    drop(child.stdout.take()); // 读端关掉：hook 写结论时得到 EPIPE
    let mut stderr = String::new();
    child
        .stderr
        .take()
        .unwrap()
        .read_to_string(&mut stderr)
        .unwrap();
    let status = child.wait().unwrap();
    assert_eq!(status.code(), Some(2), "stderr: {stderr}");
    assert!(stderr.contains("出错"), "{stderr}");
}

/// 报到：请求种类是 `session_start`，不输出决定，退出码 0；报不了到也是 0。
#[test]
fn session_start_reports_quietly() {
    let bridge = Bridge::answering(Response::allow());
    let ran = run_with(&["session-start"], &bridge.env(), b"{}", true);
    assert_eq!(ran.code, Some(0));
    assert_eq!(ran.stdout, "");
    assert_eq!(bridge.next().unwrap().header.kind, HookKind::SessionStart);

    let ran = run_with(&["session-start"], &[], b"{}", true);
    assert_eq!((ran.code, ran.stdout.as_str()), (Some(0), ""));
}

/// 子命令不认识、多给了参数、没给子命令：一律按 PreToolUse 拒绝。
#[test]
fn bad_arguments_deny() {
    let bridge = Bridge::answering(Response::allow());
    for args in [&["post-tool-use"][..], &["pre-tool-use", "extra"], &[]] {
        run_with(args, &bridge.env(), INPUT, true).assert_denied("参数不对");
    }
    assert!(bridge.nothing_arrived());
}

/// 连接上的另一种冒充：socket 路径上监听的是别的进程（这里用一个子进程扮演），进程号对不上。
#[test]
fn impostor_listener_in_other_process_is_refused() {
    let dir = tempfile::tempdir().unwrap();
    let sock = dir.path().join("s");
    // python3 在 CI 三个平台上都有；这里只跑 unix
    let script = format!(
        "import socket,time\ns=socket.socket(socket.AF_UNIX)\ns.bind({:?})\ns.listen(1)\nc,_=s.accept()\ntime.sleep(5)\n",
        sock.display().to_string()
    );
    let mut impostor = Command::new("python3")
        .args(["-c", &script])
        .spawn()
        .expect("起 python3");
    for _ in 0..100 {
        if sock.exists() {
            break;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    // 环境变量里的桥接进程号是测试进程，监听方是 python3
    let env = vec![
        (ENV_SOCK, sock.display().to_string()),
        (ENV_TOKEN, wire::token_hex(&TOKEN)),
        (ENV_BRIDGE_PID, std::process::id().to_string()),
        (ENV_DEADLINE, "60".into()),
    ];
    run(&env, INPUT).assert_denied("对端校验失败");
    let _ = impostor.kill();
    let _ = impostor.wait();
}

/// 1/F-1：结论很大、读端不读 stdout，主线程卡在写结论上；看门狗到时不等它，按时以退出码 2 结束，
/// 不拖到 Claude Code 自己的 hook 超时（超时即放行）。
#[test]
fn deadline_fires_even_when_stdout_write_blocks() {
    let big = "x".repeat(1024 * 1024);
    let bridge = Bridge::answering(Response {
        decision: Decision::Allow,
        message: String::new(),
        updated_input: Some(json!({ "command": big })),
    });
    let mut env = bridge.env();
    env[3].1 = "2".into();
    let start = Instant::now();
    let mut child = Command::new(HOOK)
        .arg("pre-tool-use")
        .env_clear()
        .envs(env.iter().map(|(k, v)| (*k, v.as_str())))
        .stdin(Stdio::piped())
        .stdout(Stdio::piped()) // 拿着读端，从不读
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    child.stdin.take().unwrap().write_all(INPUT).unwrap();
    let _held = child.stdout.take();
    let status = loop {
        if let Some(status) = child.try_wait().unwrap() {
            break status;
        }
        if start.elapsed() > Duration::from_secs(10) {
            let _ = child.kill();
            panic!("hook 10 秒还没退出：看门狗被卡住了");
        }
        std::thread::sleep(Duration::from_millis(50));
    };
    let took = start.elapsed();
    let mut stderr = String::new();
    child
        .stderr
        .take()
        .unwrap()
        .read_to_string(&mut stderr)
        .unwrap();
    assert_eq!(status.code(), Some(2), "stderr: {stderr}");
    assert!(stderr.contains("时限"), "{stderr}");
    assert!(
        took >= Duration::from_secs(2) && took < Duration::from_secs(5),
        "{took:?}"
    );
    assert!(
        bridge.next().is_some(),
        "对照：请求确实到了桥接、桥接答了允许"
    );
}
