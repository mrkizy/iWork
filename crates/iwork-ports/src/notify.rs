//! 系统通知。

use std::future::Future;

use iwork_domain::id::ConversationId;

/// 一条系统通知；`open` 是点击后打开的对话。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SystemNotification {
    pub title: String,
    pub body: String,
    pub open: Option<ConversationId>,
}

pub trait Notifier: Send + Sync {
    fn notify(&self, n: SystemNotification)
    -> impl Future<Output = Result<(), NotifyError>> + Send;
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum NotifyError {
    #[error("系统通知不可用：{message}")]
    Unavailable { message: String },
}

impl NotifyError {
    /// 通知丢了不影响业务，不重试。
    pub fn retryable(&self) -> bool {
        false
    }
}
