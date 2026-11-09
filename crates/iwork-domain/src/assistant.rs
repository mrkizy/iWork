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
