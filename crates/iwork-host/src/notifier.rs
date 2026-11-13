//! 系统通知的实现（T10a 方案 2.10；任务卡范围 4）。
//!
//! [`LogNotifier`]：只记一行日志。Linux 上就是它（方案定 Linux 只记日志）；macOS 的系统通知接口（申请权限、点击
//! 回调、把桌面客户端叫到前台）要加新的外部依赖，等产品负责人点头后另做（supervisor 2026-11-14），在那之前 macOS
//! 也用它。日志只记种类与是哪一项，不记正文：正文里可能有命令原文。

use iwork_ports::notify::{Notifier, NotifyError, SystemNotification};

#[derive(Debug, Clone, Copy, Default)]
pub struct LogNotifier;

impl Notifier for LogNotifier {
    async fn notify(&self, n: SystemNotification) -> Result<(), NotifyError> {
        let item = n.open.map(|o| o.item.key()).unwrap_or_default();
        tracing::info!(kind = %n.title, item = %item, "系统通知（本平台只记日志）");
        Ok(())
    }
}
