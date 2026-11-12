//! hook 的退出码规定（T06 方案 3.4a 第 3 条；19 节 S3a）：只有把完整的决定写出并刷出成功之后才以 0 退出，
//! 别的路径一律非零。不然 hook 自己就造出了「退出码 0 却没有合法决定」（3.4 的情形 2、3），CLI 会照它自己的
//! 规则放行 B 类工具；非零退出则由 `onFailure: "block"` 拦下（3.4a 第 1 条）。
//!
//! 两样检查：
//! - **故障注入**：逐个注入点起真的 hook 进程，断言「退出码 0 ⇒ stdout 恰好是一条合法的决定 JSON」，
//!   以及每个注入点自己该有的退出码（不合法的决定 ⇒ 非 0）。外部条件（关 stdout、管道、环境变量、假桥接）
//!   直接对真进程做；要在进程内触发的（panic、已认领之后的 panic）照 `panic_denies.rs` 的办法：本程序
//!   （`harness = false`）带上 [`PROBE_ENV`] 重新起自己当探针，调产品同一套 `verdict` 函数。产品二进制里没有开关。
//! - **源码检查**：`src/` 里字面量 `exit(0)` 只出现在 `verdict.rs` 登记的三处，`main` 的签名是 `-> !`。
//!
//! 只对 PreToolUse 讲这条规定：SessionStart 报到不输出决定，按约定以 0 退出（方案 3.5，报不了到桥接自己会停）。

use std::collections::BTreeMap;
use std::path::Path;
use std::process::ExitCode;

/// 设了它，本程序就是探针：值是探针的名字。
const PROBE_ENV: &str = "IWORK_HOOK_EXIT_PROBE";

/// `src/` 里允许出现 `exit(0)` 的地方：所在函数 → 次数（`verdict.rs`）。
const REGISTERED_EXIT_ZERO: [(&str, usize); 3] = [
    // pre-tool-use：结论写出并刷出成功之后
    ("write_and_exit", 1),
    // session-start：主线程报到完、看门狗到时，都不输出决定
    ("finish", 1),
    ("expire", 1),
];

type Case = (&'static str, fn() -> Result<(), String>);

fn main() -> ExitCode {
    if let Ok(name) = std::env::var(PROBE_ENV) {
        #[cfg(unix)]
        unix::probe(&name);
        #[cfg(not(unix))]
        {
            let _ = name;
            std::process::exit(64);
        }
    }
    let mut cases: Vec<Case> = vec![
        ("exit_zero_appears_only_where_registered", source_exit_zero),
        ("main_never_returns", source_main_signature),
    ];
    #[cfg(unix)]
    cases.extend(unix::CASES);
    let mut failed = 0;
    for (name, case) in &cases {
        match case() {
            Ok(()) => println!("test {name} ... ok"),
            Err(e) => {
                failed += 1;
                println!("test {name} ... FAILED：{e}");
            }
        }
    }
    println!("exit_code：{} 通过，{failed} 失败", cases.len() - failed);
    if failed == 0 {
        ExitCode::SUCCESS
    } else {
        ExitCode::FAILURE
    }
}

fn src_dir() -> &'static Path {
    Path::new(concat!(env!("CARGO_MANIFEST_DIR"), "/src"))
}

/// `exit(0)` 出现的位置：文件 → 所在函数 → 次数。所在函数取它前面最近的一个 `fn <名字>`。
fn exit_zero_sites() -> Result<BTreeMap<String, BTreeMap<String, usize>>, String> {
    let mut sites: BTreeMap<String, BTreeMap<String, usize>> = BTreeMap::new();
    let entries = std::fs::read_dir(src_dir()).map_err(|e| e.to_string())?;
    for entry in entries {
        let path = entry.map_err(|e| e.to_string())?.path();
        if path.extension().and_then(|e| e.to_str()) != Some("rs") {
            continue;
        }
        let file = path.file_name().unwrap().to_string_lossy().into_owned();
        let text = std::fs::read_to_string(&path).map_err(|e| e.to_string())?;
        let mut current_fn = String::from("<文件顶层>");
        for line in text.lines() {
            if let Some(at) = line.find("fn ") {
                let name: String = line[at + 3..]
                    .chars()
                    .take_while(|c| c.is_alphanumeric() || *c == '_')
                    .collect();
                if !name.is_empty() {
                    current_fn = name;
                }
            }
            let hits = line.matches("exit(0)").count();
            if hits > 0 {
                *sites
                    .entry(file.clone())
                    .or_default()
                    .entry(current_fn.clone())
                    .or_default() += hits;
            }
        }
    }
    Ok(sites)
}

