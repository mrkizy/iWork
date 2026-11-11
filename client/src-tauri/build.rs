fn main() {
    // 应用自己的 ACL 清单：三个命令也要过 capability（只给主窗口、只给应用自身来源；方案 R7、E6）。
    tauri_build::try_build(tauri_build::Attributes::new().app_manifest(
        tauri_build::AppManifest::new().commands(&["attach", "next_frame", "ui_intent"]),
    ))
    .expect("tauri-build 失败");
}
