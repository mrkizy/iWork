//! 对话命令的外壳（T10a 方案 X1、2.6）。

use iwork_app::conversation_command::{
    plan_create_conversation, plan_delete_conversation, plan_mark_read, plan_set_conversation_model,
};
use iwork_app::convert::required_model;
use iwork_core::approval::engine::Scope;
use iwork_domain::id::{CommandId, ConversationId, ProjectId};
use iwork_ports::read_model::ReadModelPort;
use iwork_ports::store::{CommitReceipt, StoragePort};
use iwork_proto::pb;

use super::dispatch::{CommandError, Dispatcher, with_replan};
use super::engine::EngineCommands;
use super::turn::{parse, request};

impl<St, Rd, E> Dispatcher<St, Rd, E>
where
    St: StoragePort + 'static,
    Rd: ReadModelPort + 'static,
    E: EngineCommands,
{
    /// X1：先落删除意图，经引擎（同一笔里作废待审批与待执行卡片）。已在删 → 空的一笔。之后的收尾归宿主（X2–X4）。
    pub(super) async fn delete_conversation(
        &self,
        id: CommandId,
        c: pb::DeleteConversation,
    ) -> Result<CommitReceipt, CommandError> {
        let conversation: ConversationId = parse(&c.conversation_id)?;
        with_replan(|| async {
            let conv = self.conversation_of(conversation).await?;
            let events = plan_delete_conversation(&conv)?;
            if events.is_empty() {
                return self.commit(id, events).await;
            }
            Ok(self
                .ports
                .engine
                .commit_with(
                    Scope::ConversationDeleting(conversation),
                    request(id, events),
                )
                .await?
                .receipt)
        })
        .await
    }

    pub(super) async fn create_conversation(
        &self,
        id: CommandId,
        c: pb::CreateConversation,
    ) -> Result<CommitReceipt, CommandError> {
        let project: ProjectId = parse(&c.project_id)?;
        let projects = self.ports.read.projects().await?;
        let events = plan_create_conversation(
            projects
                .iter()
                .map(|p| &p.project)
                .find(|p| p.id == project),
            project,
            ConversationId::from_raw(self.ports.ids.next()),
            self.ports.clock.now(),
        )?;
        self.commit(id, events).await
    }

    pub(super) async fn mark_read(
        &self,
        id: CommandId,
        c: pb::MarkRead,
    ) -> Result<CommitReceipt, CommandError> {
        let conversation: ConversationId = parse(&c.conversation_id)?;
        let conv = self.conversation_of(conversation).await?;
        let head = self.ports.store.head_seq().await?;
        let events = plan_mark_read(&conv, c.up_to_seq, head)?;
        self.commit(id, events).await
    }

    pub(super) async fn set_conversation_model(
        &self,
        id: CommandId,
        c: pb::SetConversationModel,
    ) -> Result<CommitReceipt, CommandError> {
        let conversation: ConversationId = parse(&c.conversation_id)?;
        let model = required_model("SetConversationModel", "model", c.model.as_ref())?;
        let conv = self.conversation_of(conversation).await?;
        let events = plan_set_conversation_model(&conv, model)?;
        self.commit(id, events).await
    }
}
