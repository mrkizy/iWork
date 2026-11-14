//! 回合命令各提交什么（T10a 方案 0a.3 A5 表下「回合命令各提交什么」）：纯函数，输入是受理函数读到的状态，
//! 输出是要提交的**一笔**事件，或一个点名的拒绝。
//!
//! 状态以读到的为准；读和提交之间别人先落了盘，由写线程的提交时校验（A3、A4）拒掉这一笔，外壳重读重算。
//! 落盘之后发不发取消、续不续接都不在这里：用户的「取消」「继续」落成回合意图，宿主从库里推出来（A5）。

use iwork_domain::assistant::Assistant;
use iwork_domain::common::{Timestamp, Usage};
use iwork_domain::conversation::{Conversation, ConversationKind};
use iwork_domain::event::{
    DomainEvent, MessageAppended, TurnFinished, TurnIntentRecorded, TurnQueued, TurnStateChanged,
};
use iwork_domain::id::{MessageId, TurnId};
use iwork_domain::message::{Author, Message, MessageBody, MessageStatus};
use iwork_domain::project::Project;
use iwork_domain::turn::{Turn, TurnIntentKind, TurnKind, TurnState};

use crate::refusal::Refusal;

/// `SendMessage` 受理要读的事实与这次生成的编号。
#[derive(Debug, Clone)]
pub struct SendInput<'a> {
    pub conversation: &'a Conversation,
    /// 全部助手（项目对话找内置的 iWork，私聊找它自己的助手）
    pub assistants: &'a [Assistant],
    /// 全部项目（取这条会话线所在项目的默认模型）
    pub projects: &'a [Project],
    pub text: String,
    pub message_id: MessageId,
    pub turn_id: TurnId,
    pub now: Timestamp,
}

/// `SendMessage`：用户消息与 `TurnQueued` 在同一笔里（A3；`summary_source` 靠这一点把这一轮的请求挡在
/// 摘要外，T06 方案 5.2a）。对话在删、已删 → 拒绝；回合记下助手版本、运行时、模型。
pub fn plan_send_message(input: SendInput<'_>) -> Result<Vec<DomainEvent>, Refusal> {
    let c = input.conversation;
    c.admit_command()?;
    if input.text.trim().is_empty() {
        return Err(Refusal::InvalidArgument {
            field: "SendMessage.text",
            reason: "消息是空的".into(),
        });
    }
    let (assistant, line_project) = match c.kind {
        // M1 的项目对话只和内置助手 iWork 说（多成员是 M2）
        ConversationKind::Project { project_id } => (
            input
                .assistants
                .iter()
                .find(|a| a.builtin)
                .ok_or_else(|| Refusal::NotFound {
                    entity: "assistant",
                    id: "内置助手 iWork".into(),
                })?,
            Some(project_id),
        ),
        ConversationKind::AssistantDm {
            assistant_id,
            context_project,
        } => (
            input
                .assistants
                .iter()
                .find(|a| a.id == assistant_id)
                .ok_or_else(|| Refusal::NotFound {
                    entity: "assistant",
                    id: assistant_id.to_string(),
                })?,
            context_project,
        ),
    };
    let config = &assistant.current.config;
    // 技术设计 9.6 第 1 条：对话的改选 > iWork 取项目默认（FR04）> 助手自己的默认
    let project_default = assistant
        .builtin
        .then(|| line_project.and_then(|p| input.projects.iter().find(|x| x.id == p)))
        .flatten()
        .and_then(|p| p.settings.default_model.clone());
    let model = c
        .model_override
        .clone()
        .or(project_default)
        .unwrap_or_else(|| config.model.clone());
    let message = Message {
        id: input.message_id,
        conversation_id: c.id,
        author: Author::User,
        context_project_id: line_project,
        status: MessageStatus::Sent,
        body: MessageBody::Inline(input.text),
        attachments: Vec::new(),
        created_at: input.now,
        turn_id: None,
    };
    let turn = Turn {
        id: input.turn_id,
        conversation_id: c.id,
        assistant_id: assistant.id,
        assistant_version: assistant.current.version,
        runtime_id: config.runtime_id,
        model,
        runtime_session_id: None,
        kind: TurnKind::Assistant,
        state: TurnState::Queued,
        started_at: input.now,
        ended_at: None,
    };
    Ok(vec![
        MessageAppended { message }.into(),
        TurnQueued { turn }.into(),
    ])
}

