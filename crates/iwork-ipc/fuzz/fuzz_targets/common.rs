//! fuzz 目标共用：计数 handler、空事件源、在内存传输上起一个服务端。

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::path::PathBuf;

use iwork_ipc::peer::{ImageId, Manifest, PeerIdentity, SelfIdentity, SignerIdentity, UserId};
use iwork_ipc::server::{
    ChunkRefused, ConnectionContext, EventSource, ReplayPage, RequestHandler, Server, ServerConfig, SourceError,
};
use iwork_ipc::transport::memory::{self, MemoryConnector};
use iwork_proto::pb;
use iwork_proto::routing::StreamKey;

#[derive(Clone, Default)]
pub struct Counting {
    pub handled: Arc<AtomicUsize>,
    pub empty_body_seen: Arc<AtomicUsize>,
}

impl RequestHandler for Counting {
    async fn handle(&self, _: &ConnectionContext, req: pb::Request) -> pb::Response {
        self.handled.fetch_add(1, Ordering::SeqCst);
        if req.body.is_none() {
            self.empty_body_seen.fetch_add(1, Ordering::SeqCst);
        }
        pb::Response { request_id: req.request_id, body: Some(pb::response::Body::Result(pb::QueryResult::default())) }
    }
    async fn chunk(&self, _: &ConnectionContext, _: pb::Chunk) -> Result<(), ChunkRefused> {
        Ok(())
    }
    async fn connection_closed(&self, _: &ConnectionContext) {}
}

pub struct Empty;

impl EventSource for Empty {
    async fn head_seq(&self) -> Result<u64, SourceError> {
        Ok(0)
    }
    fn scope_streams(&self, scope: &pb::SubscriptionScope) -> Vec<StreamKey> {
        if scope.kind.is_some() { vec![StreamKey::all("project")] } else { Vec::new() }
    }
    async fn replay(&self, _: &[StreamKey], _: u64, to: u64, _: usize) -> Result<ReplayPage, SourceError> {
        Ok(ReplayPage { events: Vec::new(), next_from: to })
    }
    async fn snapshot(&self, _: &[pb::SubscriptionScope]) -> Result<(pb::Snapshot, u64), SourceError> {
        Ok((pb::Snapshot::default(), 0))
    }
}

fn identity(img: u8) -> PeerIdentity {
    PeerIdentity { pid: 1, user: UserId::Uid(1), exe: Some(PathBuf::from("/i/x")), signer: SignerIdentity::Image(ImageId::Sha256([img; 32])) }
}

pub fn start(handler: Counting) -> (MemoryConnector, iwork_ipc::transport::Endpoint, tokio::task::JoinHandle<()>, tokio_util::sync::CancellationToken) {
    let own = SelfIdentity {
        user: UserId::Uid(1),
        signer: SignerIdentity::Image(ImageId::Sha256([1; 32])),
        exe_dir: PathBuf::from("/i"),
        manifest: Some(Manifest::parse(&format!(r#"{{"manifest_version":1,"app_version":"0","binaries":{{"x":["{}"]}}}}"#, ImageId::Sha256([2; 32]))).unwrap()),
    };
    let mut cfg = ServerConfig::new("fuzz");
    cfg.handshake_timeout = std::time::Duration::from_millis(50);
    let (server, _publisher) = Server::new(cfg, own, handler, Empty);
    let (listener, connector) = memory::endpoint("fuzz", identity(2), Arc::new(|_| identity(1)));
    let token = tokio_util::sync::CancellationToken::new();
    let task = tokio::spawn(server.serve(listener, token.clone()));
    (connector, iwork_ipc::transport::Endpoint::Memory("fuzz".into()), task, token)
}

pub fn runtime() -> tokio::runtime::Runtime {
    tokio::runtime::Builder::new_current_thread().enable_time().build().unwrap()
}
