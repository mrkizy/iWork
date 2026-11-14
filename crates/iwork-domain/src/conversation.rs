//! 对话（技术设计 6.4、9.0、9.1）。

use crate::approval_mode::ApprovalMode;
use crate::common::{ModelSelection, Timestamp};
use crate::error::DomainError;
use crate::id::{AssistantId, ConversationId, ProjectId, Seq};
use crate::machine::{IllegalTransition, Transition, finite_enum, lookup};
use crate::user_action::{UserActionCard, UserActionStateKind};

/// 对话类型。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ConversationKind {
    /// 项目对话
    Project { project_id: ProjectId },
    /// 助手私聊：每个助手一条；`context_project` 是标题栏项目选择器的值，`None` 表示「私聊」
    AssistantDm {
        assistant_id: AssistantId,
        context_project: Option<ProjectId>,
    },
}

finite_enum! {
    /// 对话状态。`Deleted` 是终态。
    #[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
    pub enum ConversationState {
        Active,
        /// 正在收尾：取消运行中的回合、停止持有句柄的卡片、作废待处理项
        Deleting,
        Deleted,
    }
}

finite_enum! {
    /// 对话的触发。
    #[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
    pub enum ConversationTrigger {
        DeleteRequested,
        /// 回合与卡片全部到终态（`Unknown` 的卡片不算终态，要等用户确认）
        AllSettled,
    }
}

use ConversationState as S;
use ConversationTrigger as T;

/// 对话转换表（方案 4.3）。
pub const CONVERSATION_TRANSITIONS: &[Transition<ConversationState, ConversationTrigger>] = &[
    (S::Active, T::DeleteRequested, S::Deleting),
    (S::Deleting, T::AllSettled, S::Deleted),
];

pub const CONVERSATION_MACHINE: &str = "conversation";

pub type ConversationIllegal = IllegalTransition<ConversationState, ConversationTrigger>;

/// 按 [`CONVERSATION_TRANSITIONS`] 求下一状态。
pub fn next(
    state: ConversationState,
    trigger: ConversationTrigger,
) -> Result<ConversationState, ConversationIllegal> {
    lookup(
        CONVERSATION_MACHINE,
        CONVERSATION_TRANSITIONS,
        state,
        trigger,
    )
}

impl ConversationState {
    pub fn is_terminal(self) -> bool {
        self == S::Deleted
    }
}

/// 删除流程在等的收尾命令。`Deleting` 期间只有这些命令能进来，进来前还要过各自的条件。
///
/// 哪些命令名走这里，由 `iwork-app` 的登记表 `DELETING_ALLOWED` 决定（命令枚举在 proto 里，
/// domain 看不到）；每条命令的放行条件在这里判断，是领域规则。
#[derive(Debug, Clone, Copy)]
pub enum CleanupCommand<'a> {
    /// 删除停在 `Deleting` 就是在等它；挡住等于永远删不掉
    ConfirmUserActionEnded { card: &'a UserActionCard },
    /// 删除流程自己会发停止，用户手动点也应当有效
    StopUserAction {
        card: &'a UserActionCard,
        /// 本次后台服务进程是否仍持有这张卡片的执行句柄（内存事实，由调用方提供）
        handle_held: bool,
    },
    /// 同一对话重发删除：幂等，不是新操作
    DeleteConversation,
}

/// 对话实体。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Conversation {
    pub id: ConversationId,
    pub kind: ConversationKind,
    pub state: ConversationState,
    pub last_read_seq: Seq,
    pub model_override: Option<ModelSelection>,
    /// 对话级档位切换。私聊里就是私聊自己的覆盖值
    pub approval_mode: Option<ApprovalMode>,
    pub updated_at: Timestamp,
}

impl Conversation {
    /// 普通命令（收尾命令以外的全部命令）能否受理：只有 `Active` 能。
    pub fn admit_command(&self) -> Result<(), DomainError> {
        match self.state {
            S::Active => Ok(()),
            S::Deleting => Err(DomainError::ConversationDeleting { id: self.id }),
            S::Deleted => Err(DomainError::ConversationDeleted { id: self.id }),
        }
    }

