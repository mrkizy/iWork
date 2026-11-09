//! 占位：平台实现待填（方案 2.4、2.4b）。映像绑定未被实测证明前，对端一律 Unavailable。

use std::os::fd::BorrowedFd;
use std::path::PathBuf;

use super::{PeerError, PeerIdentity, SignerIdentity, UserId};

pub(crate) fn current_user() -> Result<UserId, PeerError> {
    Ok(UserId::Uid(unsafe { libc::geteuid() }))
}

pub(crate) fn own_image() -> Result<(SignerIdentity, PathBuf), PeerError> {
    Err(PeerError::SelfIdentity("未实现".into()))
}

pub(crate) fn identify_socket_peer(_fd: BorrowedFd<'_>) -> Result<PeerIdentity, PeerError> {
    Err(PeerError::SelfIdentity("未实现".into()))
}
