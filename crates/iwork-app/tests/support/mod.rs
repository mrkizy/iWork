//! app 测试的公共部件。每个测试文件各自 `mod support;`，用不到的部分在那个编译单元里是死代码。
#![allow(dead_code)]

pub mod arb;

use iwork_proto::FILE_DESCRIPTOR_SET;
use prost::Message;
use prost_types::{DescriptorProto, FileDescriptorSet};

/// 在描述符集里找 `iwork.v1.<name>` 消息。
pub fn message_descriptor(name: &str) -> DescriptorProto {
    let fds = FileDescriptorSet::decode(FILE_DESCRIPTOR_SET).expect("描述符集能解码");
    fds.file
        .into_iter()
        .filter(|f| f.package() == iwork_proto::PROTO_PACKAGE)
        .flat_map(|f| f.message_type)
        .find(|m| m.name() == name)
        .unwrap_or_else(|| panic!("描述符集里没有 {name}"))
}

/// 消息里属于 oneof 的字段：(编号, 字段名)，按声明顺序。
pub fn oneof_fields(name: &str) -> Vec<(i32, String)> {
    message_descriptor(name)
        .field
        .into_iter()
        .filter(|f| f.oneof_index.is_some() && !f.proto3_optional())
        .map(|f| (f.number(), f.name().to_owned()))
        .collect()
}
