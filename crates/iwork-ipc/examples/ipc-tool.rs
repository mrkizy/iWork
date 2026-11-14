//! 调试客户端（方案 2.13；只在 `dev` feature 下编译，example 从不进任何发布产物）。
//!
//! ```text
//! cargo run -p iwork-ipc --features dev --example ipc-tool -- hello
//! cargo run -p iwork-ipc --features dev --example ipc-tool -- subscribe sidebar --from 0
//! cargo run -p iwork-ipc --features dev --example ipc-tool -- subscribe conversation <id> --from <seq>
//! cargo run -p iwork-ipc --features dev --example ipc-tool -- query list-projects
//! # 开发自测：起一个玩具后台（DevTrust、内存事件、每秒提交一条），再用上面的命令连它
//! cargo run -p iwork-ipc --features dev --example ipc-tool -- --endpoint /tmp/x/d.sock serve
//! ```
//!
//! 自己没有签名，用 `TrustPolicy::DevTrust`（只跳过签名，同用户照查）；所以后台也必须跑在 dev-trust 下才接受它。
//! 收到的每一帧以 JSON 一行打印。

use std::process::ExitCode;

use iwork_ipc::client::{Client, ClientConfig, IncomingItem};
use iwork_ipc::frame::Frame;
use iwork_ipc::peer::{SelfIdentity, TrustPolicy};
use iwork_ipc::transport::Endpoint;
use iwork_ipc::{LocalConnector, LocalEndpoint};
use iwork_proto::pb;

const USAGE: &str = "用法：ipc-tool [--endpoint <socket 路径>] (hello | subscribe sidebar [--from N] | subscribe conversation <id> [--from N] | query list-projects | serve)";

#[tokio::main(flavor = "current_thread")]
async fn main() -> ExitCode {
    let mut args: Vec<String> = std::env::args().skip(1).collect();
    let endpoint = match take_opt(&mut args, "--endpoint") {
        Some(p) => endpoint_from(&p),
        None => match LocalEndpoint::for_current_user() {
            Ok(ep) => ep.endpoint().clone(),
            Err(e) => return fail(&format!("算不出本机端点：{e}")),
        },
    };
    let own = match SelfIdentity::detect() {
        Ok(own) => own,
        Err(e) => return fail(&format!("算不出本进程身份：{e}")),
    };
    match args.first().map(String::as_str) {
        Some("serve") => serve(endpoint, own).await,
        Some(_) => client(endpoint, own, args).await,
        None => fail(USAGE),
    }
}

fn take_opt(args: &mut Vec<String>, name: &str) -> Option<String> {
    let i = args.iter().position(|a| a == name)?;
    args.remove(i);
    (i < args.len()).then(|| args.remove(i))
}

#[cfg(unix)]
fn endpoint_from(p: &str) -> Endpoint {
    Endpoint::Unix(p.into())
}

#[cfg(windows)]
fn endpoint_from(p: &str) -> Endpoint {
    Endpoint::Pipe(p.into())
}

fn fail(msg: &str) -> ExitCode {
    eprintln!("ipc-tool：{msg}");
    ExitCode::from(2)
}

async fn client(endpoint: Endpoint, own: SelfIdentity, mut args: Vec<String>) -> ExitCode {
    let from = take_opt(&mut args, "--from")
        .and_then(|v| v.parse().ok())
        .unwrap_or(0);
    let mut cfg = ClientConfig::new(pb::ClientKind::DevTool, env!("CARGO_PKG_VERSION"));
    cfg.trust = TrustPolicy::DevTrust;
    let (client, mut incoming) = match Client::connect(&LocalConnector, &endpoint, &own, cfg).await
    {
        Ok(c) => c,
        Err(e) => return fail(&format!("连接失败：{e}")),
    };
    println!("{}", Frame::Welcome(client.welcome().clone()));
    let words: Vec<&str> = args.iter().map(String::as_str).collect();
    let result = match words.as_slice() {
        ["hello"] => return ExitCode::SUCCESS,
        ["subscribe", "sidebar"] => {
            client
                .subscribe(
                    1,
                    scope(pb::subscription_scope::Kind::Sidebar(pb::SidebarScope {})),
                    from,
                )
                .await
        }
        ["subscribe", "conversation", id] => {
            let kind = pb::subscription_scope::Kind::Conversation(pb::ConversationScope {
                conversation_id: (*id).into(),
            });
            client.subscribe(1, scope(kind), from).await
        }
        ["query", "list-projects"] => {
            let q = pb::Query {
                kind: Some(pb::query::Kind::ListProjects(pb::ListProjects {})),
            };
            let r = client.view_query(q).await;
            if r.is_ok()
                && let Some(item) = incoming.next().await
            {
                print_item(item);
            }
            return if r.is_ok() {
                ExitCode::SUCCESS
            } else {
                fail(&format!("{r:?}"))
            };
        }
        _ => return fail(USAGE),
    };
    if let Err(e) = result {
        return fail(&format!("订阅失败：{e}"));
    }
    while let Some(item) = incoming.next().await {
        let closed = matches!(item, IncomingItem::Closed(_));
        print_item(item);
        if closed {
            break;
        }
    }
    ExitCode::SUCCESS
}

