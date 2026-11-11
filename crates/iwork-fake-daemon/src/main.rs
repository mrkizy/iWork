//! `iwork-fake-daemon --endpoint <sock> [--scenario <名字>] [--auto-reply]`：起假后台、跑一个场景、继续服务到被杀。
//! `--list` 列出场景。只在开发期有意义（`pnpm -C client dev:fake`、录像工具）。

use std::process::ExitCode;

const USAGE: &str =
    "用法：iwork-fake-daemon --endpoint <socket 路径> [--scenario <名字>] [--auto-reply] | --list";

#[cfg(unix)]
#[tokio::main]
async fn main() -> ExitCode {
    use iwork_fake_daemon::{FakeDaemon, scenario};

    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.iter().any(|a| a == "--list") {
        for s in scenario::REGISTRY {
            println!("{}\t{}", s.name, s.about);
        }
        return ExitCode::SUCCESS;
    }
    let value = |flag: &str| {
        args.iter()
            .position(|a| a == flag)
            .and_then(|i| args.get(i + 1).cloned())
    };
    let Some(endpoint) = value("--endpoint") else {
        eprintln!("{USAGE}");
        return ExitCode::from(2);
    };
    let daemon = match FakeDaemon::start(endpoint.clone().into()).await {
        Ok(d) => d,
        Err(e) => {
            eprintln!("假后台在 {endpoint} 上起不来：{e}");
            return ExitCode::FAILURE;
        }
    };
    daemon.set_auto_reply(args.iter().any(|a| a == "--auto-reply"));
    eprintln!("iwork-fake-daemon：在 {endpoint} 上监听（DevTrust）");
    if let Some(name) = value("--scenario") {
        if let Err(e) = daemon.run_scenario(&name).await {
            eprintln!("{e}");
            return ExitCode::from(2);
        }
        eprintln!("场景 {name} 跑完，head = {}", daemon.head());
    }
    std::future::pending::<()>().await;
    ExitCode::SUCCESS
}

#[cfg(not(unix))]
fn main() -> ExitCode {
    eprintln!("iwork-fake-daemon 目前只在 Unix 上可用。{USAGE}");
    ExitCode::FAILURE
}
