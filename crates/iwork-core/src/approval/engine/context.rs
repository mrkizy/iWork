//! 审查输入只读同一条私聊项目线的消息；在线订阅与恢复共用。
use super::*;
use iwork_domain::id::MessageId;

#[derive(Default, Clone)]
pub(super) struct ReviewContext {
    pub task_goal: String,
    pub recent_context: String,
}

#[derive(Default)]
pub(super) struct ReviewMessages {
    pub contexts: BTreeMap<Option<ProjectId>, ReviewContext>,
    assistant_messages: BTreeMap<MessageId, Option<ProjectId>>,
}

fn bounded_tail(text: &str) -> String {
    text.chars()
        .rev()
        .take(review::CONTEXT_LIMIT)
        .collect::<Vec<_>>()
        .into_iter()
        .rev()
        .collect()
}

impl ReviewMessages {
    pub fn observe(&mut self, event: &DomainEvent) {
        match event {
            DomainEvent::MessageAppended(e) => {
                let message = &e.message;
                // core 不读取制品正文；T10 接制品消息时须供给有界摘要。
                let MessageBody::Inline(text) = &message.body else {
                    return;
                };
                let context = self.contexts.entry(message.context_project_id).or_default();
                match &message.author {
                    Author::User => {
                        context.task_goal = text.clone();
                    }
                    Author::Assistant { .. } => {
                        if !message.status.is_terminal() {
                            self.assistant_messages
                                .insert(message.id, message.context_project_id);
                        }
                        if !text.is_empty() {
                            context.recent_context = bounded_tail(text);
                        }
                    }
                    Author::System => {}
                }
            }
            DomainEvent::MessageCompleted(e) => {
                if let Some(project) = self.assistant_messages.remove(&e.message_id)
                    && let MessageBody::Inline(text) = &e.body
                {
                    self.contexts.entry(project).or_default().recent_context = bounded_tail(text);
                }
            }
            _ => {}
        }
    }
}