/// 排着的回合直接取消（0 档的「撤回」）：`Queued → Cancelled` 与 `TurnFinished`。
/// 宿主在删除、退出时取消排着的回合也用这一个（X2、E2 ③）。
pub fn withdraw_queued(turn: &Turn) -> Vec<DomainEvent> {
    // 排着的回合从没跑过：没有记下的用量
    finish(
        turn,
        TurnState::Queued,
        TurnState::Cancelled,
        Usage::default(),
    )
}

/// 一条状态变化加上结束事件。结束事件带回合的用量累计（用量口径 b：`recorded_usage` 是读模型里记下的那一份）；
/// 这里收的回合这一刻都没有编排在跑，所以没有改动摘要。
fn finish(turn: &Turn, from: TurnState, to: TurnState, recorded_usage: Usage) -> Vec<DomainEvent> {
    vec![
        TurnStateChanged {
            turn_id: turn.id,
            conversation_id: Some(turn.conversation_id),
            from,
            to,
            reason: None,
        }
        .into(),
        TurnFinished {
            turn_id: turn.id,
            conversation_id: Some(turn.conversation_id),
            state: to,
            usage: recorded_usage,
            celebrate: false,
            change_summary: None,
        }
        .into(),
    ]
}

fn intent(turn: &Turn, intent: TurnIntentKind) -> DomainEvent {
    TurnIntentRecorded {
        turn_id: turn.id,
        conversation_id: Some(turn.conversation_id),
        intent,
    }
    .into()
}

/// 一个回合在「取消」下要提交什么。`None` = 这个状态下取消不受理（已中断）。
fn cancel_events(turn: &Turn) -> Option<Vec<DomainEvent>> {
    match turn.state {
        TurnState::Queued => Some(withdraw_queued(turn)),
        TurnState::Running | TurnState::WaitingApproval => {
            Some(vec![intent(turn, TurnIntentKind::Cancel)])
        }
        // 要的结果已经有了，或正在发生：空的一笔
        TurnState::Cancelling | TurnState::Completed | TurnState::Cancelled | TurnState::Failed => {
            Some(Vec::new())
        }
        TurnState::Interrupted { .. } => None,
    }
}

/// `CancelTurn`。
pub fn plan_cancel_turn(
    turn: &Turn,
    conversation: &Conversation,
) -> Result<Vec<DomainEvent>, Refusal> {
    conversation.admit_command()?;
    cancel_events(turn).ok_or(Refusal::TurnNotCancellable { turn: turn.id })
}

/// `CancelAll`：这个对话里每个没到终态的回合各出一份，合成一笔（要么都落、要么都不落）。
/// `Queued`、`Running`、`WaitingApproval` 三种各出一份；`Interrupted`、`Cancelling` 的不动。
/// 管到哪些回合由 `open` 定：读完之后才受理的回合不在这一笔里。
pub fn plan_cancel_all(
    conversation: &Conversation,
    open: &[Turn],
) -> Result<Vec<DomainEvent>, Refusal> {
    conversation.admit_command()?;
    Ok(open
        .iter()
        .filter(|t| t.conversation_id == conversation.id)
        .filter_map(cancel_events)
        .flatten()
        .collect())
}

fn require_interrupted(turn: &Turn) -> Result<(), Refusal> {
    match turn.state {
        TurnState::Interrupted { .. } => Ok(()),
        state => Err(Refusal::TurnNotResumable {
            turn: turn.id,
            state,
        }),
    }
}

/// `ResumeTurn`：已中断的回合落一条继续意图。
pub fn plan_resume_turn(
    turn: &Turn,
    conversation: &Conversation,
) -> Result<Vec<DomainEvent>, Refusal> {
    conversation.admit_command()?;
    require_interrupted(turn)?;
    Ok(vec![intent(turn, TurnIntentKind::Resume)])
}

/// `AbandonTurn`：已中断 → 已取消。名下待审批与放行的作废由引擎在同一笔里附带（`Scope::VoidTurn`）。
/// `recorded_usage`：中断前记下的用量累计，结束事件原样带上。
pub fn plan_abandon_turn(
    turn: &Turn,
    conversation: &Conversation,
    recorded_usage: Usage,
) -> Result<Vec<DomainEvent>, Refusal> {
    conversation.admit_command()?;
    require_interrupted(turn)?;
    Ok(finish(
        turn,
        turn.state,
        TurnState::Cancelled,
        recorded_usage,
    ))
}

#[cfg(test)]
#[path = "turn_command_tests.rs"]
mod tests;
