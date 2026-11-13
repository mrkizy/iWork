//! 助手（技术设计 9.1a）。

use crate::common::{ModelSelection, Timestamp, ToolGroups};
use crate::id::{AssistantId, RuntimeId};

/// 头像：首字与配色。
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct Avatar {
    pub initial: String,
    pub color: String,
}

/// 助手的一份配置。每次修改产生新版本，回合记下开始时的版本号。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AssistantConfig {
    pub name: String,
    pub avatar: Avatar,
    pub prompt: String,
    pub runtime_id: RuntimeId,
    pub model: ModelSelection,
    /// 被禁用的工具分组；被禁用的不能在审批卡片上临时启用，要用须到助手配置里启用
    pub disabled_groups: ToolGroups,
}

/// 助手配置的一个版本。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AssistantVersion {
    /// 从 1 开始；写入携带 `expected_version`，不符即冲突（需求 10.1）
    pub version: u32,
    pub config: AssistantConfig,
    pub created_at: Timestamp,
}

/// 助手实体。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Assistant {
    pub id: AssistantId,
    /// 内置助手 iWork：可改、可重置，不可删除
    pub builtin: bool,
    pub current: AssistantVersion,
}

/// 内置助手 iWork 的默认配置（T10a 方案 2.8 第 1a 步）。启动时登记、T10b 的 `ResetAssistant` 都只从这里取
/// （AC21「可重置」）。名字与运行时有依据（需求第 3 章、技术设计 9.1a）；提示词、模型、工具开关、头像四样文档没写，
/// 模型与思考强度产品负责人 2026-11-13 定为 opus / high，另三样先按推荐取值；改了只动这一个函数（方案第 10 节第 5 条）。
pub fn builtin_iwork_config() -> AssistantConfig {
    AssistantConfig {
        name: "iWork".into(),
        // 界面上内置 iWork 显示的是 iWork 标志（ui-m1.md），这两个字段只是落库的占位
        avatar: Avatar {
            initial: "i".into(),
            color: "#8A8F98".into(),
        },
        prompt: String::new(),
        runtime_id: RuntimeId::DEFAULT_RUNTIME,
        // 适配器的模型别名与思考强度（产品负责人 2026-11-13 定）
        model: crate::common::ModelSelection {
            model_id: crate::common::ModelId("opus".into()),
            effort: Some(crate::common::EffortLevel("high".into())),
        },
        // 四组工具都开
        disabled_groups: ToolGroups::new(),
    }
}
