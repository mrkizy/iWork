//! macOS、Linux：Unix domain socket（方案 2.2）。
//!
//! 对端身份由 `peer::{linux, macos}::identify_socket_peer` 从 socket 本身取（`SO_PEERCRED` / `getpeereid`、
//! `LOCAL_PEERTOKEN`），两端走同一条路：服务端拿到的是客户端，客户端拿到的是服务端。

use std::io;
use std::os::fd::AsFd;
use std::path::Path;

use tokio::net::unix::{OwnedReadHalf, OwnedWriteHalf};
use tokio::net::{UnixListener, UnixStream};

use super::{Connection, Connector, Endpoint, Listener};
use crate::peer::{self, PeerError, PeerIdentity};

pub struct LocalListener {
    inner: UnixListener,
    endpoint: Endpoint,
}

impl LocalListener {
    /// 绑定 socket。前置条件：调用方已持有 `run/daemon.lock`（T04），所以这里删掉的残留 socket 文件
    /// 一定不属于另一个活着的后台。目录权限由 [`crate::LocalEndpoint`] 事先校验。
    pub fn bind(path: &Path) -> io::Result<LocalListener> {
        match std::fs::remove_file(path) {
            Ok(()) => {}
            Err(e) if e.kind() == io::ErrorKind::NotFound => {}
            Err(e) => return Err(e),
        }
        Ok(LocalListener {
            inner: UnixListener::bind(path)?,
            endpoint: Endpoint::Unix(path.to_path_buf()),
        })
    }
}

impl Listener for LocalListener {
    type Conn = LocalConn;

    async fn accept(&mut self) -> io::Result<LocalConn> {
        let (stream, _) = self.inner.accept().await?;
        Ok(LocalConn { stream })
    }

    fn endpoint(&self) -> &Endpoint {
        &self.endpoint
    }
}

#[derive(Debug, Default, Clone, Copy)]
pub struct LocalConnector;

impl Connector for LocalConnector {
    type Conn = LocalConn;

    async fn connect(&self, endpoint: &Endpoint) -> io::Result<LocalConn> {
        match endpoint {
            Endpoint::Unix(path) => Ok(LocalConn {
                stream: UnixStream::connect(path).await?,
            }),
            other => Err(io::Error::new(
                io::ErrorKind::Unsupported,
                format!("本平台只支持 Unix domain socket 端点：{other:?}"),
            )),
        }
    }
}

pub struct LocalConn {
    stream: UnixStream,
}

impl Connection for LocalConn {
    type Read = OwnedReadHalf;
    type Write = OwnedWriteHalf;

    async fn peer(&self) -> Result<PeerIdentity, PeerError> {
        // 算映像哈希要读几 MB 的文件，放到阻塞线程池；复制一份 fd，不借用 self 跨线程。
        let fd = self
            .stream
            .as_fd()
            .try_clone_to_owned()
            .map_err(PeerError::Credentials)?;
        tokio::task::spawn_blocking(move || peer::identify_socket_peer(fd.as_fd()))
            .await
            .map_err(|e| PeerError::Credentials(io::Error::other(e)))?
    }

    fn into_split(self) -> (Self::Read, Self::Write) {
        self.stream.into_split()
    }
}
