//! 审计日志字段的唯一定义处（方案 2.12）。全部走 `tracing` 的 `iwork.ipc.audit` target，级别 `warn`。
//!
//! 字段表是封闭的：这里的函数是唯一的写法，调用方不自己拼字段；**任何函数都不接收消息内容或载荷**。

use std::path::Path;

use crate::peer::{Manifest, PeerIdentity, RejectReason};

/// 审计日志的 tracing target。
pub const TARGET: &str = "iwork.ipc.audit";

/// 校验对端的一方。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Role {
    Server,
    Client,
}

impl Role {
    fn as_str(self) -> &'static str {
        match self {
            Role::Server => "server",
            Role::Client => "client",
        }
    }
}

pub(crate) fn peer_rejected(role: Role, peer: &PeerIdentity, reason: &RejectReason) {
    tracing::warn!(
        target: TARGET,
        event = "peer_rejected",
        role = role.as_str(),
        pid = peer.pid,
        user = %peer.user,
        exe = %peer.exe.as_deref().map(Path::display).map(|d| d.to_string()).unwrap_or_default(),
        reason = %reason,
    );
}

#[cfg(feature = "dev")]
pub(crate) fn peer_accepted_without_signature_check(role: Role, peer: &PeerIdentity) {
    tracing::warn!(
        target: TARGET,
        event = "peer_accepted_without_signature_check",
        role = role.as_str(),
        pid = peer.pid,
        user = %peer.user,
        exe = %peer.exe.as_deref().map(Path::display).map(|d| d.to_string()).unwrap_or_default(),
    );
}

/// 取对端身份本身失败（拿不到 uid）。按拒绝处理。
pub(crate) fn peer_identity_failed(role: Role, error: &dyn std::fmt::Display) {
    tracing::warn!(target: TARGET, event = "peer_rejected", role = role.as_str(), reason = %format_args!("Unavailable: {error}"));
}

pub(crate) fn manifest_loaded(path: &Path, manifest: &Manifest, self_listed: bool) {
    if self_listed {
        tracing::info!(target: TARGET, event = "manifest_loaded", path = %path.display(), app_version = %manifest.app_version, entries = manifest.entries(), self_listed);
    } else {
        tracing::warn!(target: TARGET, event = "manifest_loaded", path = %path.display(), app_version = %manifest.app_version, entries = manifest.entries(), self_listed);
    }
}

/// 协议违规。`phase` 是 `handshake`（握手前）或 `established`。不记帧内容。
pub(crate) fn protocol_violation(
    connection_id: u64,
    pid: u32,
    frame_type: &str,
    reason: &str,
    phase: &str,
) {
    tracing::warn!(target: TARGET, event = "protocol_violation", connection_id, pid, frame_type, reason, phase);
}

/// 连接被 `fail_connection` 关闭。后台自身问题（交接超时、发布停滞、存储出错）另记一条 `error`。
pub(crate) fn connection_failed(
    connection_id: u64,
    pid: u32,
    cause: crate::server::FailCause,
    cursor: u64,
    used_bytes: usize,
    in_flight: &str,
) {
    if cause == crate::server::FailCause::PeerClosed {
        tracing::debug!(target: TARGET, event = "connection_failed", connection_id, pid, cause = cause.as_str(), cursor, used_bytes, in_flight);
        return;
    }
    tracing::warn!(target: TARGET, event = "connection_failed", connection_id, pid, cause = cause.as_str(), cursor, used_bytes, in_flight);
    if cause.is_server_fault() {
        tracing::error!(target: TARGET, event = "connection_failed", connection_id, pid, cause = cause.as_str(), cursor, used_bytes, in_flight);
    }
}
