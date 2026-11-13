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
    abort_on_panic();
    // 最先记下父进程（后台）：之后它一换，就是后台没了（5.4a 第 5 条，审查 S6-5b 1/F-3）
    // SAFETY: getppid 不读写任何内存
    let parent = unsafe { libc::getppid() };
    // 先于运行时：运行时会开自己的句柄，关的时候不能把它们一起关掉
    let inherited = close_inherited_fds();
    init_logging();
    if let Err(e) = inherited {
        tracing::warn!(error = %e, "列不出继承来的句柄，没有关");
    }
    // 一个 CLI、一条 ACP 连接、少量并发的 hook 连接，不需要线程池（方案第 14 节）
    let runtime = match tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
    {
        Ok(rt) => rt,
        Err(e) => {
            eprintln!("iwork-bridge-claude: 起不来异步运行时：{e}");
            std::process::exit(2);
        }
    };
    runtime.block_on(run(parent));
}

#[cfg(unix)]
async fn run(parent: libc::pid_t) {
    let leader = become_group_leader();
    watch_hangup();
    let sub = std::env::args().nth(1);
    let result = match sub.as_deref() {
        None | Some("acp") => match iwork_bridge_claude::acp::BridgeConfig::from_process() {
            Ok(config) => iwork_bridge_claude::acp::serve_stdio(config, parent).await,
            Err(e) => Err(e),
        },
        Some(other) => Err(format!("不认识的子命令：{other}")),
    };
    if let Err(message) = &result {
        tracing::error!(%message, "桥接退出");
        eprintln!("iwork-bridge-claude: {message}");
    }
    // 走到这里，要么 ACP 连接结束（后台没了、关了我们、写不出去），要么致命错误之后等满 TEARDOWN_WAIT 后台没来清；
    // CLI 都已经收拾过：最后对自己的进程组发 SIGKILL，连自己一起，
    // CLI 直接起的、还留在组里的进程也一起结束（5.4a 第 5 条第二种）。不是组长时不发，免得杀到别人的组
    if leader {
        // SAFETY: killpg 只读两个整数参数；组号是自己的进程号
        unsafe { libc::killpg(libc::getpid(), libc::SIGKILL) };
    }
    if result.is_err() {
        std::process::exit(2);
    }
}

/// 任何线程、任何任务 panic 都让整个桥接立刻结束（先照默认的样子把 panic 信息写到 stderr）。会话任务是单独起的
/// 任务，默认它 panic 只结束它自己：手里的 CLI 被丢掉（kill_on_drop 杀掉 CLI，父链断了，CLI 另立会话的命令就清不到），
/// 收摊的状态也永远到不了，`TEARDOWN_WAIT` 兜底不会触发。abort 之后走「桥接崩了」那条路：后台看到输出到头就清树，
/// 这时 CLI 还活着，范围找得全（5.4a 第 5 条，审查 S6-5b 停放期第 12 条）。
#[cfg(unix)]
fn abort_on_panic() {
    let report = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        report(info);
        eprintln!("iwork-bridge-claude: panic，桥接立刻结束（abort）");
        std::process::abort();
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
/// SIGHUP、SIGCONT，桥接要活下来，接着从 stdin 读到结束、走自己收尾那条路（5.4a 第 5 条）。
#[cfg(unix)]
fn watch_hangup() {
    use tokio::signal::unix::{SignalKind, signal};
    match signal(SignalKind::hangup()) {
        Ok(mut hup) => {
            tokio::spawn(async move {
                while hup.recv().await.is_some() {
                    tracing::warn!("收到 SIGHUP：后台可能没了，等 stdin 读到结束后自己收尾");
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
        .with_writer(std::io::stderr)
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
