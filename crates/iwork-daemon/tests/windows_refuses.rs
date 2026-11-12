//! M1 不支持 Windows（方案 2.15）：后台在启动表第 1 步就拒绝，逻辑退出码 10，stderr 写明原因，
//! 不建数据目录、不碰任何端点。CI 的 Windows job 跑这一条。
#![cfg(windows)]

use std::process::Command;

#[test]
fn unsupported_platform_refuses_at_identity() {
    let home = tempfile::tempdir().unwrap();
    let out = Command::new(env!("CARGO_BIN_EXE_iwork-daemon"))
        .env("LOCALAPPDATA", home.path())
        .env("IWORK_DATA_DIR", home.path().join("data"))
        .output()
        .unwrap();
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert_eq!(out.status.code(), Some(10), "{stderr}");
    assert!(stderr.contains("M1 暂不支持 Windows"), "{stderr}");
    assert!(
        !home.path().join("data").exists(),
        "第 1 步就停了，没建数据目录"
    );
}
