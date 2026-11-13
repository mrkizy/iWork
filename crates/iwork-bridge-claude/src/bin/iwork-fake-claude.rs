//! 测试用的假 Claude Code CLI（T06 方案 15.1）。只在 `fake-cli` feature 下编译，不进发布包。
//!
//! 照脚本一步步做，把看到的东西记进记录目录，供测试断言：
//!
//! - 脚本：环境变量 `IWORK_FAKE_CLAUDE_SCRIPT`，一个 JSON 数组，每项一个 [`Step`]；
//! - 记录目录：环境变量 `IWORK_FAKE_CLAUDE_RECORD`。启动时写 `args.json`（收到的参数）、`env.json`（收到的全部
//!   环境变量）；之后每件事往 `log.jsonl` 追加一行 `{"at_ms":…,"kind":…,…}`（`at_ms` 是墙钟毫秒，测试拿它和
//!   别的进程比先后）。
//!
//! 和真 CLI 一样走设置文件：SessionStart hook 的命令从 `--settings` 指的文件里读出来，用 `sh -c` 跑，环境就是
//! 桥接给 CLI 的那一份（四个 `IWORK_HOOK_*` 从这里传给 hook）。脚本跑完以退出码 0 结束。
//!
//! 它测的是桥接与 hook 的行为，测不了 3.4 的硬前提（那是真 CLI 的性质）。

use std::io::{BufRead, Write};
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde_json::{Value, json};

/// 脚本的一步。
#[derive(Debug, serde::Deserialize)]
#[serde(tag = "step", rename_all = "snake_case", deny_unknown_fields)]
enum Step {
    /// 照设置文件跑 SessionStart hook（报到）
    SessionStart,
    /// 从 stdin 读一行并记下；stdin 已经关了就记 `stdin_eof` 并退出
    ReadLine,
    /// 回最近读到的 `initialize` 控制请求。`mode` 为 `null` 时响应里不带这个键；`extra` 合进响应；
    /// `subtype` 默认 `success`
    AnswerInitialize {
        mode: Option<String>,
        #[serde(default = "success")]
        subtype: String,
        #[serde(default)]
        extra: serde_json::Map<String, Value>,
    },
    /// 往 stdout 写一行。字符串值恰好是 `$autoMemoryDirectory` 的，换成设置文件里的那个值（改道目录在桥接随机建的
    /// 会话目录里，脚本事先不知道）
    Emit {
        line: Value,
    },
    /// 往 stdout 原样写一行文字（造坏行）
    EmitRaw {
        text: String,
    },
    /// 往 stdout 写一行合法 JSON，总长约 `bytes` 字节（造超长行；脚本放在环境变量里，放不下这么大的值）
    EmitBig {
        bytes: usize,
    },
    /// 照设置文件跑 PreToolUse hook 问一次（和真 CLI 一样：stdin 是 hook 输入），记下决定。允许时用改写后的输入
    /// （有的话）「执行」：输入里有 `marker`（路径字符串）就建这个文件
    Tool {
        id: String,
        name: String,
        input: Value,
        /// 给了的话：等这个文件出现（测试在后台收到权限请求时建它，说明 hook 已经连上、正在等答复）再把 hook
        /// 连同它所在的进程组一起杀掉（hook 在等答复时死了，桥接那边看到的是 hook 断开）。杀整组而不是只杀
        /// `sh`：`sh -c` 不一定 exec 成 hook（Linux 的 dash 上 hook 是 sh 的子进程，只杀 sh 它还活着）
        #[serde(default)]
        kill_hook_when: Option<PathBuf>,
    },
    /// 不理 stdin，等 SIGTERM：收到就记一笔、以 0 退出（测试据此确认桥接发的是 SIGTERM、不是 SIGKILL）
    WaitForSigterm,
    SleepMs {
        ms: u64,
    },
    Exit {
        code: i32,
    },
    /// 一直读 stdin 到关掉（之后接着下一步）
    WaitStdinEof,
    /// 另立会话跑一条 `sh -c` 命令，不等它（代表 CLI 起的 Bash 命令，k4：命令的 shell 自成会话）。记下它的进程号。
    /// `same_group` 为真时不另立会话、留在 CLI 的进程组里（代表 CLI 直接起、没脱组的子进程）
    Spawn {
        command: String,
        #[serde(default)]
        same_group: bool,
    },
    /// 不理 stdin，记下收到的 SIGHUP、SIGINT、SIGCONT（每收到一个记一笔 `signal`），收到 SIGTERM 记一笔后以 0
    /// 退出。`ignore_hup` 为真时 SIGHUP 照样记下、不退出；为假时记下后按默认动作退出（以 SIGHUP 结束自己）
    LogSignalsUntilTerm {
        #[serde(default)]
        ignore_hup: bool,
    },
}

