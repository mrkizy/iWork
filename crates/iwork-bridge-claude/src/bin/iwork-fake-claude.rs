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
        /// 给了的话：hook 起来这么多毫秒后把它杀掉（hook 在等答复时死了，桥接那边看到的是 hook 断开）
        #[serde(default)]
        kill_hook_after_ms: Option<u64>,
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
                kill_hook_after_ms,
            } => self.tool(&id, &name, input, kill_hook_after_ms),
            Step::WaitForSigterm => {
                let sig = wait_for_sigterm();
                self.log("got_signal", json!({ "signal": sig }));
                std::process::exit(0);
            }
            Step::SleepMs { ms } => std::thread::sleep(Duration::from_millis(ms)),
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
    fn tool(&self, id: &str, name: &str, input: Value, kill_hook_after_ms: Option<u64>) {
        let stdin = json!({
            "session_id": "fake",
            "hook_event_name": "PreToolUse",
            "tool_name": name,
            "tool_input": input,
            "tool_use_id": id,
            "cwd": std::env::current_dir().map(|p| p.display().to_string()).unwrap_or_default(),
        });
        let mut child = Command::new("sh")
            .args(["-c", &self.hook_command("PreToolUse")])
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
        if let Some(ms) = kill_hook_after_ms {
            std::thread::sleep(Duration::from_millis(ms));
            let _ = child.kill();
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
