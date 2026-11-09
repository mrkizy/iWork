//! iWork 客户端的 Tauri 壳：只负责启动窗口与把 `invoke` 转发给 `iwork-client-core`。
//! 连接、界面状态、帧合并等逻辑全部在 `iwork-client-core`（T09 填），这里不放业务判断。

// release 构建在 Windows 上不额外弹出控制台窗口；debug 保留控制台便于看日志。
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

fn main() {
    tauri::Builder::default()
        .run(tauri::generate_context!())
        .expect("Tauri 运行时启动失败");
}
