//! 拿一条握手完成的连接（T09 方案 2.3）。[`DirectDialer`] 只连一个端点；T04 的 `Launcher` 连不上就拉起后台。
//! 桌面客户端用 [`AppDialer`]：发布构建里是 `Launcher`，`dev` 构建里是 `DirectDialer`（T04p 方案 4.7）。

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

/// [`AppDialer`] 要的两样会失败的东西：取自己的身份、建真正的拨号器。测试换成可计数的假实现。
pub trait DialerParts: Send + 'static {
    type Inner: Dialer;
    /// 自己的身份与信任快照（真实现是 `SelfIdentity::detect`）。
    fn identity(&mut self) -> Result<SelfIdentity, String>;
    /// 用取到的身份建真正的拨号器。
    fn build(&mut self, own: &SelfIdentity) -> Result<Self::Inner, String>;
}

/// 桌面客户端用的拨号器（T04p 方案 4.7）。「建拨号器」这件会失败的事放进 [`Dialer::dial`] 里：客户端点「重试」只是
/// 再调一次手里这个拨号器的 `dial()`，不会回到壳里重建它，所以环境修好之后能不能连上，全看这里会不会按当前的环境再试。
///
/// 两样东西各建一次、建成就不再动：身份取成功一次就固定（信任根在启动时固定，运行中不扩大，T03 方案 2.4），真正的
/// 拨号器建成就留着；之后断线重拨、手动重试都用它们。建不成时这一次回 `Environment(原因)`，下一次 `dial()` 再试。
pub struct AppDialer<P: DialerParts> {
    parts: P,
    own: Option<SelfIdentity>,
    inner: Option<P::Inner>,
}

impl<P: DialerParts> AppDialer<P> {
    pub fn new(parts: P) -> Self {
        Self {
            parts,
            own: None,
            inner: None,
        }
    }
}

impl<P: DialerParts> Dialer for AppDialer<P> {
    async fn dial(&mut self) -> Result<(Client, Incoming), DialError> {
        if self.inner.is_none() {
            let own = match self.own.take() {
                Some(own) => own,
                None => self.parts.identity().map_err(DialError::Environment)?,
            };
            let built = self.parts.build(&own);
            self.own = Some(own);
            self.inner = Some(built.map_err(DialError::Environment)?);
        }
        self.inner.as_mut().expect("上面刚建好").dial().await
    }
}

/// 真实的两样：取身份用 `SelfIdentity::detect`；拨号器在发布构建是 `Launcher`（连不上就拉起包里的后台），在 `dev`
/// 构建是 `DirectDialer`（只连、不拉起：设了 `IWORK_DEV_ENDPOINT` 连那个端点并走开发信任，没设连当前用户的默认端点）。
/// 开发构建一律不拉起后台：拉起的会是本机最后编的调试版后台，可能带 `--dev-trust`、用开发者真实的数据目录、走系统的
/// `launchctl`（supervisor 2026-11-13 定，和方案 4.7 原文「dev 下没设变量也用 Launcher」不同）。
pub struct DesktopParts {
    client_version: String,
}

impl DesktopParts {
    pub fn new(client_version: &str) -> Self {
        Self {
            client_version: client_version.to_owned(),
        }
    }
}

/// 发布构建的真正拨号器。
#[cfg(not(feature = "dev"))]
pub type DesktopDialer =
    crate::launcher::Launcher<crate::launcher::IpcConnect, crate::launcher::DetachedSpawn>;
/// `dev` 构建的真正拨号器：只连，不拉起。
#[cfg(feature = "dev")]
pub type DesktopDialer = DirectDialer;

impl DialerParts for DesktopParts {
    type Inner = DesktopDialer;

    fn identity(&mut self) -> Result<SelfIdentity, String> {
        SelfIdentity::detect().map_err(|e| format!("算不出客户端自己的身份：{e}"))
    }

    #[cfg(not(feature = "dev"))]
    fn build(&mut self, own: &SelfIdentity) -> Result<DesktopDialer, String> {
        crate::launcher::local(own, crate::launcher::desktop_config(&self.client_version)).map_err(
            |e| match e {
                crate::launcher::LaunchError::Environment(m) => m,
                other => format!("{other:?}"),
            },
        )
    }

    #[cfg(feature = "dev")]
    fn build(&mut self, own: &SelfIdentity) -> Result<DesktopDialer, String> {
        if let Some(path) = std::env::var_os("IWORK_DEV_ENDPOINT") {
            let ep = Endpoint::Unix(path.into());
            return Ok(DirectDialer::new(ep, own.clone(), &self.client_version).dev_trust());
        }
        let ep = iwork_ipc::LocalEndpoint::for_current_user().map_err(|e| e.to_string())?;
        Ok(DirectDialer::new(
            ep.endpoint().clone(),
            own.clone(),
            &self.client_version,
        ))
    }
}