fn source_exit_zero() -> Result<(), String> {
    let sites = exit_zero_sites()?;
    let expected: BTreeMap<String, BTreeMap<String, usize>> = BTreeMap::from([(
        "verdict.rs".to_owned(),
        REGISTERED_EXIT_ZERO
            .iter()
            .map(|(f, n)| ((*f).to_owned(), *n))
            .collect(),
    )]);
    if sites == expected {
        Ok(())
    } else {
        Err(format!("登记的是 {expected:?}，源码里是 {sites:?}"))
    }
}

fn source_main_signature() -> Result<(), String> {
    let main = std::fs::read_to_string(src_dir().join("main.rs")).map_err(|e| e.to_string())?;
    if main.contains("fn main() -> ! {") {
        Ok(())
    } else {
        Err("src/main.rs 的 main 不是 `fn main() -> !`".into())
    }
}

#[cfg(unix)]
mod unix {
    use std::io::{Read, Write};
    use std::os::unix::net::UnixListener;
    use std::os::unix::process::CommandExt;
    use std::path::PathBuf;
    use std::process::{Child, Command, Stdio};
    use std::time::{Duration, Instant};

    use iwork_claude_hook::client::{ENV_BRIDGE_PID, ENV_DEADLINE, ENV_SOCK, ENV_TOKEN};
    use iwork_claude_hook::output::Outcome;
    use iwork_claude_hook::verdict;
    use iwork_claude_hook::wire::{self, Decision, HookKind, Response};
    use serde_json::{Value, json};

    use super::{Case, PROBE_ENV};

    const HOOK: &str = env!("CARGO_BIN_EXE_iwork-claude-hook");
    const TOKEN: [u8; wire::TOKEN_BYTES] = [0x3c; wire::TOKEN_BYTES];
    const INPUT: &[u8] = br#"{"session_id":"s","tool_name":"Read","tool_input":{"file_path":"a"}}"#;

    pub const CASES: [Case; 12] = [
        ("control_allow_is_valid_decision", control_allow),
        ("stdout_closed_before_start", stdout_closed_before_start),
        ("stdout_reader_gone", stdout_reader_gone),
        ("stdout_full_and_never_read", stdout_full_and_never_read),
        ("bridge_bad_frame", bridge_bad_frame),
        ("bridge_half_response", bridge_half_response),
        ("input_over_4_mib", input_over_4_mib),
        ("panic_on_main_thread", panic_on_main_thread),
        ("panic_after_claimed", panic_after_claimed),
        ("missing_env", missing_env),
        ("unknown_subcommand", unknown_subcommand),
        ("probe_control_exits_normally", probe_control),
    ];

    /// 一次运行的结果。
    struct Ran {
        code: Option<i32>,
        stdout: Vec<u8>,
    }

    /// stdout 恰好是一条合法的 PreToolUse 决定：一行、能解析、`permissionDecision` 是 allow 或 deny。
    fn decision(stdout: &[u8]) -> Option<String> {
        let text = std::str::from_utf8(stdout).ok()?;
        let line = text.strip_suffix('\n')?;
        if line.contains('\n') {
            return None;
        }
        let v: Value = serde_json::from_str(line).ok()?;
        let d = v.get("hookSpecificOutput")?;
        if d.get("hookEventName")? != "PreToolUse" {
            return None;
        }
        match d.get("permissionDecision")?.as_str()? {
            p @ ("allow" | "deny") => Some(p.to_owned()),
            _ => None,
        }
    }

    /// 规定本身，加上这个注入点该有的退出码与决定。
    fn expect(ran: &Ran, code: i32, want: Option<&str>) -> Result<(), String> {
        let got = decision(&ran.stdout);
        if ran.code == Some(0) && got.is_none() {
            return Err(format!(
                "退出码 0 而 stdout 不是合法决定：{:?}",
                String::from_utf8_lossy(&ran.stdout)
            ));
        }
        if ran.code != Some(code) {
            return Err(format!("退出码 {:?}，期望 {code}", ran.code));
        }
        if let Some(want) = want
            && got.as_deref() != Some(want)
        {
            return Err(format!("决定是 {got:?}，期望 {want}"));
        }
        Ok(())
    }

    /// 假桥接：每条连接读完请求，按 `reply` 写回字节（`None` = 读完就关）。
    struct Bridge {
        _dir: tempfile::TempDir,
        sock: PathBuf,
    }

