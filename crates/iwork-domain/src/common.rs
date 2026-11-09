//! 多个实体共用的值对象。

use core::fmt;
use std::collections::BTreeSet;

/// UTC 毫秒时间戳。domain 不读时钟，时间由 `iwork-ports::Clock` 提供后传进来。
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Default)]
pub struct Timestamp(pub i64);

/// SHA-256 摘要（32 字节）。文本形式是 64 位小写十六进制。
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Sha256(pub [u8; 32]);

impl fmt::Display for Sha256 {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        for byte in self.0 {
            write!(f, "{byte:02x}")?;
        }
        Ok(())
    }
}

impl fmt::Debug for Sha256 {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "Sha256({self})")
    }
}

/// 制品存放在哪个区。客户端经 `ReadArtifact` 只能读 `Artifacts` 与 `Attachments`。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ArtifactArea {
    Artifacts,
    Attachments,
    /// 凭据附件存档，任何查询都读不到
    Private,
}

/// 大文本（长消息正文、工具输出、亲手执行输出）不随事件推送，事件只带这个引用（技术设计 3.7）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct ArtifactRef {
    pub sha256: Sha256,
    pub area: ArtifactArea,
    pub size: u64,
}

/// 模型标识，由运行时给出，domain 不解释它的内容。
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct ModelId(pub String);

/// 思考强度，取值由运行时给出（`ListModels` 的 `effort_levels`）。
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct EffortLevel(pub String);

/// 选定的模型与思考强度（技术设计 9.6）。
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct ModelSelection {
    pub model_id: ModelId,
    pub effort: Option<EffortLevel>,
}

/// 助手工具的四个分组（技术设计 9.1a）。被禁用的分组不能在审批卡片上临时启用。
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum ToolGroup {
    ReadAndSearch,
    EditFiles,
    RunCommands,
    Web,
}

/// 一次回合的用量。
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct Usage {
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub cache_read_tokens: u64,
    pub cache_write_tokens: u64,
    /// 运行时报告的花费；不报告的运行时为 `None`
    pub cost_usd: Option<f64>,
}

/// 一组工具分组。
pub type ToolGroups = BTreeSet<ToolGroup>;

/// 做出某个决定的主体，与事件信封的 actor 同一种写法（不透明字符串）。
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct Actor(pub String);
