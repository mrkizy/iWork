//! 安全清单（技术设计 7.2c）：需凭据命令、敏感目录、可写目录三张清单。
//!
//! 清单条目**就是**规则条目 [`RuleEntry`]（需求第 9 章「安全规则的组织」：安全规则都是同一种
//! 带 `id` 的条目），不另设类型、不另存一份：
//! - 属于哪张清单看 `layer`（[`SecurityList::layer`] / [`RuleEntry::security_list`]）；
//! - 清单的值在 `matcher` 里：命令清单用 `ProgramSubcommand`，目录清单用 `PathUnder`；
//! - 条目 ID 是 `RuleId`，作用范围是 `scope`，内置与否看 `source`（[`RuleEntry::is_builtin`]）。
//!
//! 条目的校验函数在 `iwork-core`（T07）；这里只定清单种类、视图推导与校验失败的类型。

use crate::rule::{RuleEntry, RuleLayer, RuleSource};

/// 三张清单。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum SecurityList {
    CredentialCommands,
    SensitiveDirs,
    WritableDirs,
}

impl SecurityList {
    /// 这张清单的条目所在的规则层。
    pub fn layer(self) -> RuleLayer {
        match self {
            Self::CredentialCommands => RuleLayer::CredentialCommands,
            Self::SensitiveDirs => RuleLayer::SensitiveDirs,
            Self::WritableDirs => RuleLayer::WritableDirs,
        }
    }
}

impl RuleEntry {
    /// 这条规则属于哪张安全清单；不是清单条目返回 `None`。
    pub fn security_list(&self) -> Option<SecurityList> {
        match self.layer {
            RuleLayer::CredentialCommands => Some(SecurityList::CredentialCommands),
            RuleLayer::SensitiveDirs => Some(SecurityList::SensitiveDirs),
            RuleLayer::WritableDirs => Some(SecurityList::WritableDirs),
            RuleLayer::Baseline
            | RuleLayer::DenialMemory
            | RuleLayer::ToolConfig
            | RuleLayer::UserDeny
            | RuleLayer::MustAsk
            | RuleLayer::Remembered
            | RuleLayer::ProjectAllow
            | RuleLayer::Default
            | RuleLayer::AutorunConfig
            | RuleLayer::WritableCache
            | RuleLayer::RejectList => None,
        }
    }

    /// 内置条目（随规则集发布）不可删。
    pub fn is_builtin(&self) -> bool {
        matches!(self.source, RuleSource::Builtin { .. })
    }
}

/// 用户添加的条目被拒收：理由给人看（映射 `SECURITY_LIST_ENTRY_REJECTED`）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ListEntryRejection {
    pub reason: String,
}