    impl Bridge {
        fn start(reply: impl Fn() -> Option<Vec<u8>> + Send + 'static) -> Self {
            let dir = tempfile::tempdir().unwrap();
            let sock = dir.path().join("s");
            let listener = UnixListener::bind(&sock).unwrap();
            std::thread::spawn(move || {
                for conn in listener.incoming() {
                    let Ok(mut conn) = conn else { return };
                    if wire::read_request_header(&mut conn).is_err()
                        || wire::read_request_body(&mut conn).is_err()
                    {
                        continue;
                    }
                    if let Some(bytes) = reply() {
                        let _ = conn.write_all(&bytes);
                    }
                }
            });
            Self { _dir: dir, sock }
        }

        fn answering(r: Response) -> Self {
            Self::start(move || {
                let mut buf = Vec::new();
                wire::write_response(&mut buf, &r).unwrap();
                Some(buf)
            })
        }

        fn env(&self, deadline_secs: u32) -> Vec<(&'static str, String)> {
            vec![
                (ENV_SOCK, self.sock.display().to_string()),
                (ENV_TOKEN, wire::token_hex(&TOKEN)),
                (ENV_BRIDGE_PID, std::process::id().to_string()),
                (ENV_DEADLINE, deadline_secs.to_string()),
            ]
        }
    }