/// 收到过的信号（处理函数里只置位，主循环里记日志）。
#[cfg(unix)]
static SIGNALS_SEEN: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);

#[cfg(unix)]
extern "C" fn note_signal(sig: libc::c_int) {
    SIGNALS_SEEN.fetch_or(1 << sig, std::sync::atomic::Ordering::SeqCst);
}

#[cfg(unix)]
fn log_signals_until_term(fake: &Fake, ignore_hup: bool) -> ! {
    use std::sync::atomic::Ordering;
    let watched = [
        (libc::SIGHUP, "SIGHUP"),
        (libc::SIGINT, "SIGINT"),
        (libc::SIGCONT, "SIGCONT"),
        (libc::SIGTERM, "SIGTERM"),
    ];
    for (sig, _) in watched {
        // SAFETY: 处理函数只做一次原子写
        unsafe { libc::signal(sig, note_signal as *const () as libc::sighandler_t) };
    }
    fake.log("watching_signals", json!({}));
    loop {
        std::thread::sleep(Duration::from_millis(5));
        let seen = SIGNALS_SEEN.swap(0, Ordering::SeqCst);
        for (sig, name) in watched {
            if seen & (1 << sig) != 0 {
                fake.log("signal", json!({ "signal": name }));
            }
        }
        if seen & (1 << libc::SIGTERM) != 0 {
            std::process::exit(0);
        }
        if seen & (1 << libc::SIGHUP) != 0 && !ignore_hup {
            // SAFETY: 恢复默认动作后给自己发 SIGHUP：照默认动作结束
            unsafe {
                libc::signal(libc::SIGHUP, libc::SIG_DFL);
                libc::raise(libc::SIGHUP);
            }
        }
    }
}

#[cfg(not(unix))]
fn log_signals_until_term(_fake: &Fake, _ignore_hup: bool) -> ! {
    unimplemented!("log_signals_until_term 只在 unix 上")
}

/// 另立会话起一条命令。
#[cfg(unix)]
fn spawn_detached(command: &str) -> u32 {
    use std::os::unix::process::CommandExt;
    let mut cmd = Command::new("sh");
    cmd.args(["-c", command])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    // SAFETY: fork 之后只调 setsid（异步信号安全）
    unsafe {
        cmd.pre_exec(|| {
            libc::setsid();
            Ok(())
        });
    }
    cmd.spawn().expect("起得了命令").id()
}

/// 在 CLI 自己的进程组里起一条命令，不等它。
fn spawn_in_group(command: &str) -> u32 {
    Command::new("sh")
        .args(["-c", command])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("起得了命令")
        .id()
}

#[cfg(not(unix))]
fn spawn_detached(_command: &str) -> u32 {
    unimplemented!("spawn 只在 unix 上")
}

/// 把 SIGTERM 屏蔽掉，再用 sigwait 同步地等它（不装处理函数），返回收到的信号号。
#[cfg(unix)]
fn wait_for_sigterm() -> i32 {
    // SAFETY: 假 CLI 是单线程的；set 与 got 都是本地变量，调用顺序照 sigwait(3) 的要求
    unsafe {
        let mut set: libc::sigset_t = std::mem::zeroed();
        libc::sigemptyset(&mut set);
        libc::sigaddset(&mut set, libc::SIGTERM);
        libc::pthread_sigmask(libc::SIG_BLOCK, &set, std::ptr::null_mut());
        let mut got: libc::c_int = 0;
        libc::sigwait(&set, &mut got);
        got
    }
}

/// M1 不支持 Windows（AGENTS.md）：用到这一步的测试只在 unix 上跑。
#[cfg(not(unix))]
fn wait_for_sigterm() -> i32 {
    unimplemented!("wait_for_sigterm 只在 unix 上")
}

/// 杀掉 hook 所在的整个进程组（组号就是 `sh` 的进程号，见 [`Fake::tool`]）。
#[cfg(unix)]
fn kill_group(child: &std::process::Child) {
    let pgid = libc::pid_t::try_from(child.id()).expect("进程号放得进 pid_t");
    // SAFETY: 只发信号，参数是我们自己起的进程组
    let rc = unsafe { libc::kill(-pgid, libc::SIGKILL) };
    assert_eq!(
        rc,
        0,
        "杀得掉 hook 的进程组：{}",
        std::io::Error::last_os_error()
    );
}

#[cfg(not(unix))]
fn kill_group(_child: &std::process::Child) {
    unimplemented!("kill_hook_when 只在 unix 上")
}

fn success() -> String {
    "success".into()
}

struct Fake {
    record: PathBuf,
    settings: Option<PathBuf>,
    last_read: Option<Value>,
    stdin: std::io::StdinLock<'static>,
}

