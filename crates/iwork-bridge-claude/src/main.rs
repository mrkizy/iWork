//! Claude Code 桥接：ACP agent 端 ↔ Claude Code CLI（`-p` + stream-json + hook）（T06 方案 2.2）。
//!
//! 子命令：`acp`（默认，stdin/stdout 说 ACP）。`probe`、`login`、`verify`、`forget` 随 S8、S7、S6b 加。
//!
//! 日志只写 stderr（stdout 是 ACP 的通道），后台按运行时的 stderr 收（T05：不直接进后台日志，由它过滤）。
//! 级别默认 info，环境变量 [`ENV_LOG`] 可以改成 error / warn / info / debug / trace。

/// 日志级别的环境变量。
const ENV_LOG: &str = "IWORK_BRIDGE_LOG";

#[cfg(unix)]
fn main() {
    use iwork_bridge_claude::{diag, reaper};
    // 1. 后台是谁：后台起桥接时经环境变量告诉它（审查 S6-5b 2/F-2）。读完就从环境里去掉：之后起 CLI 的环境取自
    //    桥接自己的环境，这个号不能带给 CLI 和它的子进程
    let supervisor = take_supervisor();
    // 2. 先于任何线程和运行时：它们会开自己的句柄，关的时候不能把它们一起关掉
    let inherited = close_inherited_fds();
    // 3. 诊断输出走队列和写线程，任何地方写日志都不会被 stderr 卡住（2/F-3）
    let _ = diag::start();
    init_logging();
    abort_on_panic();
    if let Err(e) = inherited {
        tracing::warn!(error = %e, "列不出继承来的句柄，没有关");
    }
    // 4. 不是后台起的：不起来（失败即关闭，不去猜谁是后台）
    let Some(supervisor) = supervisor else {
        refuse(&format!(
            "没有合法的 {}：不是后台起的，拒绝启动",
            reaper::ENV_SUPERVISOR_PID
        ));
    };
    // 5. 收尾线程：一起来就先比一次父进程，后台在桥接进 main 之前就没了的话当场收尾（2/F-2）；之后后台没了、
    //    输入结束、致命错误后等满 TEARDOWN_WAIT，都由它收尾（5.4a 第 5 条第二种）
    let leader = become_group_leader();
    if let Err(e) = reaper::start(supervisor, leader) {
        refuse(&format!("起不来收尾线程：{e}"));
    }
    // 一个 CLI、一条 ACP 连接、少量并发的 hook 连接，不需要线程池（方案第 14 节）
    let runtime = match tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
    {
        Ok(rt) => rt,
        Err(e) => refuse(&format!("起不来异步运行时：{e}")),
    };
    runtime.block_on(run());
}

#[cfg(unix)]
async fn run() {
    use iwork_bridge_claude::reaper::{self, Event};
    watch_hangup();
    let sub = std::env::args().nth(1);
    let message = match sub.as_deref() {
        None | Some("acp") => match iwork_bridge_claude::acp::BridgeConfig::from_process() {
            Ok(config) => iwork_bridge_claude::acp::serve_stdio(config).await,
            Err(e) => e,
        },
        Some(other) => format!("不认识的子命令：{other}"),
    };
    // 起不来（还没有 CLI）：交给收尾线程结束进程
    tracing::error!(%message, "桥接退出");
    reaper::notify(Event::Ended("桥接起不来"));
    std::future::pending::<()>().await;
}

/// 读出后台的进程号并从环境里去掉。缺失、不是正整数、是 1 都算不合法。
#[cfg(unix)]
fn take_supervisor() -> Option<libc::pid_t> {
    use iwork_bridge_claude::reaper::ENV_SUPERVISOR_PID;
    let value = std::env::var(ENV_SUPERVISOR_PID).ok();
    // SAFETY: 这时还是单线程，没有别人在读写环境
    unsafe { std::env::remove_var(ENV_SUPERVISOR_PID) };
    value?.parse::<libc::pid_t>().ok().filter(|p| *p > 1)
}

/// 不起来：记一行，有上限地等它写出去，退出码 2。这时还没有 CLI，不用收尾。
#[cfg(unix)]
fn refuse(message: &str) -> ! {
    tracing::error!(message, "桥接拒绝启动");
    iwork_bridge_claude::diag::flush(std::time::Duration::from_millis(500));
    std::process::exit(2);
}

