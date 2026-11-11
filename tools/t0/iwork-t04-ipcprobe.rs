//! iWork T04 N5 的握手探针（方案 8 N5 的 A1、C）。由 iwork-t04-bundle-probe.sh / iwork-t04-linux-bundle.sh
//! 在临时目录里建一个独立 crate 编译（依赖 crates/iwork-ipc，**不开 dev feature**，所以只有发布构建的
//! RequireSigner 判定）。放进包里冒充 iwork-daemon / iwork-client 两个角色：
//!
//!   ipcprobe serve <sock> [秒]   SelfIdentity::detect（读清单、打 manifest_loaded 审计）→ 玩具后台 → 监听若干秒
//!   ipcprobe hello <sock>        SelfIdentity::detect → 连接 → 打印 welcome 或连接错误
//!   ipcprobe self                打印 SelfIdentity（签名者摘要、安装目录、清单是否读到）
//!   ipcprobe runid [包路径]      用 iwork-ipc 的 running_image_id / file_image_ids（/ verify_bundle）打印身份
//!   ipcprobe selfwatch [秒]      每秒重新 SelfIdentity::detect 一次，打印完整的映像身份（换文件后看它变不变）
//!
//! 审计日志（iwork.ipc.audit）用 tracing-subscriber 打到 stderr。
use std::process::ExitCode;
use std::time::Duration;

use iwork_ipc::LocalListener;
use iwork_ipc::client::{Client, ClientConfig};
use iwork_ipc::peer::SelfIdentity;
use iwork_ipc::server::{
    ChunkRefused, ConnectionContext, EventSource, ReplayPage, RequestHandler, Server,
    ServerConfig, SourceError,
};
use iwork_ipc::transport::Endpoint;
use iwork_ipc::LocalConnector;
use iwork_proto::pb;
use iwork_proto::routing::StreamKey;

struct Empty;
impl EventSource for Empty {
    async fn head_seq(&self) -> Result<u64, SourceError> {
        Ok(0)
    }
    fn scope_streams(&self, _: &pb::SubscriptionScope) -> Vec<StreamKey> {
        Vec::new()
    }
    async fn replay(&self, _: &[StreamKey], _: u64, to: u64, _: usize) -> Result<ReplayPage, SourceError> {
        Ok(ReplayPage { events: Vec::new(), next_from: to })
    }
    async fn snapshot(&self, _: &[pb::SubscriptionScope]) -> Result<(pb::Snapshot, u64), SourceError> {
        Ok((pb::Snapshot { results: Vec::new() }, 0))
    }
}

struct Handler;
impl RequestHandler for Handler {
    async fn handle(&self, _: &ConnectionContext, req: pb::Request) -> pb::Response {
        pb::Response {
            request_id: req.request_id,
            body: Some(pb::response::Body::Result(pb::QueryResult { seq: 0, kind: None })),
        }
    }
    async fn chunk(&self, _: &ConnectionContext, _: pb::Chunk) -> Result<(), ChunkRefused> {
        Ok(())
    }
    async fn connection_closed(&self, _: &ConnectionContext) {}
}

fn full(s: &iwork_ipc::peer::SignerIdentity) -> String {
    match s {
        iwork_ipc::peer::SignerIdentity::Image(id) => id.to_string(),
        other => other.summary().to_string(),
    }
}

#[tokio::main(flavor = "current_thread")]
async fn main() -> ExitCode {
    tracing_subscriber::fmt().with_writer(std::io::stderr).with_ansi(false).init();
    let args: Vec<String> = std::env::args().skip(1).collect();
    let own = match SelfIdentity::detect() {
        Ok(own) => own,
        Err(e) => {
            println!("self_error={e}");
            return ExitCode::from(10);
        }
    };
    println!(
        "self signer={} exe_dir={} manifest={}",
        own.signer.summary(),
        own.exe_dir.display(),
        own.manifest.as_ref().map_or("none".into(), |m| format!("app_version={} entries={}", m.app_version, m.entries()))
    );
    match args.iter().map(String::as_str).collect::<Vec<_>>().as_slice() {
        ["self"] => {
            println!("image={}", full(&own.signer));
            ExitCode::SUCCESS
        }
        ["runid", rest @ ..] => {
            use iwork_ipc::peer::{Want, file_image_ids, running_image_id};
            let running = match running_image_id() {
                Ok(id) => id,
                Err(e) => {
                    println!("running_error={e}");
                    return ExitCode::from(10);
                }
            };
            let exe = std::env::current_exe().unwrap();
            let file: Vec<String> = file_image_ids(&exe, Want::SameKindAs(&running))
                .map(|v| v.iter().map(ToString::to_string).collect())
                .unwrap_or_else(|e| vec![format!("error {e}")]);
            println!("running={running} file={} equal={}", file.join(","), file == [running.to_string()]);
            #[cfg(target_os = "macos")]
            if let Some(app) = rest.first() {
                println!("verify_bundle={:?}", iwork_ipc::peer::verify_bundle(std::path::Path::new(app)).map_err(|e| e.to_string()));
            }
            #[cfg(not(target_os = "macos"))]
            let _ = rest;
            ExitCode::SUCCESS
        }
        ["selfwatch", rest @ ..] => {
            println!("image={}", full(&own.signer));
            let n: u64 = rest.first().and_then(|s| s.parse().ok()).unwrap_or(5);
            for i in 0..n {
                tokio::time::sleep(Duration::from_secs(1)).await;
                match SelfIdentity::detect() {
                    Ok(again) => println!("tick={i} image={}", full(&again.signer)),
                    Err(e) => println!("tick={i} error={e}"),
                }
            }
            ExitCode::SUCCESS
        }
        ["serve", sock, rest @ ..] => {
            let secs: u64 = rest.first().and_then(|s| s.parse().ok()).unwrap_or(10);
            let (server, _publisher) = Server::new(ServerConfig::new("t04-probe"), own, Handler, Empty);
            let listener = match LocalListener::bind(std::path::Path::new(sock)) {
                Ok(l) => l,
                Err(e) => {
                    println!("bind_error={e}");
                    return ExitCode::from(13);
                }
            };
            println!("listening");
            let token = tokio_util::sync::CancellationToken::new();
            let stop = token.clone();
            tokio::spawn(async move {
                tokio::time::sleep(Duration::from_secs(secs)).await;
                stop.cancel();
            });
            server.serve(listener, token).await;
            ExitCode::SUCCESS
        }
        ["hello", sock] => {
            let cfg = ClientConfig::new(pb::ClientKind::Desktop, env!("CARGO_PKG_VERSION"));
            match Client::connect(&LocalConnector, &Endpoint::Unix((*sock).into()), &own, cfg).await {
                Ok((client, _)) => {
                    println!("handshake=ok welcome_server_version={}", client.welcome().server_version);
                    ExitCode::SUCCESS
                }
                Err(e) => {
                    println!("handshake=error {e}");
                    ExitCode::from(1)
                }
            }
        }
        _ => {
            eprintln!("用法：ipcprobe self | runid [包路径] | selfwatch [秒] | serve <sock> [秒] | hello <sock>");
            ExitCode::from(2)
        }
    }
}
