//! 审批命令的外壳（T10a 方案 2.6）：`DecideApproval`、`SetApprovalMode` 由引擎自己提交，`command_id` 用客户端
//! 那条命令的（A2 的去重才管得到它们）。批准后要不要续接由宿主读引擎的折叠，这里不管。

use iwork_app::convert::{
    approval_mode_from_proto, mode_target_from_proto, remember_scope_from_proto,
};
use iwork_core::approval::engine::UserDecision;
use iwork_domain::approval_mode::RememberScope;
use iwork_domain::event::ApprovalModeTarget;
use iwork_domain::id::{ApprovalId, CommandId};
use iwork_ports::read_model::ReadModelPort;
use iwork_ports::store::{CommitReceipt, StoragePort};
use iwork_proto::pb;

use super::dispatch::{CommandError, Dispatcher, user};
use super::engine::EngineCommands;
use super::turn::parse;

impl<St, Rd, E> Dispatcher<St, Rd, E>
where
    St: StoragePort + 'static,
    Rd: ReadModelPort + 'static,
    E: EngineCommands,
{
    pub(super) async fn decide_approval(
        &self,
        id: CommandId,
        c: pb::DecideApproval,
    ) -> Result<CommitReceipt, CommandError> {
        const K: &str = "DecideApproval";
        let approval: ApprovalId = parse(&c.approval_id)?;
        let decision = match pb::ApprovalDecision::try_from(c.decision) {
            Ok(pb::ApprovalDecision::Allow) => UserDecision::Allow {
                // 没说记住范围就是只这一次
                remember: if c.remember == pb::RememberScope::Unspecified as i32 {
                    RememberScope::Once
                } else {
                    remember_scope_from_proto(K, "remember", c.remember)?
                },
            },
            Ok(pb::ApprovalDecision::Deny) => UserDecision::Deny,
            _ => {
                return Err(iwork_app::convert::ConvertError::UnsupportedEnum {
                    kind: K,
                    field: "decision",
                    value: c.decision,
                }
                .into());
            }
        };
        Ok(self
            .ports
            .engine
            .decide(approval, decision, user(), id)
            .await?
            .receipt)
    }

    pub(super) async fn set_approval_mode(
        &self,
        id: CommandId,
        c: pb::SetApprovalMode,
    ) -> Result<CommitReceipt, CommandError> {
        const K: &str = "SetApprovalMode";
        let target = mode_target_from_proto(K, c.target.as_ref())?;
        let mode = if c.follow_supervisor {
            None
        } else {
            Some(approval_mode_from_proto(K, "mode", c.mode)?)
        };
        // 对话在删时不切档（普通命令只在 `Active` 受理）
        if let ApprovalModeTarget::Conversation(conversation)
        | ApprovalModeTarget::Member {
            conversation_id: conversation,
            ..
        } = target
        {
            self.conversation_of(conversation).await?.admit_command()?;
        }
        Ok(self
            .ports
            .engine
            .switch_mode(target, mode, user(), id)
            .await?
            .receipt)
    }
}
