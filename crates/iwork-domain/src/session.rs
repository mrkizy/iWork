//! 随会话下发给运行时的抽象约束（技术设计 7.0、7.2b）。
//!
//! 编排核心只产出抽象约束，翻译成具体运行时的设置项是适配器的事。

use std::path::PathBuf;

use crate::common::ToolGroups;

/// 沙箱抽象约束。系统底线三档相同；只写白名单只在手动、自动档生效，全开档放开为 `Anywhere`。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionConstraints {
    /// 禁读禁写：凭据目录、iWork 数据目录
    pub deny_read_write: Vec<PathBuf>,
    /// 只读区：数据目录下的附件区与导入区
    pub read_only: Vec<PathBuf>,
    pub allow_write: AllowWrite,
    /// 需凭据命令清单：在沙箱外运行
    pub excluded_commands: Vec<CommandPattern>,
    /// 清单外命令被挡后可请求沙箱外重试（手动、自动档为 true，全开档为 false）
    pub allow_unsandboxed_retry: bool,
    pub network: NetworkPolicy,
}

/// 可写范围。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AllowWrite {
    Whitelist(Vec<PathBuf>),
    /// 全开档：可写任何位置，底线目录仍禁读禁写
    Anywhere,
}

/// 程序名 + 可选子命令。
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct CommandPattern {
    pub program: String,
    pub subcommand: Option<String>,
}

/// 联网策略。联网不受控（需求 v1.38），目前只有 `Open`；以后要收紧只能加值。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum NetworkPolicy {
    Open,
}

/// 本次会话可用的工具（技术设计 7.2b）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionToolSet {
    pub enabled_groups: ToolGroups,
    /// 是否提供不属于四个分组、自身无副作用的工具。AI 审查回合为 false
    pub include_always_on: bool,
}
