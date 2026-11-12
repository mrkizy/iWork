//! 壳的副作用：选端点、Dock 角标与跳动、打开外链、系统文件选择框、「iWork」菜单的两种退出、退出。
//! 系统通知由后台发（T09 方案 2.8），客户端不发。

use iwork_client_core::{App, DirectDialer, Effect, ShellEvent};
use iwork_ipc::LocalEndpoint;
use iwork_ipc::peer::SelfIdentity;
use tauri::menu::{MenuBuilder, MenuItemBuilder, SubmenuBuilder};
use tauri::{AppHandle, Manager};

use crate::{MAIN, dock};
use tokio::sync::mpsc;

/// 「退出 iWork」：只退客户端（Cmd+Q），后台照常运行（Q16）。
const QUIT_CLIENT_ID: &str = "quit-client";
/// 「完全退出 iWork」：连后台一起关（Option+Cmd+Q）；macOS 上是上一项的 Option 替换项，按住 Option 才显示。
const QUIT_FULLY_ID: &str = "quit-fully";
const QUIT_FULLY_TITLE: &str = "完全退出 iWork";

/// 连哪：发布构建只认当前用户的默认端点；`dev` feature 下可用 `IWORK_DEV_ENDPOINT` 指向假后台（R8）。
pub fn dialer() -> Result<DirectDialer, String> {
    let own = SelfIdentity::detect().map_err(|e| e.to_string())?;
    #[cfg(feature = "dev")]
    if let Some(path) = std::env::var_os("IWORK_DEV_ENDPOINT") {
        let ep = iwork_ipc::transport::Endpoint::Unix(path.into());
        return Ok(DirectDialer::new(ep, own, env!("CARGO_PKG_VERSION")).dev_trust());
    }
    let ep = LocalEndpoint::for_current_user().map_err(|e| e.to_string())?;
    Ok(DirectDialer::new(
        ep.endpoint().clone(),
        own,
        env!("CARGO_PKG_VERSION"),
    ))
}

/// 菜单与效果循环。
pub fn spawn(handle: AppHandle, core: App, mut effects: mpsc::UnboundedReceiver<Effect>) {
    if let Err(e) = install_menu(&handle, core.clone()) {
        tracing::warn!(target: "iwork_client", error = %e, "菜单装不上");
    }
    tauri::async_runtime::spawn(async move {
        while let Some(e) = effects.recv().await {
            match e {
                // AppKit 只能在主线程上调
                Effect::SetBadge(label) => {
                    if let Err(e) =
                        handle.run_on_main_thread(move || dock::set_badge(label.as_deref()))
                    {
                        tracing::warn!(target: "iwork_client", error = %e, "Dock 角标没设上");
                    }
                }
                Effect::RequestAttention => {
                    if let Err(e) = handle.run_on_main_thread(dock::request_attention) {
                        tracing::warn!(target: "iwork_client", error = %e, "Dock 图标没跳");
                    }
                }
                Effect::OpenExternal(url) => open_external(&url),
                Effect::PickPaths(kind) => crate::picker::open(&handle, core.clone(), kind),
                Effect::Exit => handle.exit(0),
            }
        }
    });
}

fn install_menu(handle: &AppHandle, core: App) -> tauri::Result<()> {
    let quit_client = MenuItemBuilder::with_id(QUIT_CLIENT_ID, "退出 iWork")
        .accelerator("CmdOrCtrl+Q")
        .build(handle)?;
    let quit_fully = MenuItemBuilder::with_id(QUIT_FULLY_ID, QUIT_FULLY_TITLE)
        .accelerator("CmdOrCtrl+Alt+Q")
        .build(handle)?;
    let app_menu = SubmenuBuilder::new(handle, "iWork")
        .item(&quit_client)
        .item(&quit_fully)
        .build()?;
    // 换掉默认菜单后 macOS 的 Cmd+C/V/X/A/Z 靠「编辑」菜单里的预置项才生效（审查 1/F-7），补回来。
    let edit = SubmenuBuilder::new(handle, "编辑")
        .undo()
        .redo()
        .separator()
        .cut()
        .copy()
        .paste()
        .select_all()
        .build()?;
    let menu = MenuBuilder::new(handle)
        .item(&app_menu)
        .item(&edit)
        .build()?;
    handle.set_menu(menu)?;
    // macOS：「完全退出」平时藏着，按住 Option 时替换「退出」（Q16）。其他平台两项都显示。
    if cfg!(target_os = "macos") && !dock::make_alternate(QUIT_FULLY_TITLE) {
        tracing::warn!(target: "iwork_client", "没找到「完全退出 iWork」菜单项，两项都会显示");
    }
    handle.on_menu_event(move |app, event| {
        if event.id() == QUIT_CLIENT_ID {
            // 只退客户端：后台照常运行，不提示（Q16）。显式退出不受「关窗留在 Dock」拦截
            app.exit(0);
        } else if event.id() == QUIT_FULLY_ID {
            // 确认框由网页按 ConnectionView.quit_requested 画；确认后经 QuitIntent 回来。
            core.shell(ShellEvent::QuitRequested);
            if let Some(w) = app.get_webview_window(MAIN) {
                let _ = w.set_focus();
            }
        }
    });
    Ok(())
}

/// 外链交给系统默认浏览器（调用前已只放行 http / https）。
fn open_external(url: &str) {
    #[cfg(target_os = "macos")]
    let r = std::process::Command::new("open").arg(url).spawn();
    #[cfg(target_os = "linux")]
    let r = std::process::Command::new("xdg-open").arg(url).spawn();
    #[cfg(windows)]
    let r = std::process::Command::new("rundll32")
        .args(["url.dll,FileProtocolHandler", url])
        .spawn();
    if let Err(e) = r {
        tracing::warn!(target: "iwork_client", error = %e, "打不开外链");
    }
}
