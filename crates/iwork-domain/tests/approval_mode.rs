//! 有效档位求值（需求 FR11；方案 4.3 `effective_mode_table`）。期望值逐条手写，不从实现推。

use iwork_domain::approval_mode::{ApprovalMode, ModeInputs, ProjectDefaultMode, effective_mode};

use ApprovalMode::{Auto, FullAccess, Manual};

fn project_conv(
    member: Option<ApprovalMode>,
    conversation: Option<ApprovalMode>,
    default: ProjectDefaultMode,
) -> ModeInputs {
    ModeInputs::ProjectConversation {
        member_override: member,
        conversation_setting: conversation,
        project_default: default,
    }
}

fn dm(own: Option<ApprovalMode>, project_default: Option<ProjectDefaultMode>) -> ModeInputs {
    ModeInputs::AssistantDm {
        own_setting: own,
        selected_project_default: project_default,
    }
}

#[test]
fn effective_mode_table() {
    let cases: [(&str, ModeInputs, ApprovalMode); 12] = [
        // 1. 项目对话，无任何设置 → 项目默认
        (
            "1 默认手动",
            project_conv(None, None, ProjectDefaultMode::Manual),
            Manual,
        ),
        (
            "1 默认自动",
            project_conv(None, None, ProjectDefaultMode::Auto),
            Auto,
        ),
        // 2. iWork 切到自动，某成员单独设置手动 → 该成员手动，其他成员自动
        (
            "2 单独设置的成员",
            project_conv(Some(Manual), Some(Auto), ProjectDefaultMode::Manual),
            Manual,
        ),
        (
            "2 其他成员",
            project_conv(None, Some(Auto), ProjectDefaultMode::Manual),
            Auto,
        ),
        // 3. 切到全开 → 全开；单独设置手动的成员仍手动
        (
            "3 全开",
            project_conv(None, Some(FullAccess), ProjectDefaultMode::Manual),
            FullAccess,
        ),
        (
            "3 单独设置的成员",
            project_conv(Some(Manual), Some(FullAccess), ProjectDefaultMode::Auto),
            Manual,
        ),
        // 4. 私聊选了默认自动的项目 → 自动；iWork 对话切全开不在私聊的输入里，仍自动
        (
            "4 私聊跟项目默认",
            dm(None, Some(ProjectDefaultMode::Auto)),
            Auto,
        ),
        // 5. 私聊自己切到全开 → 全开
        (
            "5 私聊全开",
            dm(Some(FullAccess), Some(ProjectDefaultMode::Auto)),
            FullAccess,
        ),
        // 6. 私聊全开时切到默认手动的项目：DmProjectSwitched 清掉自己的设置 → 手动
        (
            "6 切项目后",
            dm(None, Some(ProjectDefaultMode::Manual)),
            Manual,
        ),
        // 7. 私聊未选项目 → 手动；未选项目时自己切到自动 → 自动
        ("7 未选项目", dm(None, None), Manual),
        ("7 未选项目自己切自动", dm(Some(Auto), None), Auto),
        // 对照：未选项目时自己切全开也立即生效
        (
            "7 未选项目自己切全开",
            dm(Some(FullAccess), None),
            FullAccess,
        ),
    ];
    for (name, inputs, expected) in cases {
        assert_eq!(effective_mode(&inputs), expected, "用例「{name}」");
    }
}
