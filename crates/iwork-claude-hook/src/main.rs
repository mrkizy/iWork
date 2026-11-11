//! `iwork-claude-hook pre-tool-use` / `session-start`（T06 方案 2.3）。
//!
//! 不读配置文件、不写日志文件；诊断全在桥接一侧。结论只输出一次：主线程、看门狗、panic 钩子谁先到
//! 谁写，后到的不再写。

use std::io::{Read, Write};
use std::process::exit;
use std::sync::Mutex;

use iwork_claude_hook::client::{self, ChannelEnv, Failure};
use iwork_claude_hook::deadline::{self, Started};
use iwork_claude_hook::output::{Outcome, pre_tool_use_json};
use iwork_claude_hook::wire::{self, HookKind};

/// stdout 写不出去、或 panic 时的退出码：Claude Code 把它当作拦下这次调用。
const EXIT_BLOCK: i32 = 2;

/// 结论只写一次。持有者写完就退出进程，别的线程拿不到锁就一直等到进程结束。
static DECIDED: Mutex<()> = Mutex::new(());

/// 输出 PreToolUse 的结论并退出。stdout 写不出去 → 退出码 2，原因写 stderr。
fn decide(outcome: &Outcome) -> ! {
    let _guard = DECIDED.lock().unwrap_or_else(|p| p.into_inner());
    let line = pre_tool_use_json(outcome);
    let mut out = std::io::stdout().lock();
    if writeln!(out, "{line}").and_then(|()| out.flush()).is_err() {
        let _ = writeln!(std::io::stderr(), "{}", Failure::Internal.reason());
        exit(EXIT_BLOCK);
    }
    exit(0)
}

/// 报到结束：不输出决定，退出码 0（报到没成，桥接那边收不到报到自己会停，方案 3.5）。
fn done_quietly() -> ! {
    let _guard = DECIDED.lock().unwrap_or_else(|p| p.into_inner());
    exit(0)
}

fn finish(kind: HookKind, outcome: &Outcome) -> ! {
    match kind {
        HookKind::PreToolUse => decide(outcome),
        HookKind::SessionStart => done_quietly(),
    }
}

/// panic 也要给出拒绝，不指望 Claude Code 怎么处理异常退出码。锁被占着（另一条线程正在写结论，
/// 或 panic 就发生在写的时候）就只写 stderr，退出码 2 同样拦下调用。
fn on_panic(_: &std::panic::PanicHookInfo<'_>) {
    let reason = Failure::Internal.reason();
    if let Ok(_guard) = DECIDED.try_lock() {
        let line = pre_tool_use_json(&Outcome::deny(reason));
        let _ = writeln!(std::io::stdout(), "{line}");
        let _ = std::io::stdout().flush();
    }
    let _ = writeln!(std::io::stderr(), "{reason}");
    exit(EXIT_BLOCK);
}

fn parse_kind() -> Option<HookKind> {
    let mut args = std::env::args_os().skip(1);
    let kind = match args.next()?.to_str()? {
        "pre-tool-use" => HookKind::PreToolUse,
        "session-start" => HookKind::SessionStart,
        _ => return None,
    };
    args.next().is_none().then_some(kind)
}

/// 读 stdin，最多多读一个字节用来判断超没超上限。
fn read_stdin_capped() -> Result<Vec<u8>, Failure> {
    let limit = u64::from(wire::MAX_BODY_BYTES);
    let mut buf = Vec::new();
    std::io::stdin()
        .lock()
        .take(limit + 1)
        .read_to_end(&mut buf)
        .map_err(|_| Failure::Internal)?;
    if buf.len() as u64 > limit {
        return Err(Failure::InputTooLarge);
    }
    Ok(buf)
}

fn main() {
    let started = Started::now();
    std::panic::set_hook(Box::new(on_panic));
    let Some(kind) = parse_kind() else {
        decide(&Outcome::from_failure(Failure::BadArguments));
    };
    let env = match ChannelEnv::from_env() {
        Ok(env) => env,
        Err(f) => finish(kind, &Outcome::from_failure(f)),
    };
    // 从这里起，卡在哪一步都由看门狗按时拒绝
    deadline::arm(started, env.deadline, move || {
        finish(kind, &Outcome::from_failure(Failure::Deadline))
    });
    let outcome = read_stdin_capped()
        .and_then(|body| client::ask(&env, kind, &body))
        .map_or_else(Outcome::from_failure, Outcome::from_response);
    finish(kind, &outcome)
}
