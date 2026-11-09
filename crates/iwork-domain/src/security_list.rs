//! 安全清单（技术设计 7.2c）：需凭据命令、敏感目录、可写目录三张清单。
//!
//! 条目的校验函数在 `iwork-core`（T07）；这里只定条目与校验失败的类型。

use crate::common::Timestamp;
use crate::id::SecurityEntryId;
use crate::rule::RuleScope;

/// 三张清单。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum SecurityList {
    CredentialCommands,
    SensitiveDirs,
    WritableDirs,
}

/// 安全清单条目。内置条目不可删。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SecurityListEntry {
    pub id: SecurityEntryId,
    pub list: SecurityList,
    pub value: String,
    pub scope: RuleScope,
    pub builtin: bool,
    pub created_at: Timestamp,
}

/// 用户添加的条目被拒收：理由给人看（映射 `SECURITY_LIST_ENTRY_REJECTED`）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ListEntryRejection {
    pub reason: String,
}
