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
    /// 往 stdout 写一行
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
    SleepMs {
        ms: u64,
    },
    Exit {
        code: i32,
    },
    /// 一直读 stdin 到关掉（之后接着下一步）
    WaitStdinEof,
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
            Step::Emit { line } => {
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
        let path = self.settings.as_ref().expect("参数里有 --settings");
        let settings: Value =
            serde_json::from_slice(&std::fs::read(path).expect("设置文件读得到")).expect("是 JSON");
        settings["hooks"]["SessionStart"][0]["hooks"][0]["command"]
            .as_str()
            .expect("设置里有 SessionStart hook")
            .to_owned()
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
