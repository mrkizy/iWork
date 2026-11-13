//! 「用户消息 → 输入块」：新回合交给运行时的请求，和续接回合在摘要新开时再给一次的请求，都经这一个函数转
//! （T06 方案 5.2a、S2b 审查 1/F-3：不另写一条转换路）。

use iwork_domain::message::{Message, MessageBody};
use iwork_ports::agent::ContentBlock;

/// 转不出来的原因（给人看，写进回合失败或 `ResumeRequest::Unavailable`）。
pub type Unconvertible = String;

/// 用户消息转成发给运行时的输入块。正文内联的直接给；存成制品的长正文、附件归 T10d，读取还没接上，
/// 照实说转不出来，不发残缺的请求。
pub fn message_blocks(m: &Message) -> Result<Vec<ContentBlock>, Unconvertible> {
    if !m.attachments.is_empty() {
        return Err(format!(
            "消息 {} 带附件，附件的读取还没有接上（T10d）",
            m.id
        ));
    }
    match &m.body {
        MessageBody::Inline(text) => Ok(vec![ContentBlock::Text(text.clone())]),
        MessageBody::Artifact(_) => Err(format!(
            "消息 {} 的正文存成了制品，制品的读取还没有接上（T10d）",
            m.id
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use iwork_domain::common::Timestamp;
    use iwork_domain::id::{AttachmentId, ConversationId, MessageId, RawUlid};
    use iwork_domain::message::{Author, MessageStatus};

    fn message(body: MessageBody, attachments: Vec<AttachmentId>) -> Message {
        Message {
            id: MessageId::from_raw(RawUlid::from_u128(1)),
            conversation_id: ConversationId::from_raw(RawUlid::from_u128(2)),
            author: Author::User,
            context_project_id: None,
            status: MessageStatus::Sent,
            body,
            attachments,
            created_at: Timestamp(0),
            turn_id: None,
        }
    }

    #[test]
    fn inline_text_is_one_text_block_and_attachments_refuse() {
        assert_eq!(
            message_blocks(&message(MessageBody::Inline("写个文件".into()), vec![])),
            Ok(vec![ContentBlock::Text("写个文件".into())])
        );
        let with_file = message(
            MessageBody::Inline("看这个".into()),
            vec![AttachmentId::from_raw(RawUlid::from_u128(3))],
        );
        assert!(message_blocks(&with_file).unwrap_err().contains("附件"));
    }
}
