//! 系统文件选择框（回形针、「重新定位」；T09 组 2b）。网页只说要选什么，路径在这里拿到，交回 client-core，
//! 由它发 `UploadAttachment` 或 `RelocateProject`、后台校验。网页拿不到文件能力（方案 R7）。
//!
//! 选择框在主线程上建（AppKit / GTK 都要求），建好后在别的任务里等用户选完，主线程不被占住。
//! 写法照 tauri-plugin-dialog 的桌面实现。

use iwork_client_core::{App, PickKind, ShellEvent};
use rfd::{AsyncFileDialog, FileHandle};
use tauri::{AppHandle, Manager};

use crate::MAIN;

/// 弹框，用户选完（或取消）后一定回一次 `ShellEvent::Picked`：client-core 靠它收掉「开着的选择框」。
pub fn open(handle: &AppHandle, core: App, kind: PickKind) {
    let (tx, rx) = tokio::sync::oneshot::channel();
    let main = handle.clone();
    let built = handle.run_on_main_thread(move || {
        let mut dialog = AsyncFileDialog::new();
        // 挂到主窗口上：macOS 上是窗口顶上的面板，不会被别的窗口挡住
        if let Some(w) = main.get_webview_window(MAIN) {
            dialog = dialog.set_parent(&w);
        }
        let picking = async move {
            match kind {
                PickKind::Files => dialog.pick_files().await.unwrap_or_default(),
                PickKind::Folder => dialog.pick_folder().await.into_iter().collect(),
            }
        };
        let _ = tx.send(picking);
    });
    if let Err(e) = built {
        tracing::warn!(target: "iwork_client", error = %e, "选择框没弹出来");
    }
    tauri::async_runtime::spawn(async move {
        // 主线程上没建成时 tx 被丢掉，这里得到空结果，照样回一次
        let picked: Vec<FileHandle> = match rx.await {
            Ok(picking) => picking.await,
            Err(_) => Vec::new(),
        };
        let paths = picked
            .iter()
            .map(|f| f.path().to_string_lossy().into_owned())
            .collect();
        core.shell(ShellEvent::Picked(paths));
    });
}
