//! iWork 应用服务（T01 只有映射骨架，命令受理在 T10）。
//!
//! - [`convert`]：domain ↔ proto 的转换，写成普通函数（两边类型对本 crate 都是外部类型，
//!   孤儿规则不允许 `From` impl；方案 3.6）。
//! - [`codec::ProtoEventCodec`]：`EventCodec` 端口的实现，存储层经它编解码事件。
//! - [`details`]：对外错误 `Error.details` 的键。
//! - [`admission`]：对话删除期间放行的收尾命令登记表 `DELETING_ALLOWED`。
//! - [`lifecycle`]：完全退出的判定（T04）。
//! - [`turn_command`]、[`conversation_command`]、[`settings_command`]：命令受理的计划（纯函数，T10a 方案 2.6），
//!   拒绝与错误码在 [`refusal`]。
//! - [`subscription`]：订阅范围 → 路由键与快照查询、事件 → 路由键（T03 方案 2.8）。

pub mod admission;
pub mod codec;
pub mod conversation_command;
pub mod convert;
pub mod details;
pub mod lifecycle;
pub mod message_blocks;
pub mod refusal;
pub mod settings_command;
pub mod subscription;
pub mod turn_command;
