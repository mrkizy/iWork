//! T06 方案 4.6 第 15 行：hook panic → 输出拒绝 JSON、退出码 2，panic 的文字不外泄。
//! 起的是 `examples/panic_probe`：它调的是产品同一个 `verdict::install_panic_hook`，
//! 产品二进制里没有任何触发 panic 的开关。

use std::path::PathBuf;
use std::process::Command;

use serde_json::Value;

const SECRET: &str = "panic-secret-t06-7f3a";

/// `cargo test` 把 examples 编到 `target/<profile>/examples/`，测试程序在同级的 `deps/` 里。
fn probe() -> PathBuf {
    let exe = std::env::current_exe().unwrap();
    let dir = exe.parent().unwrap().parent().unwrap().join("examples");
    let p = dir.join(format!("panic_probe{}", std::env::consts::EXE_SUFFIX));
    assert!(
        p.exists(),
        "找不到 {}（cargo test 会先编 examples）",
        p.display()
    );
    p
}

fn assert_panic_denies(where_: &str) {
    let out = Command::new(probe())
        .arg(where_)
        .env_clear()
        .output()
        .unwrap();
    let stdout = String::from_utf8(out.stdout).unwrap();
    let stderr = String::from_utf8(out.stderr).unwrap();
    assert_eq!(out.status.code(), Some(2), "{where_}: stderr={stderr}");
    let d: Value = serde_json::from_str(stdout.trim_end()).expect("stdout 是一行 JSON");
    let d = &d["hookSpecificOutput"];
    assert_eq!(d["hookEventName"], "PreToolUse");
    assert_eq!(d["permissionDecision"], "deny", "{where_}");
    assert!(
        d["permissionDecisionReason"]
            .as_str()
            .unwrap()
            .contains("出错"),
        "{d}"
    );
    assert!(!stdout.contains(SECRET), "{where_}: panic 文字进了 stdout");
    assert!(
        !stderr.contains(SECRET),
        "{where_}: panic 文字进了 stderr：{stderr}"
    );
}

#[test]
fn panic_on_main_thread_denies() {
    assert_panic_denies("main");
}

#[test]
fn panic_on_other_thread_denies() {
    assert_panic_denies("thread");
}

/// 对照：不 panic 时它按约定退出码 64 退出，证明上面两条的退出码 2 来自 panic 钩子。
#[test]
fn probe_without_panic_exits_normally() {
    let out = Command::new(probe()).arg("none").output().unwrap();
    assert_eq!(out.status.code(), Some(64));
    assert!(out.stdout.is_empty());
}
