//! 审批档位与有效档位求值（需求 FR11）。

use crate::machine::finite_enum;

finite_enum! {
    /// 审批档位。全开只在当前对话生效、不可设为默认（需求 FR11）。
    #[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
    pub enum ApprovalMode {
        Manual,
        Auto,
        FullAccess,
    }
}

finite_enum! {
    /// 项目默认档位：只有手动与自动两个变体，「项目默认全开」在类型上写不出来。
    #[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
    pub enum ProjectDefaultMode {
        #[default]
        Manual,
        Auto,
    }
}

impl From<ProjectDefaultMode> for ApprovalMode {
    fn from(mode: ProjectDefaultMode) -> Self {
        match mode {
            ProjectDefaultMode::Manual => Self::Manual,
            ProjectDefaultMode::Auto => Self::Auto,
        }
    }
}

finite_enum! {
    /// 批准时「记住」的范围（技术设计 7.3）。
    #[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
    pub enum RememberScope {
        Once,
        Conversation,
        Project,
        Forever,
    }
}

/// 求有效档位的输入。两种对话的输入不同，用枚举分开，不共用一个「对话设置」字段——
/// 这样私聊的输入里根本没有 iWork 对话的设置，「私聊跟随 iWork 对话」在类型上写不出来。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ModeInputs {
    /// 项目对话：iWork 对话里的切换对全体成员生效，成员单独设置优先。
    ProjectConversation {
        member_override: Option<ApprovalMode>,
        conversation_setting: Option<ApprovalMode>,
        project_default: ProjectDefaultMode,
    },
    /// 助手私聊：有自己的覆盖值；没有时按所选项目的默认档位；未选项目时手动。
    /// 切换项目时调用方把 `own_setting` 清成 `None`（`DmProjectSwitched`，方案 Q7 已定全清）。
    AssistantDm {
        own_setting: Option<ApprovalMode>,
        selected_project_default: Option<ProjectDefaultMode>,
    },
}

/// 有效档位：项目对话按「成员单独设置 > 对话设置 > 项目默认」，
/// 私聊按「自己的设置 > 所选项目默认 > 手动」。
pub fn effective_mode(inputs: &ModeInputs) -> ApprovalMode {
    match *inputs {
        ModeInputs::ProjectConversation {
            member_override,
            conversation_setting,
            project_default,
        } => member_override
            .or(conversation_setting)
            .unwrap_or(project_default.into()),
        ModeInputs::AssistantDm {
            own_setting,
            selected_project_default,
        } => own_setting
            .or(selected_project_default.map(ApprovalMode::from))
            .unwrap_or(ApprovalMode::Manual),
    }
}
