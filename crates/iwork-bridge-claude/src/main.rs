//! Claude Code 桥接：ACP agent 端 ↔ Claude Code CLI（`-p` + stream-json + hook）（T06 方案 2.2）。
//!
//! 子命令：`acp`（默认，stdin/stdout 说 ACP）。`probe`、`login`、`verify`、`forget` 随 S8、S7、S6b 加。

#[cfg(unix)]
#[tokio::main]
async fn main() {
    let sub = std::env::args().nth(1);
    let result = match sub.as_deref() {
        None | Some("acp") => match iwork_bridge_claude::acp::BridgeConfig::from_process() {
            Ok(config) => iwork_bridge_claude::acp::serve_stdio(config).await,
            Err(e) => Err(e),
        },
        Some(other) => Err(format!("不认识的子命令：{other}")),
    };
    if let Err(message) = result {
        eprintln!("iwork-bridge-claude: {message}");
        std::process::exit(2);
    }
}

/// M1 不支持 Windows（AGENTS.md）。
#[cfg(not(unix))]
fn main() {
    eprintln!("iwork-bridge-claude: 这个平台还没接入");
    std::process::exit(2);
}
