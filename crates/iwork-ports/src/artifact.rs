//! 制品存储（技术设计 5.4）。按内容哈希寻址；区域决定目录与 Agent 可见性（需求 v1.68）。

use std::future::Future;
use std::ops::Range;
use std::path::{Path, PathBuf};

use iwork_domain::common::{ArtifactArea, ArtifactRef, Sha256};

/// 一次垃圾回收的结果。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct GcReport {
    pub removed: u64,
    pub freed_bytes: u64,
}

/// 制品存储。字节用 `Vec<u8>`（不用 `bytes::Bytes`：ports 的依赖白名单只有 thiserror）。
pub trait ArtifactStore: Send + Sync {
    fn put_bytes(
        &self,
        area: ArtifactArea,
        data: Vec<u8>,
    ) -> impl Future<Output = Result<ArtifactRef, ArtifactError>> + Send;
    fn put_file(
        &self,
        area: ArtifactArea,
        path: &Path,
    ) -> impl Future<Output = Result<ArtifactRef, ArtifactError>> + Send;
    /// 读 `range` 范围的字节；越界返回 `RangeInvalid`。
    fn read_range(
        &self,
        r: &ArtifactRef,
        range: Range<u64>,
    ) -> impl Future<Output = Result<Vec<u8>, ArtifactError>> + Send;
    /// 引用计数加一（附件副本按引用计数清理）。
    fn retain(&self, r: &ArtifactRef) -> impl Future<Output = Result<(), ArtifactError>> + Send;
    fn release(&self, r: &ArtifactRef) -> impl Future<Output = Result<(), ArtifactError>> + Send;
    fn collect_garbage(&self) -> impl Future<Output = Result<GcReport, ArtifactError>> + Send;
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ArtifactError {
    #[error("找不到制品 {sha256}")]
    NotFound { sha256: Sha256 },
    #[error("制品 {sha256} 的范围 {start}..{end} 越界（大小 {size}）")]
    RangeInvalid {
        sha256: Sha256,
        start: u64,
        end: u64,
        size: u64,
    },
    #[error("制品读写失败：{path}：{message}")]
    Io { path: PathBuf, message: String },
}

impl ArtifactError {
    pub fn retryable(&self) -> bool {
        false
    }
}