fn now_ms() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or(Duration::ZERO)
        .as_millis()
}

impl Fake {
    fn log(&self, kind: &str, data: Value) {
        let mut entry = json!({ "at_ms": now_ms() as u64, "kind": kind });
        if let (Value::Object(e), Value::Object(d)) = (&mut entry, data) {
            e.extend(d);
        }
        let mut f = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(self.record.join("log.jsonl"))
            .expect("记录目录写得进去");
        // 整行一次写出：测试在别的进程里随时读这个文件，分几次写会读到半行
        f.write_all(format!("{entry}\n").as_bytes())
            .expect("记录写得进去");
    }

    fn out(&self, line: &Value) {
        let mut stdout = std::io::stdout().lock();
        writeln!(stdout, "{line}").expect("stdout 写得出去");
        stdout.flush().expect("stdout 刷得出去");
    }

    fn read_line(&mut self) -> Option<String> {
        let mut line = String::new();
        match self.stdin.read_line(&mut line) {
            Ok(0) | Err(_) => None,
            Ok(_) => Some(line.trim_end_matches('\n').to_owned()),
        }
    }

    fn run(&mut self, step: Step) {
        match step {
            Step::SessionStart => {
                let command = self.session_start_command();
                let mut child = Command::new("sh")
                    .args(["-c", &command])
                    .stdin(Stdio::piped())
                    .stdout(Stdio::null())
                    .stderr(Stdio::null())
                    .spawn()
                    .expect("起得了 hook");
                child
                    .stdin
                    .take()
                    .expect("stdin 是 piped")
                    .write_all(br#"{"hook_event_name":"SessionStart","source":"startup"}"#)
                    .expect("hook 的 stdin 写得进去");
                let status = child.wait().expect("等得到 hook");
                self.log("session_start", json!({ "exit_code": status.code() }));
            }
            Step::ReadLine => match self.read_line() {
                Some(line) => {
                    self.last_read = serde_json::from_str(&line).ok();
                    self.log("stdin", json!({ "line": line }));
                }
                None => {
                    self.log("stdin_eof", json!({}));
                    std::process::exit(0);
                }
            },
            Step::AnswerInitialize {
                mode,
                subtype,
                extra,
            } => {
                let request_id = self
                    .last_read
                    .as_ref()
                    .filter(|v| v["request"]["subtype"] == "initialize")
                    .and_then(|v| v["request_id"].as_str())
                    .expect("上一行读到的是 initialize 控制请求")
                    .to_owned();
                let mut response = extra;
                if let Some(mode) = mode {
                    response.insert("current_permission_mode".into(), Value::String(mode));
                }
                self.out(&json!({
                    "type": "control_response",
                    "response": { "subtype": subtype, "request_id": request_id, "response": response },
                }));
                self.log("answered_initialize", json!({}));
            }
            Step::Emit { mut line } => {
                self.fill_placeholders(&mut line);
                self.out(&line);
                self.log("emitted", json!({}));
            }
            Step::EmitRaw { text } => {
                let mut stdout = std::io::stdout().lock();
                writeln!(stdout, "{text}").expect("stdout 写得出去");
                stdout.flush().expect("stdout 刷得出去");
                self.log("emitted", json!({}));
            }
            Step::EmitBig { bytes } => {
                self.out(&json!({ "type": "system", "subtype": "big", "pad": "x".repeat(bytes) }));
                self.log("emitted", json!({ "bytes": bytes }));
            }
            Step::Tool {
                id,
                name,
                input,
                kill_hook_when,
            } => self.tool(&id, &name, input, kill_hook_when.as_deref()),
            Step::WaitForSigterm => {
                let sig = wait_for_sigterm();
                self.log("got_signal", json!({ "signal": sig }));
                std::process::exit(0);
            }
            Step::SleepMs { ms } => std::thread::sleep(Duration::from_millis(ms)),
            Step::Spawn {
                command,
                same_group,
            } => {
                let pid = if same_group {
                    spawn_in_group(&command)
                } else {
                    spawn_detached(&command)
                };
                self.log("spawned", json!({ "pid": pid }));
            }
            Step::LogSignalsUntilTerm { ignore_hup } => log_signals_until_term(self, ignore_hup),
            Step::Exit { code } => {
                self.log("exit", json!({ "code": code }));
                std::process::exit(code);
            }
            Step::WaitStdinEof => {
                while let Some(line) = self.read_line() {
                    self.log("stdin", json!({ "line": line }));
                }
                self.log("stdin_eof", json!({}));
            }
        }
    }

    /// 设置文件里 SessionStart 的第一个 hook 命令。
    fn session_start_command(&self) -> String {
        self.hook_command("SessionStart")
    }

    fn fill_placeholders(&self, v: &mut Value) {
        match v {
            Value::String(s) if s == "$autoMemoryDirectory" => {
                let path = self.settings.as_ref().expect("参数里有 --settings");
                let settings: Value =
                    serde_json::from_slice(&std::fs::read(path).expect("设置文件读得到"))
                        .expect("是 JSON");
                *v = settings["autoMemoryDirectory"].clone();
            }
            Value::Array(a) => a.iter_mut().for_each(|x| self.fill_placeholders(x)),
            Value::Object(o) => o.values_mut().for_each(|x| self.fill_placeholders(x)),
            _ => {}
        }
    }

    /// 设置文件里某个事件的第一个 hook 命令。
    fn hook_command(&self, event: &str) -> String {
        let path = self.settings.as_ref().expect("参数里有 --settings");
        let settings: Value =
            serde_json::from_slice(&std::fs::read(path).expect("设置文件读得到")).expect("是 JSON");
        settings["hooks"][event][0]["hooks"][0]["command"]
            .as_str()
            .unwrap_or_else(|| panic!("设置里有 {event} hook"))
            .to_owned()
    }

    /// 一次工具调用：问 hook，按决定「执行」。
    fn tool(&self, id: &str, name: &str, input: Value, kill_hook_when: Option<&std::path::Path>) {
        let stdin = json!({
            "session_id": "fake",
            "hook_event_name": "PreToolUse",
            "tool_name": name,
            "tool_input": input,
            "tool_use_id": id,
            "cwd": std::env::current_dir().map(|p| p.display().to_string()).unwrap_or_default(),
        });
        let mut command = Command::new("sh");
        command.args(["-c", &self.hook_command("PreToolUse")]);
        // hook 自成一个进程组，杀的时候连 sh 带 hook 一起杀
        #[cfg(unix)]
        std::os::unix::process::CommandExt::process_group(&mut command, 0);
        let mut child = command
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .expect("起得了 hook");
        child
            .stdin
            .take()
            .expect("stdin 是 piped")
            .write_all(stdin.to_string().as_bytes())
            .expect("hook 的 stdin 写得进去");
        if let Some(signal) = kill_hook_when {
            let deadline = std::time::Instant::now() + Duration::from_secs(30);
            while !signal.exists() {
                assert!(
                    std::time::Instant::now() < deadline,
                    "30 秒内等到 {signal:?}"
                );
                std::thread::sleep(Duration::from_millis(10));
            }
            kill_group(&child);
        }
        let out = child.wait_with_output().expect("等得到 hook");
        let decided: Value = serde_json::from_slice(&out.stdout).unwrap_or(Value::Null);
        let d = &decided["hookSpecificOutput"];
        let allow = d["permissionDecision"] == "allow";
        let effective = d.get("updatedInput").cloned().unwrap_or(input);
        if allow && let Some(marker) = effective.get("marker").and_then(Value::as_str) {
            std::fs::write(marker, b"ran").expect("建得了标记文件");
        }
        self.log(
            "tool",
            json!({
                "id": id,
                "exit_code": out.status.code(),
                "decision": d["permissionDecision"],
                "reason": d["permissionDecisionReason"],
                "effective_input": effective,
            }),
        );
    }
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let record = PathBuf::from(
        std::env::var_os("IWORK_FAKE_CLAUDE_RECORD").expect("给了 IWORK_FAKE_CLAUDE_RECORD"),
    );
    let script: Vec<Step> = serde_json::from_str(
        &std::env::var("IWORK_FAKE_CLAUDE_SCRIPT").expect("给了 IWORK_FAKE_CLAUDE_SCRIPT"),
    )
    .expect("脚本是合法的步骤数组");
    std::fs::write(record.join("args.json"), json!(args).to_string()).expect("记得下参数");
    let env: serde_json::Map<String, Value> = std::env::vars_os()
        .map(|(k, v)| {
            (
                k.to_string_lossy().into_owned(),
                Value::String(v.to_string_lossy().into_owned()),
            )
        })
        .collect();
    std::fs::write(record.join("env.json"), Value::Object(env).to_string()).expect("记得下环境");
    let settings = args
        .iter()
        .position(|a| a == "--settings")
        .and_then(|i| args.get(i + 1))
        .map(PathBuf::from);
    // 设置文件随会话目录一起删（收摊时就删，5.4a）：测试要看它、之后再起 hook 都从这份副本读
    let settings = settings.map(|path| {
        let copy = record.join("settings.json");
        std::fs::copy(&path, &copy).map(|_| copy).unwrap_or(path)
    });
    let mut fake = Fake {
        record,
        settings,
        last_read: None,
        stdin: std::io::stdin().lock(),
    };
    fake.log("started", json!({ "pid": std::process::id() }));
    for step in script {
        fake.run(step);
    }
    fake.log("script_done", json!({}));
}