    /// 收尾命令能否受理。`Active` 与 `Deleting` 下都按同样的条件判断；`Deleted` 一律拒绝。
    pub fn admit_cleanup(&self, command: CleanupCommand<'_>) -> Result<(), DomainError> {
        if self.state == S::Deleted {
            return Err(DomainError::ConversationDeleted { id: self.id });
        }
        match command {
            CleanupCommand::ConfirmUserActionEnded { card } => self.check_owns(card),
            CleanupCommand::StopUserAction { card, handle_held } => {
                self.check_owns(card)?;
                let state = card.state.kind();
                if state == UserActionStateKind::Executing && handle_held {
                    Ok(())
                } else {
                    Err(DomainError::UserActionNotStoppable {
                        card: card.id,
                        state,
                        handle_held,
                    })
                }
            }
            CleanupCommand::DeleteConversation => Ok(()),
        }
    }

    /// 发起删除：`Active → Deleting`。助手私聊不能删除；已在删除中的重发由调用方按幂等处理，
    /// 不走这里（见 [`CleanupCommand::DeleteConversation`]）。
    pub fn request_delete(&mut self) -> Result<(), DomainError> {
        if let ConversationKind::AssistantDm { .. } = self.kind {
            return Err(DomainError::AssistantDmNotDeletable { id: self.id });
        }
        self.state = next(self.state, T::DeleteRequested)?;
        Ok(())
    }

    /// 收尾完成：`Deleting → Deleted`。
    pub fn settle(&mut self) -> Result<(), DomainError> {
        self.state = next(self.state, T::AllSettled)?;
        Ok(())
    }

    /// 私聊切换所选项目（`DmProjectSwitched`）。私聊自己的档位覆盖一并清掉：
    /// 需求 FR11「切换项目时全开失效，回到该项目的默认档位」，方案 Q7 已定连自动覆盖一起清。
    pub fn switch_dm_project(&mut self, project: Option<ProjectId>) -> Result<(), DomainError> {
        let ConversationKind::AssistantDm {
            context_project, ..
        } = &mut self.kind
        else {
            return Err(DomainError::NotAssistantDm { id: self.id });
        };
        *context_project = project;
        self.approval_mode = None;
        Ok(())
    }

    fn check_owns(&self, card: &UserActionCard) -> Result<(), DomainError> {
        if card.conversation_id == self.id {
            Ok(())
        } else {
            Err(DomainError::CardNotInConversation {
                card: card.id,
                conversation: self.id,
            })
        }
    }
}

/// 级联删除的前提（T10a 方案 0a.3 第 X5 行）：被级联掉的对话名下没有没到终态的回合。回合还在跑时
/// 删掉它的记录，进程没了以后崩溃恢复就找不到它。`ConversationDeleted` 与连带删私聊的 `AssistantDeleted`
/// 在写线程上过这一处。卡片那一半由 T10e 加参数。
pub fn check_cascade(
    conversation: ConversationId,
    turns: &[(crate::id::TurnId, crate::turn::TurnState)],
) -> Result<(), crate::approval::PreconditionFailed> {
    match turns.iter().find(|(_, state)| !state.is_terminal()) {
        None => Ok(()),
        Some((turn, state)) => Err(crate::approval::PreconditionFailed::new(format!(
            "对话 {conversation} 的回合 {turn} 还是 {state:?}，不能删掉它的记录"
        ))),
    }
}

/// `ConversationRead` 的状态前提（T10a 1/F-2）：已读位置只往前走，事件的位置要比库里现在的 `current` 大。
/// 受理函数在读到的位置上判「是不是往前」；两条连接交错时，读得早的那一笔可能在更大的位置落盘之后才到，
/// 直接写就把已读位置拨回去、未读数按小的位置重算。写线程拒掉它，事件流里的已读位置因此单调，
/// 投影与客户端照事件写都不会倒退。
pub fn check_read_committed(
    current: Seq,
    e: &crate::event::ConversationRead,
) -> Result<(), crate::approval::PreconditionFailed> {
    if e.last_read_seq > current {
        Ok(())
    } else {
        Err(crate::approval::PreconditionFailed::new(format!(
            "对话 {} 的已读位置只能往前：事件是 {}，库里已经是 {}",
            e.conversation_id, e.last_read_seq.0, current.0
        )))
    }
}
