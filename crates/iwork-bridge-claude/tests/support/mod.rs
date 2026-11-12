//! 集成测试共用：拿到真的 hook 程序。
//!
//! hook 是另一个包（`iwork-claude-hook`）的二进制，这个包的测试拿不到 `CARGO_BIN_EXE_*`。直接去 target 目录里找
//! 可能找到旧的（只跑 `-p iwork-bridge-claude` 时 cargo 不会顺手重编它），所以这里让 cargo 按当前源码构建一次，
//! 用它报出来的路径。跑测试时 cargo 已经放开了构建锁，嵌套调用不会自己等自己。

#![allow(dead_code)]

#[cfg(unix)]
pub mod channel;

use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::OnceLock;

/// 当前源码构建出来的 `iwork-claude-hook`。整个测试进程只构建一次。
pub fn hook_program() -> &'static Path {
    static HOOK: OnceLock<PathBuf> = OnceLock::new();
    HOOK.get_or_init(|| {
        let out = Command::new(env!("CARGO"))
            .args([
                "build",
                "-p",
                "iwork-claude-hook",
                "--bin",
                "iwork-claude-hook",
                "--message-format=json-render-diagnostics",
            ])
            .current_dir(env!("CARGO_MANIFEST_DIR"))
            .output()
            .expect("起得了 cargo");
        assert!(
            out.status.success(),
            "构建 iwork-claude-hook 失败：{}",
            String::from_utf8_lossy(&out.stderr)
        );
        String::from_utf8_lossy(&out.stdout)
            .lines()
            .filter_map(|l| serde_json::from_str::<serde_json::Value>(l).ok())
            .filter(|m| m["target"]["name"] == "iwork-claude-hook")
            .filter_map(|m| m["executable"].as_str().map(PathBuf::from))
            .next_back()
            .expect("cargo 报出了 iwork-claude-hook 的路径")
    })
}
