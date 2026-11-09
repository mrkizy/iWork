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
/// - 释放时尽力覆盖当前 `Vec` 里的有效字节：`fill(0)` 后把引用交给 `black_box`。
///   `black_box` 只是给优化器的提示，**不保证**优化后的程序一定执行这次清零，所以这不等同于
///   zeroize crate 的易失写入（ports 的依赖白名单只有 thiserror，没有引入它）。
///   覆盖范围也只是当前缓冲区的有效长度：构造前调用方手里的副本、`Vec` 扩容留下的旧缓冲区、
///   容量里长度以外的字节都不在其中。需要更强保证时由持有密钥的实现（T10 的钥匙串）自行处理。
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
