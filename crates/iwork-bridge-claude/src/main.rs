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
    let sub = std::env::args().nth(1);
    let result = match sub.as_deref() {
        None | Some("acp") => match iwork_bridge_claude::acp::BridgeConfig::from_process() {
            Ok(config) => iwork_bridge_claude::acp::serve_stdio(config).await,
            Err(e) => Err(e),
        },
        Some(other) => Err(format!("不认识的子命令：{other}")),
    };
    if let Err(message) = result {
        tracing::error!(%message, "桥接退出");
        eprintln!("iwork-bridge-claude: {message}");
        std::process::exit(2);
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
