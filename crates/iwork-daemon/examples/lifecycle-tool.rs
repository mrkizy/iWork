//! 生命周期命令的调试工具（只在 `dev` feature 下编译；演练脚本 `scripts/drill-*.sh` 用它）。
//!
//! ```text
//! cargo run -p iwork-daemon --features dev --example lifecycle-tool -- --run-dir <socket 目录> shutdown [--cancel-running]
//! cargo run -p iwork-daemon --features dev --example lifecycle-tool -- --run-dir <socket 目录> set-autostart on|off
//! ```
//!
//! 以开发工具身份（`DevTrust`）连后台，后台也要跑在 `--dev-trust` 下。响应以一行 JSON 打印；退出码 0 = 受理，
//! 1 = 后台回了错误，2 = 用法或连接问题。

use std::path::PathBuf;
use std::process::ExitCode;

use iwork_ipc::client::{Client, ClientConfig, ClientError};
use iwork_ipc::peer::{SelfIdentity, TrustPolicy};
use iwork_ipc::{LocalConnector, LocalEndpoint};
use iwork_proto::pb;

const USAGE: &str = "用法：lifecycle-tool --run-dir <socket 目录> (shutdown [--cancel-running] | set-autostart on|off)";

fn fail(msg: &str) -> ExitCode {
    eprintln!("lifecycle-tool：{msg}");
    ExitCode::from(2)
}

#[tokio::main(flavor = "current_thread")]
async fn main() -> ExitCode {
    let mut args: Vec<String> = std::env::args().skip(1).collect();
    let Some(i) = args.iter().position(|a| a == "--run-dir") else {
        return fail(USAGE);
    };
    args.remove(i);
    if i >= args.len() {
        return fail(USAGE);
    }
    let run_dir = PathBuf::from(args.remove(i));
    let kind = match args.iter().map(String::as_str).collect::<Vec<_>>()[..] {
        ["shutdown"] => pb::command::Kind::ShutdownDaemon(pb::ShutdownDaemon {
            cancel_running: false,
        }),
        ["shutdown", "--cancel-running"] => pb::command::Kind::ShutdownDaemon(pb::ShutdownDaemon {
            cancel_running: true,
        }),
        ["set-autostart", "on"] => {
            pb::command::Kind::SetAutostart(pb::SetAutostart { enabled: true })
        }
        ["set-autostart", "off"] => {
            pb::command::Kind::SetAutostart(pb::SetAutostart { enabled: false })
        }
        _ => return fail(USAGE),
    };
    let ep = match LocalEndpoint::in_run_dir(&run_dir) {
        Ok(ep) => ep,
        Err(e) => return fail(&e.to_string()),
    };
    let own = match SelfIdentity::detect() {
        Ok(o) => o,
        Err(e) => return fail(&format!("算不出本进程身份：{e}")),
    };
    let mut cfg = ClientConfig::new(pb::ClientKind::DevTool, "lifecycle-tool");
    cfg.trust = TrustPolicy::DevTrust;
    let (client, _incoming) = match Client::connect(&LocalConnector, ep.endpoint(), &own, cfg).await
    {
        Ok(c) => c,
        Err(e) => return fail(&format!("连不上：{e}")),
    };
    let r = client
        .request(pb::request::Body::Command(pb::Command {
            command_id: ulid::Ulid::generate().to_string(),
            kind: Some(kind),
        }))
        .await;
    match r {
        Ok(resp) => {
            println!(
                "{}",
                serde_json::json!({ "accepted": format!("{:?}", resp.body) })
            );
            ExitCode::SUCCESS
        }
        Err(ClientError::Server(e)) => {
            println!(
                "{}",
                serde_json::json!({
                    "error": e.code,
                    "message": e.message,
                    "details": e.details,
                })
            );
            ExitCode::from(1)
        }
        Err(e) => fail(&format!("请求失败：{e:?}")),
    }
}
