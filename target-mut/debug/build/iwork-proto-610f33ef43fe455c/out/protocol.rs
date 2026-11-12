/// 协议主版本，从 proto 包名 `iwork.v1` 解析得出（构建时生成，不手抄）。
pub const PROTOCOL_MAJOR: u32 = 1;
/// 当前协议的 proto 包名。
pub const PROTO_PACKAGE: &str = "iwork.v1";
/// 生成的 Protobuf 类型。路径不带版本号：升主版本只改 `.proto` 的包名。
#[allow(clippy::all, clippy::pedantic, missing_docs, reason = "prost 生成代码，不手改")]
pub mod pb {
    include!("/Users/lilithgames/LiWork/iWork/.cao/worktrees/c87fefd6/target-mut/debug/build/iwork-proto-610f33ef43fe455c/out/iwork.v1.rs");
}
