//! 回合命令的外壳（T10a 方案 A3、A5、2.6）：取数 → `iwork_app::turn_command` → 一笔提交 → 回执。
//! 落盘之后的事（派发、发取消、续接）都归回合宿主，这里不做。

use iwork_app::refusal::Refusal;
use iwork_app::turn_command::{
    SendInput, plan_abandon_turn, plan_cancel_all, plan_cancel_turn, plan_resume_turn,
    plan_send_message,
};
use iwork_core::approval::engine::Scope;
use iwork_domain::approval::VoidReason;
use iwork_domain::conversation::Conversation;
use iwork_domain::event::DomainEvent;
use iwork_domain::id::{CommandId, ConversationId, MessageId, TurnId};
use iwork_domain::turn::Turn;
use iwork_ports::read_model::{MessagePage, ReadModelPort};
use iwork_ports::store::{CommitReceipt, CommitRequest, StoragePort};
use iwork_proto::pb;

use super::dispatch::{CommandError, Dispatcher, user, with_replan};
use super::engine::EngineCommands;

pub(super) fn parse<T: std::str::FromStr<Err = iwork_domain::id::IdParseError>>(
    text: &str,
) -> Result<T, CommandError> {
    text.parse::<T>()
        .map_err(|e| CommandError::Refused(iwork_domain::DomainError::from(e).into()))
}

/// 这一笔提交：客户端那条命令的 `command_id`、用户、这些事件。
pub(super) fn request(id: CommandId, events: Vec<DomainEvent>) -> CommitRequest {
    CommitRequest {
        command_id: id,
        actor: user(),
        events,
        outbox: Vec::new(),
    }
}

impl<St, Rd, E> Dispatcher<St, Rd, E>
where
    St: StoragePort + 'static,
    Rd: ReadModelPort + 'static,
    E: EngineCommands,
{
    /// 一笔直接提交（没有牵动审批或卡片）。事件为空也提交：受理了就一定有这一笔，回执照样查得到（A1）。
    pub(super) async fn commit(
        &self,
        id: CommandId,
        events: Vec<DomainEvent>,
    ) -> Result<CommitReceipt, CommandError> {
        #[cfg(any(test, feature = "test-hooks"))]
        self.before_commit(id).await;
        Ok(self.ports.store.commit(request(id, events)).await?)
    }

    /// 宿主已关（完全退出、封口）时不再收会起新活的命令（A6）。
    fn refuse_if_closing(&self) -> Result<(), CommandError> {
        if self.ports.host.is_closed() {
            return Err(Refusal::Closing.into());
        }
        Ok(())
    }

    pub(super) async fn conversation_of(
        &self,
        c: ConversationId,
    ) -> Result<Conversation, CommandError> {
        // 只要对话本身：一条消息都不取
        let page = MessagePage {
            before: None,
            limit: 0,
        };
        Ok(self.ports.read.conversation(c, page).await?.conversation)
    }

    async fn turn_of(&self, t: TurnId) -> Result<(Turn, Conversation), CommandError> {
        self.ports
            .read
            .turn_with_conversation(t)
            .await?
            .ok_or_else(|| {
                Refusal::NotFound {
                    entity: "turn",
                    id: t.to_string(),
                }
                .into()
            })
    }

    pub(super) async fn send_message(
        &self,
        id: CommandId,
        c: pb::SendMessage,
    ) -> Result<CommitReceipt, CommandError> {
        self.refuse_if_closing()?;
        let conversation: ConversationId = parse(&c.conversation_id)?;
        with_replan(|| async {
            let inputs = self.ports.read.snapshot_inputs(conversation).await?;
            let events = plan_send_message(SendInput {
                conversation: &inputs.conversation,
                assistants: &inputs.assistants,
                projects: &inputs.projects,
                text: c.text.clone(),
                message_id: MessageId::from_raw(self.ports.ids.next()),
                turn_id: TurnId::from_raw(self.ports.ids.next()),
                now: self.ports.clock.now(),
            })?;
            self.commit(id, events).await
        })
        .await
    }

    pub(super) async fn cancel_turn(
        &self,
        id: CommandId,
        c: pb::CancelTurn,
    ) -> Result<CommitReceipt, CommandError> {
        let turn: TurnId = parse(&c.turn_id)?;
        with_replan(|| async {
            let (t, conversation) = self.turn_of(turn).await?;
            let events = plan_cancel_turn(&t, &conversation)?;
            self.commit(id, events).await
        })
        .await
    }

    pub(super) async fn cancel_all(
        &self,
        id: CommandId,
        c: pb::CancelAll,
    ) -> Result<CommitReceipt, CommandError> {
        let conversation: ConversationId = parse(&c.conversation_id)?;
        with_replan(|| async {
            let conv = self.conversation_of(conversation).await?;
            let work = self.ports.read.turn_work().await?;
            let open: Vec<Turn> = work
                .conversations
                .into_iter()
                .filter(|w| w.conversation.id == conversation)
                .flat_map(|w| w.open_turns.into_iter().map(|o| o.turn))
                .collect();
            let events = plan_cancel_all(&conv, &open)?;
            self.commit(id, events).await
        })
        .await
    }

    pub(super) async fn resume_turn(
        &self,
        id: CommandId,
        c: pb::ResumeTurn,
    ) -> Result<CommitReceipt, CommandError> {
        self.refuse_if_closing()?;
        let turn: TurnId = parse(&c.turn_id)?;
        with_replan(|| async {
            let (t, conversation) = self.turn_of(turn).await?;
            let events = plan_resume_turn(&t, &conversation)?;
            self.commit(id, events).await
        })
        .await
    }

    /// `AbandonTurn` 经引擎：名下的待审批与放行在同一笔里作废（`Scope::VoidTurn`）。
    pub(super) async fn abandon_turn(
        &self,
        id: CommandId,
        c: pb::AbandonTurn,
    ) -> Result<CommitReceipt, CommandError> {
        let turn: TurnId = parse(&c.turn_id)?;
        with_replan(|| async {
            let (t, conversation) = self.turn_of(turn).await?;
            let recorded = self.ports.read.recorded_usage(t.id).await?;
            let events = plan_abandon_turn(&t, &conversation, recorded)?;
            let scope = Scope::VoidTurn {
                conversation: conversation.id,
                turn: t.id,
                reason: VoidReason::TurnAbandonedAfterInterrupt,
            };
            Ok(self
                .ports
                .engine
                .commit_with(scope, request(id, events))
                .await?
                .receipt)
        })
        .await
    }
}
