//! T06 方案 4.6 第 15 行：hook panic → 输出拒绝 JSON、退出码 2，panic 的文字不外泄。
//!
//! 不依赖别的构建产物：这个测试程序（`harness = false`）带上 [`PROBE_ENV`] 重新起自己，那一份就是探针：
//! 装上产品同一个 `verdict::install_panic_hook`，按环境变量在主线程或子线程 panic。产品二进制里没有开关。

use std::process::{Command, ExitCode};

use serde_json::Value;

/// 设了它，本程序就是探针：值是 `main` 或 `thread`，决定在哪个线程 panic。
const PROBE_ENV: &str = "IWORK_HOOK_PANIC_PROBE";
/// panic 文字里的暗号，断言它没从任何输出里漏出去。
const SECRET: &str = "panic-secret-t06-7f3a";

fn probe(where_: &str) -> ! {
    iwork_claude_hook::verdict::install_panic_hook();
    match where_ {
        "main" => panic!("{SECRET}"),
        "thread" => {
            let _ = std::thread::spawn(|| panic!("{SECRET}")).join();
            // 钩子里就结束了进程，走不到这里
            std::process::exit(0)
        }
        _ => std::process::exit(64),
    }
}

fn run_probe(where_: &str) -> std::process::Output {
    Command::new(std::env::current_exe().unwrap())
        .env_clear()
        .env(PROBE_ENV, where_)
        .output()
        .unwrap()
}

fn panic_denies(where_: &str) -> Result<(), String> {
    let out = run_probe(where_);
    let stdout = String::from_utf8_lossy(&out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);
    if out.status.code() != Some(2) {
        return Err(format!("退出码 {:?}，stderr={stderr}", out.status.code()));
    }
    let d: Value = serde_json::from_str(stdout.trim_end())
        .map_err(|e| format!("stdout 不是一行 JSON（{e}）：{stdout:?}"))?;
    let d = &d["hookSpecificOutput"];
    if d["hookEventName"] != "PreToolUse" || d["permissionDecision"] != "deny" {
        return Err(format!("不是拒绝：{d}"));
    }
    if !d["permissionDecisionReason"]
        .as_str()
        .unwrap_or("")
        .contains("出错")
    {
        return Err(format!("原因不对：{d}"));
    }
    if stdout.contains(SECRET) || stderr.contains(SECRET) {
        return Err(format!(
            "panic 文字外泄：stdout={stdout:?} stderr={stderr:?}"
        ));
    }
    Ok(())
}

/// 对照：不 panic 时按约定退出码 64、什么都不输出，证明上面的退出码 2 来自 panic 钩子。
fn probe_without_panic_exits_normally() -> Result<(), String> {
    let out = run_probe("none");
    match (out.status.code(), out.stdout.is_empty()) {
        (Some(64), true) => Ok(()),
        other => Err(format!("{other:?}")),
    }
}

/// 一条用例：通过返回 `Ok`，失败返回说明。
type Case = fn() -> Result<(), String>;

fn main() -> ExitCode {
    if let Ok(where_) = std::env::var(PROBE_ENV) {
        probe(&where_);
    }
    let cases: [(&str, Case); 3] = [
        ("panic_on_main_thread_denies", || panic_denies("main")),
        ("panic_on_other_thread_denies", || panic_denies("thread")),
        (
            "probe_without_panic_exits_normally",
            probe_without_panic_exits_normally,
        ),
    ];
    let mut failed = 0;
    for (name, case) in cases {
        match case() {
            Ok(()) => println!("test {name} ... ok"),
            Err(e) => {
                failed += 1;
                println!("test {name} ... FAILED：{e}");
            }
        }
    }
    println!("panic_denies：{} 通过，{failed} 失败", cases.len() - failed);
    if failed == 0 {
        ExitCode::SUCCESS
    } else {
        ExitCode::FAILURE
    }
}
