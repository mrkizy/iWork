//! iWork 客户端的 Tauri 壳：起窗口、把三个命令转给 `iwork-client-core`、把效果（通知、外链、退出）落到系统上。
//! 连接、界面状态、帧合并等逻辑全部在 `iwork-client-core`，这里不放业务判断。

// release 构建在 Windows 上不额外弹出控制台窗口；debug 保留控制台便于看日志。
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod bridge;
#[cfg(feature = "e2e")]
mod e2e;
mod effects;

use std::sync::Arc;

use bridge::{Cmd, PathCounters};
use iwork_client_core::{App, Options, ShellEvent};
use tauri::ipc::{InvokeBody, Request, Response};
use tauri::webview::NewWindowResponse;
use tauri::{Manager, State, WebviewUrl, WebviewWindowBuilder, WindowEvent};

/// 主窗口的标识：capability 只放行它（capabilities/main.json）。
const MAIN: &str = "main";

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

fn main() {
    let dialer = effects::dialer().unwrap_or_else(|e| panic!("找不到后台服务的端点：{e}"));
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
            let (core, effects) = tauri::async_runtime::block_on(async { App::start(dialer, Options::default()) });
            app.manage(Bridge {
                app: core.clone(),
                counters: Arc::new(PathCounters::default()),
                #[cfg(feature = "e2e")]
                harness: setup_harness.clone(),
            });
            #[allow(unused_mut, reason = "只有 e2e 构建加初始化脚本")]
            let mut window = WebviewWindowBuilder::new(app, MAIN, WebviewUrl::App("index.html".into()));
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
            window
                .title("iWork")
                .inner_size(1440.0, 900.0)
                .min_inner_size(1024.0, 640.0)
                .on_navigation(|url| {
                    let ok = is_app_origin(url);
                    if !ok {
                        tracing::warn!(target: "iwork_client", %url, "主窗口导航到别的来源，已拦下");
                    }
                    ok
                })
                // 不开新窗口；外链只经 OpenExternal（网页发 UiIntent，Rust 只放行 http/https）。
                .on_new_window(|_, _| NewWindowResponse::Deny)
                .build()?;
            effects::spawn(app.handle().clone(), core, effects);
            Ok(())
        })
        .on_window_event(|window, event| {
            let Some(bridge) = window.app_handle().try_state::<Bridge>() else {
                return;
            };
            match event {
                WindowEvent::Focused(f) => bridge.app.shell(ShellEvent::Focused(*f)),
                WindowEvent::DragDrop(tauri::DragDropEvent::Drop { paths, .. }) => {
                    let paths = paths.iter().map(|p| p.to_string_lossy().into_owned()).collect();
                    bridge.app.shell(ShellEvent::Dropped(paths));
                }
                _ => {}
            }
        })
        .run(context)
        .expect("Tauri 运行时启动失败");
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
