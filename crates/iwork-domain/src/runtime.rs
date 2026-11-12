//! 运行时：本机检测到的 Agent CLI（技术设计 6.5、9.4、9.5）。
//!
//! 运行时种类是不透明字符串，厂商名只在适配层出现。

use std::path::PathBuf;

use crate::common::Timestamp;
use crate::id::RuntimeId;

/// 运行时种类：不透明字符串，显示名由适配器给。
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct RuntimeKind(pub String);

/// 运行时状态。由重新检测的结果整体替换，不是逐步转换，所以没有转换表。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum RuntimeState {
    Missing,
    NotLoggedIn,
    ReadyVerified,
    ReadyUnverified,
    Incompatible,
}

/// 登录信息：**只有这四个字段**，是安全不变量允许保存的全部。
/// 不得新增邮箱、账号名、组织等任何字段；需要时交产品负责人决定。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LoginInfo {
    pub logged_in: bool,
    pub auth_method: String,
    pub subscription_type: String,
    pub login_expires_at: Option<Timestamp>,
}

/// 运行时实体。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Runtime {
    pub id: RuntimeId,
    pub kind: RuntimeKind,
    pub display_name: String,
    pub cli_path: Option<PathBuf>,
    pub cli_version: Option<String>,
    pub adapter_version: String,
    pub state: RuntimeState,
    pub login: Option<LoginInfo>,
}

/// 一个额度窗口（技术设计 9.4）。上报了哪些窗口就有哪些。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct QuotaWindow {
    /// 窗口类型，运行时给的不透明字符串
    pub window: String,
    /// 使用率百分比，0–100
    pub used_pct: u8,
    pub resets_at: Option<Timestamp>,
    pub observed_at: Timestamp,
}

/// 本机配置项的种类（技术设计 9.5）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum LocalConfigKind {
    Mcp,
    Skill,
    Plugin,
}

/// 本机可导入的一项配置。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LocalConfigItem {
    /// 后台给的稳定 id，`ImportLocalConfig` 按它勾选
    pub id: String,
    pub kind: LocalConfigKind,
    pub name: String,
    pub description: String,
    /// 来源目录或配置文件，给人看
    pub source: String,
    pub imported: bool,
    /// MCP：要用户填值的键名（只有键名，值不读）；别的种类为空
    pub value_keys: Vec<String>,
    /// 导不了的原因（给人看，后台拼好）；`None` = 能导
    pub blocked_reason: Option<String>,
}
