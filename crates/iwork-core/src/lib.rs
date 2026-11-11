//! 编排核心层：调度、等待与唤醒、消息路由、审批引擎、防失控底线（技术设计第 2 节）。
//!
//! 只依赖 `iwork-domain` 与 `iwork-ports`，不知道 SQLite、git、ACP 的存在，也不绑定异步运行时
//! （只用无运行时的 `futures-channel` 做内部信箱）。
//!
//! - [`project_identity`]：项目身份规则（去重、重新定位、路径检查），T08。

pub mod approval;
pub mod project_identity;
pub mod turn;
