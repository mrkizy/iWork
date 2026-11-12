//! 真实 WebView 测试的构建（feature `e2e`，隐含 `dev`；发布构建与普通开发构建都没有它）。
//! T09 方案第 7 节「真实 WebView」：R7 的证据要在真窗口里取，DOM 模拟环境给不了。
//!
//! 由环境变量驱动（client/tests/e2e/webview.ts 设置）：
//! - `IWORK_E2E_SCRIPT`：一段 JS 文件，作为主窗口的初始化脚本（每次加载、每次刷新都在页面脚本之前跑）；
//! - `IWORK_E2E_OUT`：结果文件，一行一条 JSON；
//! - `IWORK_E2E_CSP`：可选，换掉发布层 CSP（强制回退用例）；
//! - `IWORK_E2E_OTHER`：设了就再开一个同来源的副窗口 `other`，同一段脚本在里面跑（命令只收主窗口）。
//!
//! 页面把结果写进 `document.title`（前缀 `E2E:`），壳经 `on_document_title_changed` 收下——不新增命令、
//! 不改 capability、不放宽 CSP。页面报 `{"done":true}` 时写一行计数（按路径的调用数、回退时刻、
//! 执行过的操作种类、已交出的帧字节）并退出。

use std::collections::BTreeMap;
use std::io::Write;
use std::path::PathBuf;
use std::sync::Mutex;
use std::time::{SystemTime, UNIX_EPOCH};

use iwork_proto::pb;
use prost::Message;
use tauri::{AppHandle, Manager, Runtime};

use crate::bridge::{Cmd, TRANSPORT_FALLBACK};

pub struct Harness {
    pub script: String,
    out: PathBuf,
    pub csp: Option<String>,
    pub other_window: bool,
    log: Mutex<Log>,
}

#[derive(Default)]
struct Log {
    /// 回退路径上每次调用的时刻（毫秒）与命令。
    fallback_at: Vec<(u128, &'static str)>,
    /// 自定义协议路径上真正执行了的操作，按种类计数（视图声明也算）。
    intents: BTreeMap<String, u64>,
}

fn now_ms() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or_default()
}

impl Harness {
    pub fn from_env() -> Option<Self> {
        let script_path = std::env::var_os("IWORK_E2E_SCRIPT")?;
        let script = std::fs::read_to_string(&script_path)
            .unwrap_or_else(|e| panic!("读不了 IWORK_E2E_SCRIPT={script_path:?}：{e}"));
        let out = std::env::var_os("IWORK_E2E_OUT")
            .map(PathBuf::from)
            .unwrap_or_else(|| panic!("设了 IWORK_E2E_SCRIPT 就要设 IWORK_E2E_OUT"));
        Some(Self {
            script,
            out,
            csp: std::env::var("IWORK_E2E_CSP").ok(),
            other_window: std::env::var_os("IWORK_E2E_OTHER").is_some(),
            log: Mutex::new(Log::default()),
        })
    }

    fn write(&self, line: &str) {
        let mut f = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.out)
            .unwrap_or_else(|e| panic!("写不了 {}：{e}", self.out.display()));
        writeln!(f, "{line}").unwrap_or_else(|e| panic!("写不了 {}：{e}", self.out.display()));
    }

    /// 每次命令调用之后记一笔（壳的命令入口调）。
    pub fn record(&self, cmd: Cmd, body: &[u8], result: &Result<Vec<u8>, String>) {
        let mut log = self.log.lock().unwrap_or_else(|e| e.into_inner());
        match result {
            Err(e) if e == TRANSPORT_FALLBACK => {
                let name = match cmd {
                    Cmd::Attach => "attach",
                    Cmd::NextFrame => "next_frame",
                    Cmd::UiIntent => "ui_intent",
                };
                log.fallback_at.push((now_ms(), name));
            }
            Ok(_) if cmd == Cmd::UiIntent => {
                let kind = pb::UiIntent::decode(body)
                    .ok()
                    .and_then(|i| i.kind)
                    .map(|k| {
                        let d = format!("{k:?}");
                        d.split('(').next().unwrap_or_default().to_owned()
                    })
                    .unwrap_or_else(|| "?".into());
                *log.intents.entry(kind).or_default() += 1;
            }
            _ => {}
        }
    }

    /// 页面改了标题：`E2E:` 开头的是报告。
    pub fn on_title<R: Runtime>(
        &self,
        app: &AppHandle<R>,
        window: &str,
        title: &str,
        stats: &iwork_client_core::Stats,
        counters: &crate::bridge::PathCounters,
    ) {
        let Some(json) = title.strip_prefix("E2E:") else {
            return;
        };
        self.write(&format!(
            r#"{{"window":"{window}","at":{},"report":{json}}}"#,
            now_ms()
        ));
        if !json.contains(r#""done":true"#) {
            return;
        }
        let log = self.log.lock().unwrap_or_else(|e| e.into_inner());
        let fallback_at: Vec<String> = log
            .fallback_at
            .iter()
            .map(|(t, c)| format!(r#"[{t},"{c}"]"#))
            .collect();
        let intents: Vec<String> = log
            .intents
            .iter()
            .map(|(k, v)| format!(r#""{k}":{v}"#))
            .collect();
        use std::sync::atomic::Ordering::Relaxed;
        self.write(&format!(
            r#"{{"counters":{{"custom_protocol":{},"fallback":{},"fallback_at":[{}],"intents":{{{}}},"handed_bytes":{},"frames_built":{}}}}}"#,
            counters.custom_protocol.load(Relaxed),
            counters.fallback.load(Relaxed),
            fallback_at.join(","),
            intents.join(","),
            stats.handed_bytes,
            stats.frames_built,
        ));
        app.exit(0);
    }
}

/// 副窗口（同来源、不是主窗口）：同一段脚本在里面跑，验证命令只收主窗口（R7 第 4 道）。
pub fn open_other<R: Runtime, M: Manager<R>>(
    app: &M,
    script: &str,
    on_title: impl Fn(String) + Send + 'static,
) -> tauri::Result<()> {
    tauri::WebviewWindowBuilder::new(app, "other", tauri::WebviewUrl::App("index.html".into()))
        .title("other")
        .inner_size(400.0, 300.0)
        .initialization_script(script)
        .on_document_title_changed(move |_, t| on_title(t))
        .build()
        .map(|_| ())
}
