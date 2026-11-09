//! 密钥存取（需求第 15 章）：按名存取，只有后台服务进程可取值；M1 存 MCP 密钥。

use core::fmt;
use std::future::Future;

/// 密钥名。
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct SecretName(pub String);

/// 密钥值。
///
/// - `Debug` 只打印 `<redacted>`；不实现 `Display`、不实现序列化、不实现 `Clone`，
///   防止它随日志、事件或诊断包出去，也少一份内存副本。
/// - 释放时把字节清零。方案写的是 zeroize crate，但 ports 的依赖白名单只有 thiserror，
///   所以手写：`fill(0)` 后用 `black_box` 防止编译器把这次写入当死存储优化掉。这是尽力而为：
///   构造前调用方手里的副本、`Vec` 扩容留下的旧缓冲区都不在清零范围内。
pub struct SecretValue(Vec<u8>);

impl SecretValue {
    pub fn new(bytes: Vec<u8>) -> Self {
        Self(bytes)
    }

    /// 取出明文字节。只在真正要用时调用（交给 MCP 进程的环境变量等）。
    pub fn expose(&self) -> &[u8] {
        &self.0
    }
}

impl fmt::Debug for SecretValue {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("<redacted>")
    }
}

impl Drop for SecretValue {
    fn drop(&mut self) {
        self.0.fill(0);
        std::hint::black_box(&self.0);
    }
}

/// 密钥存储（系统钥匙串）。
pub trait SecretStore: Send + Sync {
    fn set(
        &self,
        name: &SecretName,
        value: SecretValue,
    ) -> impl Future<Output = Result<(), SecretError>> + Send;
    fn get(
        &self,
        name: &SecretName,
    ) -> impl Future<Output = Result<Option<SecretValue>, SecretError>> + Send;
    fn delete(&self, name: &SecretName) -> impl Future<Output = Result<(), SecretError>> + Send;
}

/// 密钥存储错误。只点名密钥名，绝不带值。
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum SecretError {
    #[error("系统钥匙串不可用：{message}")]
    Unavailable { message: String },
    #[error("密钥 {name:?} 存取失败：{message}")]
    Backend { name: String, message: String },
}

impl SecretError {
    pub fn retryable(&self) -> bool {
        matches!(self, Self::Unavailable { .. })
    }
}
