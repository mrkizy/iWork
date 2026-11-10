//! 帧与分片的上限（设计 3.3；T03 方案 2.5）。
//!
//! 这些数同时约束后台的帧解码（`iwork-ipc`）、`ReadArtifact.limit` 的受理（T10）与客户端分片上传（T09），
//! 所以放在两端都依赖的协议层，只在这里写一次。

/// 单帧长度字段的上限（类型字节 + 载荷）。读到长度就判，超过立刻断开，不等载荷到齐。
pub const MAX_FRAME_LEN: u32 = 4 * 1024 * 1024;

/// `Chunk.data` 与 `ReadArtifact.limit` 的上限。
pub const MAX_CHUNK_DATA: usize = 1024 * 1024;

/// 握手前唯一允许的帧（`Hello`）的上限：对端身份确认、握手完成之前，不为它分配 4MB。
pub const MAX_HELLO_LEN: u32 = 64 * 1024;