    fn command(args: &[&str], env: &[(&'static str, String)]) -> Command {
        let mut cmd = Command::new(HOOK);
        cmd.args(args)
            .env_clear()
            .envs(env.iter().map(|(k, v)| (*k, v.as_str())))
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null());
        cmd
    }

    /// 等进程退出（最多 `limit`），超时就杀掉并报错。
    fn wait(child: &mut Child, limit: Duration) -> Result<Option<i32>, String> {
        let start = Instant::now();
        loop {
            if let Some(status) = child.try_wait().map_err(|e| e.to_string())? {
                return Ok(status.code());
            }
            if start.elapsed() > limit {
                let _ = child.kill();
                let _ = child.wait();
                return Err(format!("{limit:?} 内没退出"));
            }
            std::thread::sleep(Duration::from_millis(20));
        }
    }

    /// 写入 stdin、关掉，读完 stdout，等退出。
    fn run(mut cmd: Command, stdin: &[u8]) -> Result<Ran, String> {
        let mut child = cmd.spawn().map_err(|e| e.to_string())?;
        let mut input = child.stdin.take().unwrap();
        let body = stdin.to_vec();
        // hook 可能不读完就退出，写失败不算
        let writer = std::thread::spawn(move || {
            let _ = input.write_all(&body);
        });
        let mut stdout = Vec::new();
        if let Some(mut out) = child.stdout.take() {
            let _ = out.read_to_end(&mut stdout);
        }
        let code = wait(&mut child, Duration::from_secs(10))?;
        let _ = writer.join();
        Ok(Ran { code, stdout })
    }

    /// 对照：一切正常时以 0 退出、给出允许，证明下面的检查确实在比东西。
    fn control_allow() -> Result<(), String> {
        let bridge = Bridge::answering(Response::allow());
        let ran = run(command(&["pre-tool-use"], &bridge.env(60)), INPUT)?;
        expect(&ran, 0, Some("allow"))
    }

    /// stdout 在启动前就是关着的：结论交不出去 → 非 0。连上桥接的 socket 可能拿到 1 号描述符，
    /// 结论会被写进 socket 而不是交给 CLI，这时以 0 退出就是违反规定。
    fn stdout_closed_before_start() -> Result<(), String> {
        let bridge = Bridge::answering(Response::allow());
        let mut cmd = command(&["pre-tool-use"], &bridge.env(60));
        cmd.stdout(Stdio::null());
        // SAFETY: fork 之后、exec 之前只调 async-signal-safe 的 close
        unsafe {
            cmd.pre_exec(|| {
                libc::close(1);
                Ok(())
            });
        }
        let ran = run(cmd, INPUT)?;
        expect(&ran, 2, None)
    }

    /// stdout 的读端已经关了：写结论得到 EPIPE → 退出码 2。
    fn stdout_reader_gone() -> Result<(), String> {
        let bridge = Bridge::start(|| {
            std::thread::sleep(Duration::from_millis(300));
            let mut buf = Vec::new();
            wire::write_response(&mut buf, &Response::allow()).unwrap();
            Some(buf)
        });
        let mut child = command(&["pre-tool-use"], &bridge.env(60))
            .spawn()
            .map_err(|e| e.to_string())?;
        drop(child.stdout.take());
        child.stdin.take().unwrap().write_all(INPUT).unwrap();
        let code = wait(&mut child, Duration::from_secs(10))?;
        expect(
            &Ran {
                code,
                stdout: Vec::new(),
            },
            2,
            None,
        )
    }

    /// stdout 是写满不读的管道：结论很大、写到一半卡住，看门狗到时以 2 退出，读到的是半截。
    fn stdout_full_and_never_read() -> Result<(), String> {
        let bridge = Bridge::answering(Response {
            decision: Decision::Allow,
            message: String::new(),
            updated_input: Some(json!({ "command": "x".repeat(1024 * 1024) })),
        });
        let mut child = command(&["pre-tool-use"], &bridge.env(1))
            .spawn()
            .map_err(|e| e.to_string())?;
        child.stdin.take().unwrap().write_all(INPUT).unwrap();
        let mut held = child.stdout.take().unwrap();
        let code = wait(&mut child, Duration::from_secs(10))?;
        let mut stdout = Vec::new();
        let _ = held.read_to_end(&mut stdout);
        expect(&Ran { code, stdout }, 2, None)
    }

    /// 桥接回的帧坏了：拒绝，以 0 退出。
    fn bridge_bad_frame() -> Result<(), String> {
        let bridge = Bridge::start(|| Some(b"XXXX\x05\0\0\0hello".to_vec()));
        let ran = run(command(&["pre-tool-use"], &bridge.env(60)), INPUT)?;
        expect(&ran, 0, Some("deny"))
    }

    /// 桥接回到一半断开：拒绝，以 0 退出。
    fn bridge_half_response() -> Result<(), String> {
        let bridge = Bridge::start(|| {
            let mut bytes = wire::MAGIC.to_vec();
            bytes.extend_from_slice(&100u32.to_le_bytes());
            bytes.extend_from_slice(br#"{"decision""#);
            Some(bytes)
        });
        let ran = run(command(&["pre-tool-use"], &bridge.env(60)), INPUT)?;
        expect(&ran, 0, Some("deny"))
    }

    fn input_over_4_mib() -> Result<(), String> {
        let bridge = Bridge::answering(Response::allow());
        let big = vec![b'x'; wire::MAX_BODY_BYTES as usize + 1];
        let ran = run(command(&["pre-tool-use"], &bridge.env(60)), &big)?;
        expect(&ran, 0, Some("deny"))
    }

    fn missing_env() -> Result<(), String> {
        let ran = run(command(&["pre-tool-use"], &[]), INPUT)?;
        expect(&ran, 0, Some("deny"))
    }

    fn unknown_subcommand() -> Result<(), String> {
        let bridge = Bridge::answering(Response::allow());
        let ran = run(command(&["post-tool-use"], &bridge.env(60)), INPUT)?;
        expect(&ran, 0, Some("deny"))
    }

    // ---------- 进程内的注入点：本程序当探针 ----------

    /// 探针：装产品的 panic 钩子，照名字在不同时机 panic。
    pub fn probe(name: &str) -> ! {
        verdict::install_panic_hook();
        match name {
            "main_panic" => panic!("探针在主线程 panic"),
            "claimed_panic" => {
                // 主线程认领后卡在写一个很大的结论上（读端不读）；这时另一个线程 panic
                std::thread::spawn(|| {
                    std::thread::sleep(Duration::from_millis(300));
                    panic!("探针在认领之后 panic")
                });
                let mut big = serde_json::Map::new();
                big.insert("command".into(), json!("x".repeat(1024 * 1024)));
                verdict::finish(
                    HookKind::PreToolUse,
                    &Outcome::Allow {
                        updated_input: Some(big),
                    },
                )
            }
            _ => std::process::exit(64),
        }
    }

    fn run_probe(name: &str, read_stdout: bool) -> Result<Ran, String> {
        let mut child = Command::new(std::env::current_exe().unwrap())
            .env_clear()
            .env(PROBE_ENV, name)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .map_err(|e| e.to_string())?;
        let mut out = child.stdout.take().unwrap();
        let mut stdout = Vec::new();
        if read_stdout {
            let _ = out.read_to_end(&mut stdout);
        }
        let code = wait(&mut child, Duration::from_secs(10))?;
        if !read_stdout {
            let _ = out.read_to_end(&mut stdout);
        }
        Ok(Ran { code, stdout })
    }

    fn panic_on_main_thread() -> Result<(), String> {
        expect(&run_probe("main_panic", true)?, 2, Some("deny"))
    }

    /// 已认领之后 panic：钩子不和认领者抢 stdout，只写 stderr、以 2 退出；stdout 是半截。
    fn panic_after_claimed() -> Result<(), String> {
        let ran = run_probe("claimed_panic", false)?;
        if ran.stdout.is_empty() {
            return Err("对照：认领者应当已经写出了半截结论".into());
        }
        expect(&ran, 2, None)
    }

    /// 对照：探针不 panic 时按约定以 64 退出，证明上面的 2 来自 panic 钩子。
    fn probe_control() -> Result<(), String> {
        expect(&run_probe("none", true)?, 64, None)
    }
}
