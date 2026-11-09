//! 运行时发现（技术设计 6.5）：找程序、读版本、读登录状态。实现落在适配层。

use std::future::Future;
use std::path::PathBuf;

use iwork_domain::runtime::{LoginInfo, RuntimeKind};

/// 探测结果。登录信息只有 [`LoginInfo`] 的四个字段。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RuntimeProbe {
    /// `None`：没找到程序
    pub cli_path: Option<PathBuf>,
    pub cli_version: Option<String>,
    /// `None`：没找到程序，或读不到登录状态
    pub login: Option<LoginInfo>,
}

pub trait RuntimeDiscovery: Send + Sync {
    fn discover(
        &self,
        kind: &RuntimeKind,
    ) -> impl Future<Output = Result<RuntimeProbe, DiscoveryError>> + Send;
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum DiscoveryError {
    #[error("探测运行时 {kind} 失败：{message}")]
    Probe { kind: String, message: String },
}

impl DiscoveryError {
    pub fn retryable(&self) -> bool {
        true
    }
}
