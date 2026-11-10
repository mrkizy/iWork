//! 壳的副作用：选端点、系统通知、打开外链、菜单「完全退出 iWork」、退出。

use iwork_client_core::{App, DirectDialer, Effect, ShellEvent};
use iwork_ipc::LocalEndpoint;
use iwork_ipc::peer::SelfIdentity;
use tauri::menu::{MenuBuilder, MenuItemBuilder, SubmenuBuilder};
use tauri::{AppHandle, Manager};
use tokio::sync::mpsc;

const QUIT_MENU_ID: &str = "quit-fully";

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
                Effect::Notify {
                    title,
                    body,
                    conversation_id,
                } => {
                    // 系统通知怎么落地待定：tauri-plugin-notification 带进被依赖策略禁用的 notify-rust 与三条
                    // unmaintained 公告（zbus 系），已交 supervisor。先记日志；点击跳转的会话侧接线
                    // （ShellEvent::NotificationClicked → NavigatePatch）已就绪。
                    tracing::info!(target: "iwork_client", %title, %body, ?conversation_id, "待发系统通知");
                }
                Effect::OpenExternal(url) => open_external(&url),
                Effect::Exit => handle.exit(0),
            }
        }
    });
}

fn install_menu(handle: &AppHandle, core: App) -> tauri::Result<()> {
    let quit = MenuItemBuilder::with_id(QUIT_MENU_ID, "完全退出 iWork").build(handle)?;
    let app_menu = SubmenuBuilder::new(handle, "iWork").item(&quit).build()?;
    let menu = MenuBuilder::new(handle).item(&app_menu).build()?;
    handle.set_menu(menu)?;
    handle.on_menu_event(move |app, event| {
        if event.id() == QUIT_MENU_ID {
            // 确认框由网页按 ConnectionView.quit_requested 画；确认后经 QuitIntent 回来。
            core.shell(ShellEvent::QuitRequested);
            if let Some(w) = app.get_webview_window("main") {
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