fn scope(kind: pb::subscription_scope::Kind) -> pb::SubscriptionScope {
    pb::SubscriptionScope { kind: Some(kind) }
}

fn print_item(item: IncomingItem) {
    let frame = match item {
        IncomingItem::Event(e) => Frame::Event(e),
        IncomingItem::ViewResult { request_id, result } => {
            Frame::Response(Box::new(pb::Response {
                request_id,
                body: Some(pb::response::Body::Result(*result)),
            }))
        }
        IncomingItem::Subscribed {
            request_id,
            subscribed,
        } => Frame::Response(Box::new(pb::Response {
            request_id,
            body: Some(pb::response::Body::Subscribed(subscribed)),
        })),
        IncomingItem::ServerError(e) => Frame::Response(Box::new(pb::Response {
            request_id: 0,
            body: Some(pb::response::Body::Error(e)),
        })),
        IncomingItem::Closed(reason) => {
            println!("{{\"closed\":\"{reason:?}\"}}");
            return;
        }
    };
    println!("{frame}");
}

/// 玩具后台：内存事件日志，每秒提交一条 `project:demo` 事件。只为开发自测握手与订阅。
#[cfg(unix)]
async fn serve(endpoint: Endpoint, own: SelfIdentity) -> ExitCode {
    use std::sync::{Arc, Mutex};

    use iwork_ipc::LocalListener;
    use iwork_ipc::server::{
        ChunkRefused, ConnectionContext, EventSource, Published, ReplayPage, RequestHandler,
        RoutedEvent, Server, ServerConfig, SourceError,
    };
    use iwork_proto::routing::StreamKey;

    type Log = Arc<Mutex<Vec<Arc<RoutedEvent>>>>;

    struct Toy(Log);
    impl EventSource for Toy {
        async fn head_seq(&self) -> Result<u64, SourceError> {
            Ok(self.0.lock().unwrap().len() as u64)
        }
        fn scope_streams(&self, scope: &pb::SubscriptionScope) -> Vec<StreamKey> {
            if scope.kind.is_some() {
                vec![StreamKey::all("project")]
            } else {
                Vec::new()
            }
        }
        async fn replay(
            &self,
            _: &[StreamKey],
            from: u64,
            to: u64,
            limit: usize,
        ) -> Result<ReplayPage, SourceError> {
            let log = self.0.lock().unwrap();
            let events: Vec<_> = log[from as usize..to as usize]
                .iter()
                .take(limit)
                .cloned()
                .collect();
            let next_from = events.last().map_or(to, |e| e.seq());
            Ok(ReplayPage { events, next_from })
        }
        async fn snapshot(
            &self,
            scopes: &[pb::SubscriptionScope],
        ) -> Result<(pb::Snapshot, u64), SourceError> {
            let s = self.0.lock().unwrap().len() as u64;
            let results = scopes
                .iter()
                .map(|_| pb::QueryResult { seq: s, kind: None })
                .collect();
            Ok((pb::Snapshot { results }, s))
        }
    }

    struct ToyHandler(Log);
    impl RequestHandler for ToyHandler {
        async fn handle(&self, _: &ConnectionContext, req: pb::Request) -> pb::Response {
            let seq = self.0.lock().unwrap().len() as u64;
            pb::Response {
                request_id: req.request_id,
                body: Some(pb::response::Body::Result(pb::QueryResult {
                    seq,
                    kind: None,
                })),
            }
        }
        async fn chunk(&self, _: &ConnectionContext, _: pb::Chunk) -> Result<(), ChunkRefused> {
            Ok(())
        }
        async fn connection_closed(&self, _: &ConnectionContext) {}
    }

    let Endpoint::Unix(path) = endpoint else {
        return fail("serve 只支持 Unix socket");
    };
    let log: Log = Arc::default();
    let mut cfg = ServerConfig::new("ipc-tool-serve");
    cfg.trust = TrustPolicy::DevTrust;
    let (server, publisher) = Server::new(cfg, own, ToyHandler(log.clone()), Toy(log.clone()));
    let listener = match LocalListener::bind(&path) {
        Ok(l) => l,
        Err(e) => return fail(&format!("绑定 {} 失败：{e}", path.display())),
    };
    eprintln!("ipc-tool serve：在 {} 上监听（DevTrust）", path.display());
    tokio::spawn(async move {
        loop {
            tokio::time::sleep(std::time::Duration::from_secs(1)).await;
            let e = {
                let mut log = log.lock().unwrap();
                let e = Arc::new(RoutedEvent::new(
                    pb::PersistedEvent {
                        seq: log.len() as u64 + 1,
                        actor: "demo".into(),
                        ..Default::default()
                    },
                    vec![StreamKey::one("project", "demo")],
                ));
                log.push(e.clone());
                e
            };
            publisher.publish(Published::Persisted(e));
        }
    });
    server
        .serve(listener, tokio_util::sync::CancellationToken::new())
        .await;
    ExitCode::SUCCESS
}

#[cfg(windows)]
async fn serve(_: Endpoint, _: SelfIdentity) -> ExitCode {
    fail("serve 目前只在 Unix 上可用")
}