/// 任何线程、任何任务 panic 都让桥接立刻结束（会话任务是单独起的任务，默认它 panic 只结束它自己：手里的 CLI 被
/// 丢掉，父链断了，收摊也永远到不了；停放期第 12 条）。不调默认的 panic 处理：它同步写 stderr，管道满了就卡在那里，
/// 到不了下一步（2/F-3）。诊断只往队列里放一行，放不进就丢。
///
/// - 后台还在：abort，走「桥接崩了」那条路，后台看到输出到头就清树，这时 CLI 还活着，范围找得全；
/// - 后台已经没了：没人会来清，就地收尾（和收尾线程是同一个函数，两边同时做也没关系）。
#[cfg(unix)]
fn abort_on_panic() {
    use iwork_bridge_claude::{diag, reaper};
    std::panic::set_hook(Box::new(|info| {
        diag::line(&format!("iwork-bridge-claude: panic，桥接立刻结束：{info}"));
        if reaper::daemon_alive() {
            std::process::abort();
        }
        reaper::self_clean();
    }));
}

/// 关掉从后台继承来的、0、1、2 以外的句柄。桥接只用 stdin、stdout、stderr，别的句柄都不是给它的：macOS 上 std
/// 建管道和设 close-on-exec 是两步，后台同时起两个子进程时，后起的那个可能继承到先起那个的管道（比如另一个桥接
/// stdin 的写端）。桥接拿着它（再传给 CLI 和 CLI 起的命令），后台关掉自己那一端时，另一个桥接就读不到 stdin
/// 结束，没人管地一直活着（漏桥接）。先把号码列全再关：列目录用的那个句柄这时已经关了，关它只会得到 EBADF。
#[cfg(unix)]
fn close_inherited_fds() -> std::io::Result<()> {
    let fds: Vec<libc::c_int> = std::fs::read_dir("/dev/fd")?
        .filter_map(|e| e.ok()?.file_name().to_str()?.parse().ok())
        .filter(|fd| *fd > 2)
        .collect();
    for fd in fds {
        // SAFETY: 这时还是单线程、运行时还没起，进程里没有别人拿着这些号码；关一个已经关了的号码只返回 EBADF
        unsafe { libc::close(fd) };
    }
    Ok(())
}

/// 确认自己是进程组组长（后台以 `process_group(0)` 起桥接时本来就是）；不是就自立一组：开发时从 shell 直接起
/// 桥接，最后那一下 `killpg` 不能把 shell 的进程组一起杀掉（方案 5.4）。返回最后能不能对自己的组发信号。
#[cfg(unix)]
fn become_group_leader() -> bool {
    // SAFETY: 这几个调用只读写整数
    unsafe {
        if libc::getpgrp() == libc::getpid() {
            return true;
        }
        if libc::setpgid(0, 0) == 0 {
            return true;
        }
    }
    tracing::warn!(error = %std::io::Error::last_os_error(), "没能自立进程组，退出时不对进程组发信号");
    false
}

/// 给 SIGHUP 装处理函数（不是忽略：忽略会被 CLI 继承，处理函数不会）。后台死在冻结中途时系统给这一组发
/// SIGHUP、SIGCONT，桥接要活下来，由收尾线程看到父进程变了、走自己收尾那条路（5.4a 第 5 条）。
#[cfg(unix)]
fn watch_hangup() {
    use tokio::signal::unix::{SignalKind, signal};
    match signal(SignalKind::hangup()) {
        Ok(mut hup) => {
            tokio::spawn(async move {
                while hup.recv().await.is_some() {
                    tracing::warn!("收到 SIGHUP：后台可能没了，收尾线程会看到");
                }
            });
        }
        Err(e) => tracing::warn!(error = %e, "装不上 SIGHUP 处理函数"),
    }
}

/// 装 stderr 上的日志订阅器。级别写错了就用 info，并且说一声。
#[cfg(unix)]
fn init_logging() {
    let raw = std::env::var(ENV_LOG).ok();
    let level = raw
        .as_deref()
        .and_then(|v| v.parse::<tracing::Level>().ok())
        .unwrap_or(tracing::Level::INFO);
    tracing_subscriber::fmt()
        .with_writer(|| iwork_bridge_claude::diag::Writer)
        .with_ansi(false)
        .with_max_level(level)
        .init();
    if let Some(v) = raw.filter(|v| v.parse::<tracing::Level>().is_err()) {
        tracing::warn!(value = %v, "{ENV_LOG} 不认识，用 info");
    }
}

/// M1 不支持 Windows（AGENTS.md）。
#[cfg(not(unix))]
fn main() {
    let _ = ENV_LOG;
    eprintln!("iwork-bridge-claude: 这个平台还没接入");
    std::process::exit(2);
}
