//! 集成测试共用：拿到真的 hook 程序与假 CLI。
//!
//! hook 是另一个包（`iwork-claude-hook`）的二进制，这个包的测试拿不到 `CARGO_BIN_EXE_*`；假 CLI 要 `fake-cli`
//! feature，`cargo test --workspace` 不开它。直接去 target 目录里找可能找到旧的，所以这里让 cargo 按当前源码构建
//! 一次，用它报出来的路径。跑测试时 cargo 已经放开了构建锁，嵌套调用不会自己等自己。

#![allow(dead_code)]

#[cfg(unix)]
pub mod acp;
#[cfg(unix)]
pub mod channel;
#[cfg(unix)]
pub mod procs;
#[cfg(unix)]
pub mod startup;

use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::OnceLock;

/// 当前源码构建出来的 `iwork-claude-hook`。整个测试进程只构建一次。
pub fn hook_program() -> &'static Path {
    static HOOK: OnceLock<PathBuf> = OnceLock::new();
    HOOK.get_or_init(|| build("iwork-claude-hook", "iwork-claude-hook", None))
}

/// 当前源码构建出来的假 CLI（`fake-cli` feature，方案 15.1）。整个测试进程只构建一次。
pub fn fake_claude() -> &'static Path {
    static FAKE: OnceLock<PathBuf> = OnceLock::new();
    FAKE.get_or_init(|| build("iwork-bridge-claude", "iwork-fake-claude", Some("fake-cli")))
}

/// 让 cargo 按当前源码构建一个二进制，返回它报出来的路径。
fn build(package: &str, bin: &str, features: Option<&str>) -> PathBuf {
    let mut cmd = Command::new(env!("CARGO"));
    cmd.args([
        "build",
        "-p",
        package,
        "--bin",
        bin,
        "--message-format=json-render-diagnostics",
    ]);
    if let Some(f) = features {
        cmd.args(["--features", f]);
    }
    let out = cmd
        .current_dir(env!("CARGO_MANIFEST_DIR"))
        .output()
        .expect("起得了 cargo");
    assert!(
        out.status.success(),
        "构建 {bin} 失败：{}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout)
        .lines()
        .filter_map(|l| serde_json::from_str::<serde_json::Value>(l).ok())
        .filter(|m| m["target"]["name"] == bin)
        .filter_map(|m| m["executable"].as_str().map(PathBuf::from))
        .next_back()
        .unwrap_or_else(|| panic!("cargo 报出了 {bin} 的路径"))
}
