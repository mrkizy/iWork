//! Claude Code 桥接：ACP agent 端 ↔ Claude Code CLI（`-p` + stream-json + hook）（T06 方案 2.2）。
//!
//! 子命令：`acp`（默认，stdin/stdout 说 ACP）。`probe`、`login`、`verify`、`forget` 随 S8、S7、S6b 加。
//!
//! 日志只写 stderr（stdout 是 ACP 的通道），后台按运行时的 stderr 收（T05：不直接进后台日志，由它过滤）。
//! 级别默认 info，环境变量 [`ENV_LOG`] 可以改成 error / warn / info / debug / trace。

/// 日志级别的环境变量。
const ENV_LOG: &str = "IWORK_BRIDGE_LOG";

#[cfg(unix)]
// 一个 CLI、一条 ACP 连接、少量并发的 hook 连接，不需要线程池（方案第 14 节）
#[tokio::main(flavor = "current_thread")]
async fn main() {
    init_logging();
    let leader = become_group_leader();
    watch_hangup();
    let sub = std::env::args().nth(1);
    let result = match sub.as_deref() {
        None | Some("acp") => match iwork_bridge_claude::acp::BridgeConfig::from_process() {
            Ok(config) => iwork_bridge_claude::acp::serve_stdio(config).await,
            Err(e) => Err(e),
        },
        Some(other) => Err(format!("不认识的子命令：{other}")),
    };
    if let Err(message) = &result {
        tracing::error!(%message, "桥接退出");
        eprintln!("iwork-bridge-claude: {message}");
    }
    // ACP 连接结束（后台没了或关了我们），CLI 已经收拾过：最后对自己的进程组发 SIGKILL，连自己一起，
    // CLI 直接起的、还留在组里的进程也一起结束（5.4a 第 5 条第二种）。不是组长时不发，免得杀到别人的组
    tracing::debug!("DEBUG-TEMP main: serve returned, killpg");
    if leader {
        // SAFETY: killpg 只读两个整数参数；组号是自己的进程号
        unsafe { libc::killpg(libc::getpid(), libc::SIGKILL) };
    }
    if result.is_err() {
        std::process::exit(2);
    }
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
    // DEBUG-TEMP: 文件日志
    if let Some(dir) = std::env::var_os("IWORK_BRIDGE_DEBUG_DIR") {
        let path = std::path::Path::new(&dir).join(format!("{}.log", std::process::id()));
        let f = std::fs::File::create(path).unwrap();
        tracing_subscriber::fmt()
            .with_writer(std::sync::Mutex::new(f))
            .with_ansi(false)
            .with_max_level(tracing::Level::DEBUG)
            .init();
        std::panic::set_hook(Box::new(|info| {
            tracing::error!(%info, "DEBUG-TEMP panic");
        }));
        return;
    }
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
