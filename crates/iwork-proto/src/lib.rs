//! iWork 协议：Protobuf 生成类型、描述符、协议主版本，以及几个两端共用的纯 Rust 模块：
//! 握手门（[`handshake`]）、兼容检查（[`compat`]）、帧上限（[`limits`]）、事件路由键与查询分道（[`routing`]）。
//!
//! `.proto` 在仓库根的 `proto/iwork/`，构建时由 protox 编译、prost-build 生成（生成代码不入库）。

include!(concat!(env!("OUT_DIR"), "/protocol.rs"));

/// 全部 `.proto`（含依赖的 `google/protobuf/*`）编译出的 `FileDescriptorSet`，已去掉注释位置信息。
/// 兼容测试与反射（金样本、事件种类对拍）用它。
pub const FILE_DESCRIPTOR_SET: &[u8] =
    include_bytes!(concat!(env!("OUT_DIR"), "/file_descriptor_set.binpb"));

pub mod compat;
pub mod handshake;
pub mod limits;
pub mod routing;
