//! iWork 客户端的 Tauri 壳：起窗口、把三个命令转给 `iwork-client-core`、把效果（Dock 角标与跳动、外链、选择框、退出）
//! 落到系统上。连接、界面状态、帧合并等逻辑全部在 `iwork-client-core`，这里不放业务判断。
//!
//! macOS 上关窗只关窗口、进程留在 Dock（角标与跳动照常，方案 2.8）；点 Dock 图标重新开窗，新窗口 attach 后
//! 换一代取帧。Cmd+Q（「退出 iWork」）才退客户端进程。Linux 关窗即退出。

// release 构建在 Windows 上不额外弹出控制台窗口；debug 保留控制台便于看日志。
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod bridge;
mod dock;
#[cfg(feature = "e2e")]
mod e2e;
mod effects;
mod picker;

use std::sync::Arc;

use bridge::{Cmd, PathCounters};
use iwork_client_core::{App, Options, ShellEvent};
use tauri::ipc::{InvokeBody, Request, Response};
use tauri::webview::NewWindowResponse;
use tauri::{Manager, RunEvent, State, WebviewUrl, WebviewWindowBuilder, WindowEvent};

/// 主窗口的标识：capability 只放行它（capabilities/main.json）。
pub(crate) const MAIN: &str = "main";

struct Bridge {
    app: App,
    counters: Arc<PathCounters>,
    #[cfg(feature = "e2e")]
    harness: Option<Arc<e2e::Harness>>,
}

fn raw(request: &Request<'_>) -> Vec<u8> {
    match request.body() {
        InvokeBody::Raw(b) => b.clone(),
        // JSON 体只会出现在回退路径上；入口检查会先拒掉它，这里给空即可。
        InvokeBody::Json(_) => Vec::new(),
    }
}

async fn call(state: &Bridge, cmd: Cmd, request: &Request<'_>) -> Result<Response, String> {
    let body = raw(request);
    let result = bridge::dispatch(&state.app, &state.counters, cmd, request.headers(), &body).await;
    #[cfg(feature = "e2e")]
    if let Some(h) = &state.harness {
        h.record(cmd, &body, &result);
    }
    result.map(Response::new)
}

#[tauri::command]
async fn attach(state: State<'_, Bridge>, request: Request<'_>) -> Result<Response, String> {
    call(&state, Cmd::Attach, &request).await
}

#[tauri::command]
async fn next_frame(state: State<'_, Bridge>, request: Request<'_>) -> Result<Response, String> {
    call(&state, Cmd::NextFrame, &request).await
}

#[tauri::command]
async fn ui_intent(state: State<'_, Bridge>, request: Request<'_>) -> Result<Response, String> {
    call(&state, Cmd::UiIntent, &request).await
}

/// 主窗口只停在应用自身来源（R7、E6）：开发构建是开发服务器，发布构建是 tauri 自定义协议。
fn is_app_origin(url: &tauri::Url) -> bool {
    match url.scheme() {
        "tauri" => true,
        "http" | "https" => {
            url.host_str() == Some("tauri.localhost")
                || (cfg!(debug_assertions)
                    && url.host_str() == Some("127.0.0.1")
                    && url.port() == Some(1420))
        }
        _ => false,
    }
}

/// 主窗口的建法（启动时、macOS 上点 Dock 图标重新开窗时共用）。
fn main_window<'a, M: Manager<tauri::Wry>>(m: &'a M) -> WebviewWindowBuilder<'a, tauri::Wry, M> {
    WebviewWindowBuilder::new(m, MAIN, WebviewUrl::App("index.html".into()))
        .title("iWork")
        .inner_size(1440.0, 900.0)
        // 最小宽度 = tokens.css 的 --window-min-w（ui-m1 2a 第 12b 条，client/tests/window-min-width.test.ts 对拍）
        .min_inner_size(880.0, 640.0)
        .on_navigation(|url| {
            let ok = is_app_origin(url);
            if !ok {
                // 只记来源（协议 + 主机 + 端口）：路径与查询串里可能有用户内容
                tracing::warn!(target: "iwork_client", origin = %url.origin().ascii_serialization(), "主窗口导航到别的来源，已拦下");
            }
            ok
        })
        // 不开新窗口；外链只经 OpenExternal（网页发 UiIntent，Rust 只放行 http/https）。
        .on_new_window(|_, _| NewWindowResponse::Deny)
}

/// macOS 上点 Dock 图标：窗口关着就重新开一个（新窗口的网页 attach 后换一代取帧，全量开始）。
#[cfg(target_os = "macos")]
fn reopen(app: &tauri::AppHandle) {
    if let Some(w) = app.get_webview_window(MAIN) {
        let _ = w.set_focus();
        return;
    }
    match main_window(app).build() {
        Ok(_) => {
            if let Some(bridge) = app.try_state::<Bridge>() {
                bridge.app.shell(ShellEvent::WindowOpen(true));
            }
        }
        Err(e) => tracing::error!(target: "iwork_client", error = %e, "重新开窗失败"),
    }
}

