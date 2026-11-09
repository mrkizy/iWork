//! 审批引擎（T07；技术设计第 7 节）。
//!
//! 三层：`iwork-shell`（Bash 语法）→ 纯函数（[`action`] 归一、[`rules`] 规则集、`evaluate` 七步求值、
//! [`session`] 快照与约束）→ 有状态的 `engine`（等待、拒绝记忆、一次性放行、AI 审查、作废、审计；
//! 只经端口碰外界）。路径事实经 `iwork_ports::fs::PathResolver` 取来，归一本身是纯函数。

pub mod action;
pub mod denial;
pub mod env;
pub mod evaluate;
pub mod registry;
pub mod rules;
pub mod session;
