//! 传输抽象（设计 3.1、3.8；方案 2.2）。本机实现：macOS / Linux 用 Unix domain socket，Windows 用命名管道。
//! 远程（D05）以后在 [`Endpoint`] 加变体、实现同样三个 trait，协议与续传语义不变。
//!
//! iWork 自己的进程不开任何 TCP 端口：这里没有、也不许加 TCP 实现（`scripts/check-no-tcp.sh` 检查）。

use std::future::Future;
use std::io;
use std::path::PathBuf;

use tokio::io::{AsyncRead, AsyncWrite};

use crate::peer::{PeerError, PeerIdentity};

pub(crate) mod identify;
pub mod memory;
#[cfg(unix)]
pub mod unix;
#[cfg(windows)]
pub mod windows;
// 管道 SDDL 是纯字符串：各平台都编译，好让单测在任何主机上跑。
#[cfg_attr(not(windows), allow(dead_code))]
mod pipe_sddl;

/// 本机端点。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Endpoint {
    /// Unix domain socket 路径。
    Unix(PathBuf),
    /// Windows 命名管道名（`\\.\pipe\…`）。
    Pipe(String),
    /// 进程内传输（测试与调试工具），见 [`memory`]。
    Memory(String),
}

/// 服务端监听。
pub trait Listener: Send {
    type Conn: Connection;
    fn accept(&mut self) -> impl Future<Output = io::Result<Self::Conn>> + Send;
    fn endpoint(&self) -> &Endpoint;
}

/// 一条已建立、尚未读写任何字节的连接。
pub trait Connection: Send + Sync + 'static {
    type Read: AsyncRead + Unpin + Send + 'static;
    type Write: AsyncWrite + Unpin + Send + 'static;
    /// 操作系统告诉我们的对端身份。两端都在读写任何字节之前调用：服务端看客户端，客户端看服务端。
    /// 不要直接对它套超时，用 [`identify::identify_by`]：超时时要 [`Connection::abort`]。
    fn peer(&self) -> impl Future<Output = Result<PeerIdentity, PeerError>> + Send;
    /// 立刻关掉通信，对端随即读到 EOF。识别对端超时时调，之后连接就被丢掉。要作用在底层通道本身，不能只关自己
    /// 这一个句柄：卡住的识别可能还拿着一份复制的句柄（unix 的 `peer()` 就是这样）。
    fn abort(&self);
    fn into_split(self) -> (Self::Read, Self::Write);
}

/// 客户端建连。
pub trait Connector: Send + Sync {
    type Conn: Connection;
    fn connect(&self, endpoint: &Endpoint) -> impl Future<Output = io::Result<Self::Conn>> + Send;
}
