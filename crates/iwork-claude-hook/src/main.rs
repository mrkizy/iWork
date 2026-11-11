//! `iwork-claude-hook pre-tool-use` / `session-start`（T06 方案 2.3）。
//!
//! 不读配置文件、不写日志文件；诊断全在桥接一侧。结论怎么交、谁交、到时怎么收场见
//! [`iwork_claude_hook::verdict`]。

use std::io::Read;

use iwork_claude_hook::client::{self, ChannelEnv, Failure};
use iwork_claude_hook::deadline::{self, Started};
use iwork_claude_hook::output::Outcome;
use iwork_claude_hook::verdict;
use iwork_claude_hook::wire::{self, HookKind};

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
    verdict::install_panic_hook();
    let Some(kind) = parse_kind() else {
        verdict::finish(
            HookKind::PreToolUse,
            &Outcome::from_failure(Failure::BadArguments),
        );
    };
    let env = match ChannelEnv::from_env() {
        Ok(env) => env,
        Err(f) => verdict::finish(kind, &Outcome::from_failure(f)),
    };
    // 从这里起，卡在哪一步都由看门狗按时拒绝
    deadline::arm(started, env.deadline, move || verdict::expire(kind));
    let outcome = read_stdin_capped()
        .and_then(|body| client::ask(&env, kind, &body))
        .map_or_else(Outcome::from_failure, Outcome::from_response);
    verdict::finish(kind, &outcome)
}