/// 客户端的可调项。开发构建（`dev` feature）可用 `IWORK_DEV_TIMELINE_CAP` 把时间线行数上限调小，方便对着假后台看裁剪
/// 与翻页（至少 2：一组最多两行，方案 2.2 第 4 条）；发布构建不认识这个变量（R8）。
fn options() -> Options {
    #[allow(unused_mut, reason = "只有 dev 构建改上限")]
    let mut o = Options::default();
    #[cfg(feature = "dev")]
    if let Ok(v) = std::env::var("IWORK_DEV_TIMELINE_CAP") {
        match v.parse::<usize>() {
            Ok(cap) if cap >= 2 => o.timeline_cap = cap,
            _ => eprintln!("[dev] IWORK_DEV_TIMELINE_CAP={v} 不是不小于 2 的整数，用默认上限"),
        }
    }
    o
}

fn main() {
    // 日志最先装：第一次拨号时取身份（清单审计）、判定后台（拒绝审计）都要有地方记（T03 方案 2.12）
    let _log = iwork_client_core::logging::init();
    // 建不成拨号器（身份取不到、端点目录不安全）不在这里 panic：第一次拨号回 Environment，窗口照常开、整页显示原因、
    // 留「重试」，修好后重试能连上（T04p 方案 4.7）
    let dialer = effects::dialer();
    #[allow(unused_mut, reason = "只有 e2e 构建改上下文")]
    let mut context = tauri::generate_context!();
    #[cfg(feature = "e2e")]
    let harness = e2e::Harness::from_env().map(Arc::new);
    #[cfg(feature = "e2e")]
    if let Some(csp) = harness.as_ref().and_then(|h| h.csp.clone()) {
        context.config_mut().app.security.csp = Some(tauri::utils::config::Csp::Policy(csp));
    }
    #[cfg(feature = "e2e")]
    let setup_harness = harness.clone();
    tauri::Builder::default()
        .invoke_handler(tauri::generate_handler![attach, next_frame, ui_intent])
        .setup(move |app| {
            let (core, effects) =
                tauri::async_runtime::block_on(async { App::start(dialer, options()) });
            app.manage(Bridge {
                app: core.clone(),
                counters: Arc::new(PathCounters::default()),
                #[cfg(feature = "e2e")]
                harness: setup_harness.clone(),
            });
            #[allow(unused_mut, reason = "只有 e2e 构建加初始化脚本")]
            let mut window = main_window(app);
            #[cfg(feature = "e2e")]
            if let Some(h) = &setup_harness {
                window = window
                    .initialization_script(h.script.clone())
                    .on_document_title_changed(e2e_title_handler(MAIN));
                if h.other_window {
                    let handle = app.handle().clone();
                    e2e::open_other(app, &h.script, move |t| e2e_on_title(&handle, "other", &t))?;
                }
            }
            window.build()?;
            effects::spawn(app.handle().clone(), core, effects);
            Ok(())
        })
        .on_window_event(|window, event| {
            let Some(bridge) = window.app_handle().try_state::<Bridge>() else {
                return;
            };
            match event {
                WindowEvent::Focused(f) => bridge.app.shell(ShellEvent::Focused(*f)),
                // 主窗口关了（macOS 上进程留在 Dock）：没人在看了
                WindowEvent::Destroyed if window.label() == MAIN => {
                    bridge.app.shell(ShellEvent::WindowOpen(false));
                }
                // 拖文件进来：拖动中给对话区画虚线框，松开把路径附到当前对话（ui-m1.md 第 2 节第 6 条）
                WindowEvent::DragDrop(drag) => match drag {
                    tauri::DragDropEvent::Enter { .. } => {
                        bridge.app.shell(ShellEvent::Dragging(true));
                    }
                    tauri::DragDropEvent::Leave => bridge.app.shell(ShellEvent::Dragging(false)),
                    tauri::DragDropEvent::Drop { paths, .. } => {
                        let paths = paths
                            .iter()
                            .map(|p| p.to_string_lossy().into_owned())
                            .collect();
                        bridge.app.shell(ShellEvent::Dropped(paths));
                    }
                    _ => {}
                },
                _ => {}
            }
        })
        .build(context)
        .expect("Tauri 运行时启动失败")
        .run(|app, event| match event {
            // 最后一个窗口关了（不是显式退出）：macOS 上进程留在 Dock，角标与跳动照常（方案 2.8）
            RunEvent::ExitRequested {
                code: None, api, ..
            } if cfg!(target_os = "macos") => {
                api.prevent_exit();
            }
            #[cfg(target_os = "macos")]
            RunEvent::Reopen { .. } => reopen(app),
            // 事件循环结束后进程直接退出、不跑 main 里的析构：日志队列在这里排空
            RunEvent::Exit => iwork_client_core::logging::flush_global(),
            _ => {
                let _ = app;
            }
        });
}

#[cfg(feature = "e2e")]
fn e2e_on_title<R: tauri::Runtime>(app: &tauri::AppHandle<R>, window: &str, title: &str) {
    if let Some(bridge) = app.try_state::<Bridge>()
        && let Some(h) = &bridge.harness
    {
        let stats = bridge.app.stats().borrow().clone();
        h.on_title(app, window, title, &stats, &bridge.counters);
    }
}

#[cfg(feature = "e2e")]
fn e2e_title_handler(
    label: &'static str,
) -> impl Fn(tauri::WebviewWindow, String) + Send + 'static {
    move |w, t| e2e_on_title(w.app_handle(), label, &t)
}
