//! 拿一条握手完成的连接（T09 方案 2.3）。T04 合入前只有 [`DirectDialer`]（连默认端点，不拉起）；
//! 合入后给 T04 的 Launcher 实现 [`Dialer`]，别处不动。

use std::future::Future;

use iwork_ipc::LocalConnector;
use iwork_ipc::client::{Client, ClientConfig, ConnectError, Incoming};
use iwork_ipc::peer::{SelfIdentity, TrustPolicy};
use iwork_ipc::transport::Endpoint;
use iwork_proto::pb;

use crate::store::Diagnostics;

/// 与 T04 `LaunchError` 同形。
#[derive(Debug)]
pub enum DialError {
    Connect(ConnectError),
    NotReady(Diagnostics),
    Environment(String),
}

pub trait Dialer: Send + 'static {
    fn dial(&mut self) -> impl Future<Output = Result<(Client, Incoming), DialError>> + Send;
}

/// 直接连一个端点，不拉起后台。
pub struct DirectDialer {
    endpoint: Endpoint,
    own: SelfIdentity,
    cfg: ClientConfig,
}

impl DirectDialer {
    pub fn new(endpoint: Endpoint, own: SelfIdentity, client_version: &str) -> Self {
        Self {
            endpoint,
            own,
            cfg: ClientConfig::new(pb::ClientKind::Desktop, client_version),
        }
    }

    /// 开发期：跳过签名校验（同用户照查）。只在 `dev` feature 下存在（R8、check-no-dev-trust）。
    #[cfg(feature = "dev")]
    pub fn dev_trust(mut self) -> Self {
        self.cfg.trust = TrustPolicy::DevTrust;
        self
    }

    pub fn trust(&self) -> TrustPolicy {
        self.cfg.trust
    }
}

impl Dialer for DirectDialer {
    async fn dial(&mut self) -> Result<(Client, Incoming), DialError> {
        Client::connect(&LocalConnector, &self.endpoint, &self.own, self.cfg.clone())
            .await
            .map_err(DialError::Connect)
    }
}
