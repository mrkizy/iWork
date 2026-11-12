//! 把结论交给 Claude Code，并结束进程（T06 方案 2.3、4.6 第 14–16 行）。
//!
//! 结论只交一次：主线程、看门狗、panic 钩子谁先**认领**谁交，认领是一次原子交换，不是锁。
//! 交结论要写 stdout，读端不读、结论又超过管道缓冲时这一步会一直卡住；所以别的路径**从不等**
//! 认领者写完：看门狗到时发现已被认领，就只写一行 stderr、以退出码 2 结束进程（Claude Code
//! 按拦下处理），绝不让 hook 拖到 Claude Code 自己的超时（超时即放行）。
//! 进程退出用 `std::process::exit`，不跑 std 的 stdout 清理，不会去等被卡住的 stdout 锁。

use std::io::Write;
use std::process::exit;
use std::sync::atomic::{AtomicBool, Ordering};

use crate::client::Failure;
use crate::output::{Outcome, pre_tool_use_json};
use crate::wire::HookKind;

/// stdout 写不出去、到时而结论还没交完、panic 时的退出码：Claude Code 把它当作拦下这次调用。
pub const EXIT_BLOCK: i32 = 2;

static CLAIMED: AtomicBool = AtomicBool::new(false);

/// 认领交结论的权利。只有第一个调用者拿到 `true`。
fn claim() -> bool {
    !CLAIMED.swap(true, Ordering::SeqCst)
}

/// 没认领到：认领者会结束进程，这里就停着等它，不做任何会和它抢的事。
fn park_forever() -> ! {
    loop {
        std::thread::park();
    }
}

/// 只写 stderr 并以退出码 2 结束进程。stderr 写失败也照样退出。
fn block_via_stderr(reason: &str) -> ! {
    let _ = writeln!(std::io::stderr(), "{reason}");
    exit(EXIT_BLOCK)
}

/// 已认领：写 PreToolUse 的结论并退出。写不出去 → 退出码 2，原因写 stderr。
fn write_and_exit(outcome: &Outcome) -> ! {
    let line = pre_tool_use_json(outcome);
    let mut out = std::io::stdout().lock();
    if writeln!(out, "{line}").and_then(|()| out.flush()).is_err() {
        block_via_stderr(Failure::Internal.reason());
    }
    exit(0)
}

/// 主线程交结论。PreToolUse 写结论 JSON；SessionStart 报到不输出决定，退出码 0
/// （报到没成，桥接那边收不到报到自己会停，方案 3.5）。
pub fn finish(kind: HookKind, outcome: &Outcome) -> ! {
    if !claim() {
        park_forever();
    }
    match kind {
        HookKind::PreToolUse => write_and_exit(outcome),
        HookKind::SessionStart => exit(0),
    }
}

/// 看门狗到时。还没人认领：交一个「到时」的拒绝。已被认领（主线程可能正卡在写 stdout 上）：
/// 不等它，写 stderr、退出码 2。
pub fn expire(kind: HookKind) -> ! {
    match kind {
        HookKind::SessionStart => exit(0),
        HookKind::PreToolUse if claim() => {
            write_and_exit(&Outcome::from_failure(Failure::Deadline))
        }
        HookKind::PreToolUse => block_via_stderr(Failure::Deadline.reason()),
    }
}

/// PreToolUse 开工前确认 stdout 交得到调用方手里，交不到就以退出码 2 结束（方案 3.4a 第 3 条）。
///
/// 启动时 stdout 是关着的，Rust 运行时会把 1 号描述符换成 `/dev/null`（不然后面打开的 socket 会占到它）。
/// 那样结论「写成功」了，CLI 却什么都没收到，以 0 退出就造出了「退出码 0、没有决定」。CLI 给 hook 的
/// stdout 总是管道，不会是 `/dev/null`，所以把它当成交不出去。
pub fn require_deliverable_stdout() {
    if stdout_is_dev_null() {
        block_via_stderr(Failure::Internal.reason());
    }
}

#[cfg(unix)]
fn stdout_is_dev_null() -> bool {
    use std::os::fd::AsFd;
    use std::os::unix::fs::{FileTypeExt, MetadataExt};

    let Ok(null) = std::fs::metadata("/dev/null") else {
        return false;
    };
    let Ok(out) = std::io::stdout().as_fd().try_clone_to_owned() else {
        // 描述符都拿不到：写结论时同样会失败，交给那一步按写失败处理
        return false;
    };
    match std::fs::File::from(out).metadata() {
        Ok(m) => m.file_type().is_char_device() && m.rdev() == null.rdev(),
        Err(_) => false,
    }
}

#[cfg(not(unix))]
fn stdout_is_dev_null() -> bool {
    // M1 不支持 Windows（AGENTS.md）；那里的标准句柄没有被换成 NUL 的这一步
    false
}

/// 装 panic 钩子：任何线程 panic 都交一个拒绝、退出码 2。panic 的文字不输出（可能带任何东西）。
/// 已被认领时只写 stderr，退出码 2 同样拦下调用。
pub fn install_panic_hook() {
    std::panic::set_hook(Box::new(|_| {
        let reason = Failure::Internal.reason();
        if claim() {
            let line = pre_tool_use_json(&Outcome::deny(reason));
            let mut out = std::io::stdout().lock();
            let _ = writeln!(out, "{line}").and_then(|()| out.flush());
        }
        block_via_stderr(reason)
    }));
}
