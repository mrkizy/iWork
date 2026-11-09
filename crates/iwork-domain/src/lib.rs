//! iWork 领域模型：ID、实体与值对象、状态机、审批求值类型、领域事件。
//!
//! 无 I/O、不读时钟、无随机数：时间与 ID 由 `iwork-ports` 的端口提供后传进来。
//! 状态机都写成「const 转换表 + 纯函数 `next()`」，表外转换返回点名的错误（见 [`machine`]）。
//! 这里不出现任何厂商名与版本控制工具名：运行时与版本控制都是不透明的种类字符串。

pub mod machine;

pub mod action;
pub mod approval;
pub mod approval_mode;
pub mod assistant;
pub mod attachment;
pub mod common;
pub mod conversation;
pub mod error;
pub mod event;
pub mod id;
pub mod message;
pub mod project;
pub mod rule;
pub mod runtime;
pub mod security_list;
pub mod session;
pub mod settings;
pub mod turn;
pub mod user_action;

pub use error::DomainError;
