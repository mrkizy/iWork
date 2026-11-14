//! session 各子模块单测共用的布景：一个不连后台的 `Session`（异步回包由用例直接注入）。

use super::*;

/// 永远拨不通：用例不走真连接，回包经 `on_internal` 注入。
pub(super) struct NoDial;

impl Dialer for NoDial {
    async fn dial(
        &mut self,
    ) -> Result<(iwork_ipc::client::Client, iwork_ipc::client::Incoming), crate::DialError> {
        std::future::pending().await
    }
}

pub(super) fn session() -> Session<NoDial> {
    let opts = crate::Options::default();
    let (effects, _) = tokio::sync::mpsc::unbounded_channel();
    Session::new(
        NoDial,
        opts.chunk_max,
        opts.timeline_cap,
        opts.clock,
        effects,
    )
}
