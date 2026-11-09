//! 不落盘的流式增量的出口（技术设计 3.5）：模型输出的文字片段不逐条写库，直接推给订阅方，
//! 客户端按 `(message_id, offset)` 去重。实现在后台服务组合时注入（通信层的订阅推送）。

use iwork_domain::id::MessageId;

/// 一段流式正文。`offset` 是 `text` 在整条消息正文里的起始位置（UTF-8 字节）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MessageDelta {
    pub message_id: MessageId,
    pub offset: u64,
    pub text: String,
}

/// 增量推送。
///
/// 同步且不阻塞：推送方自己做背压（合并相邻增量、标记落后，技术设计 3.5），不让回合等它。
/// 增量丢了也能恢复：消息结束时写入完整正文，期间有检查点。
pub trait LivePublisher: Send + Sync {
    fn message_delta(&self, delta: MessageDelta);
}
