//! 对话删除期间放行哪些命令（方案 4.3 对话表）。
//!
//! 真值是 [`DELETING_ALLOWED`]：受理入口按表判断，不在各处手写。表里登记的是 proto `Command`
//! oneof 的字段名；每条命令的放行条件（卡片属于本对话、执行中且持有句柄……）是领域规则，
//! 在 `Conversation::admit_cleanup`。查询不受影响（只读）。

use iwork_domain::DomainError;
use iwork_domain::conversation::{CleanupCommand, Conversation};
use iwork_domain::user_action::UserActionCard;
use iwork_proto::pb::command::Kind;

/// 收尾命令的种类。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CleanupKind {
    /// 删除停在 `Deleting` 就是在等它；挡住等于永远删不掉
    ConfirmUserActionEnded,
    /// 删除流程自己会发停止，用户手动点也应当有效
    StopUserAction,
    /// 同一对话重发删除：幂等，不是新操作
    DeleteConversation,
}

/// `Deleting` 期间仍受理的命令（proto `Command` oneof 字段名 → 种类）。加一条 = 加一行。
pub const DELETING_ALLOWED: &[(&str, CleanupKind)] = &[
    (
        "confirm_user_action_ended",
        CleanupKind::ConfirmUserActionEnded,
    ),
    ("stop_user_action", CleanupKind::StopUserAction),
    ("delete_conversation", CleanupKind::DeleteConversation),
];

/// 命令在 proto `Command` oneof 里的字段名。
pub fn command_name(command: &Kind) -> &'static str {
    match command {
        Kind::SendMessage(_) => "send_message",
        Kind::CancelTurn(_) => "cancel_turn",
        Kind::CancelAll(_) => "cancel_all",
        Kind::ResumeTurn(_) => "resume_turn",
        Kind::AbandonTurn(_) => "abandon_turn",
        Kind::SetConversationModel(_) => "set_conversation_model",
        Kind::MarkRead(_) => "mark_read",
        Kind::SwitchDmProject(_) => "switch_dm_project",
        Kind::DeleteConversation(_) => "delete_conversation",
        Kind::CreateConversation(_) => "create_conversation",
        Kind::RenameConversation(_) => "rename_conversation",
        Kind::DecideApproval(_) => "decide_approval",
        Kind::SetApprovalMode(_) => "set_approval_mode",
        Kind::CreateAssistant(_) => "create_assistant",
        Kind::UpdateAssistant(_) => "update_assistant",
        Kind::DeleteAssistant(_) => "delete_assistant",
        Kind::ResetAssistant(_) => "reset_assistant",
        Kind::AddProject(_) => "add_project",
        Kind::CreateProject(_) => "create_project",
        Kind::InitRepository(_) => "init_repository",
        Kind::CloneRepository(_) => "clone_repository",
        Kind::SetProjectsRoot(_) => "set_projects_root",
        Kind::RelocateProject(_) => "relocate_project",
        Kind::PinProject(_) => "pin_project",
        Kind::RemoveProject(_) => "remove_project",
        Kind::ConfirmAgentsMdDraft(_) => "confirm_agents_md_draft",
        Kind::UpdateProjectSettings(_) => "update_project_settings",
        Kind::UploadAttachment(_) => "upload_attachment",
        Kind::ConfirmCredentialAttachment(_) => "confirm_credential_attachment",
        Kind::ExecuteUserAction(_) => "execute_user_action",
        Kind::DismissUserAction(_) => "dismiss_user_action",
        Kind::StopUserAction(_) => "stop_user_action",
        Kind::ConfirmUserActionEnded(_) => "confirm_user_action_ended",
        Kind::RevokeRule(_) => "revoke_rule",
        Kind::AddSecurityListEntry(_) => "add_security_list_entry",
        Kind::RemoveSecurityListEntry(_) => "remove_security_list_entry",
        Kind::ImportLocalConfig(_) => "import_local_config",
        Kind::SetMcpSecret(_) => "set_mcp_secret",
        Kind::DismissLocalImportPrompt(_) => "dismiss_local_import_prompt",
        Kind::RedetectRuntime(_) => "redetect_runtime",
        Kind::SetPlayfulUi(_) => "set_playful_ui",
        Kind::SetAutostart(_) => "set_autostart",
        Kind::ShutdownDaemon(_) => "shutdown_daemon",
    }
}

/// 这条命令在删除期间的种类；不在登记表里返回 `None`。
pub fn cleanup_kind(command: &Kind) -> Option<CleanupKind> {
    let name = command_name(command);
    DELETING_ALLOWED
        .iter()
        .find(|(n, _)| *n == name)
        .map(|&(_, k)| k)
}

/// 受理一条作用于某个对话的命令前的判断。
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum AdmitError {
    #[error(transparent)]
    Domain(#[from] DomainError),
    /// 卡片类收尾命令必须先由调用方查到卡片（查不到时调用方应先返回 NOT_FOUND）
    #[error("命令 {command} 需要先加载卡片")]
    CardNotLoaded { command: &'static str },
}

/// 对话状态允许受理这条命令吗。
///
/// - 登记表里的收尾命令：按 `Conversation::admit_cleanup` 的领域条件判断（`Active`、`Deleting` 都适用）；
/// - 其他命令：只有 `Active` 受理，`Deleting` 返回 `ConversationDeleting`（映射 `CONVERSATION_DELETING`）。
///
/// `card` 是卡片类命令指向的卡片，`handle_held` 是本次后台服务进程是否持有它的执行句柄。
pub fn admit(
    conversation: &Conversation,
    command: &Kind,
    card: Option<&UserActionCard>,
    handle_held: bool,
) -> Result<(), AdmitError> {
    let Some(kind) = cleanup_kind(command) else {
        return Ok(conversation.admit_command()?);
    };
    let name = command_name(command);
    let card = || card.ok_or(AdmitError::CardNotLoaded { command: name });
    let cleanup = match kind {
        CleanupKind::ConfirmUserActionEnded => {
            CleanupCommand::ConfirmUserActionEnded { card: card()? }
        }
        CleanupKind::StopUserAction => CleanupCommand::StopUserAction {
            card: card()?,
            handle_held,
        },
        CleanupKind::DeleteConversation => CleanupCommand::DeleteConversation,
    };
    Ok(conversation.admit_cleanup(cleanup)?)
}
