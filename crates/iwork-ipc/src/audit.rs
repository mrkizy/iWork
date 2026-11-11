//! 审计日志字段的唯一定义处（方案 2.12）。全部走 `tracing` 的 `iwork.ipc.audit` target，级别 `warn`。
//!
//! 字段表是封闭的：这里的函数是唯一的写法，调用方不自己拼字段；**任何函数都不接收消息内容或载荷**。

use std::path::Path;

use crate::peer::{Manifest, PeerIdentity, RejectReason};

/// 审计日志的 tracing target。
pub const TARGET: &str = "iwork.ipc.audit";

// 审计事件名（字段 `event` 的值）。写日志与读日志的一方（T04 的日志旁路按它过滤）都用这里的常量。
/// 拒绝了对端（含取对端身份失败）。
pub const EVENT_PEER_REJECTED: &str = "peer_rejected";
/// dev 构建 `--dev-trust` 下跳过签名校验放行了对端。
pub const EVENT_PEER_ACCEPTED_WITHOUT_SIGNATURE_CHECK: &str =
    "peer_accepted_without_signature_check";
/// 启动时读到安装清单。
pub const EVENT_MANIFEST_LOADED: &str = "manifest_loaded";
/// 启动时读不到安装清单。
pub const EVENT_MANIFEST_UNAVAILABLE: &str = "manifest_unavailable";
/// 对端违反协议。
pub const EVENT_PROTOCOL_VIOLATION: &str = "protocol_violation";
/// 连接被关闭；`cause` 是后台自身问题时另有一条 `error` 级别的同名事件。
pub const EVENT_CONNECTION_FAILED: &str = "connection_failed";

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
        event = EVENT_PEER_REJECTED,
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
        event = EVENT_PEER_ACCEPTED_WITHOUT_SIGNATURE_CHECK,
        role = role.as_str(),
        pid = peer.pid,
        user = %peer.user,
        exe = %peer.exe.as_deref().map(Path::display).map(|d| d.to_string()).unwrap_or_default(),
    );
}

/// 取对端身份本身失败（拿不到 uid）。按拒绝处理。
pub(crate) fn peer_identity_failed(role: Role, error: &dyn std::fmt::Display) {
    tracing::warn!(target: TARGET, event = EVENT_PEER_REJECTED, role = role.as_str(), reason = %format_args!("Unavailable: {error}"));
}

pub(crate) fn manifest_loaded(path: &Path, manifest: &Manifest, self_listed: bool) {
    if self_listed {
        tracing::info!(target: TARGET, event = EVENT_MANIFEST_LOADED, path = %path.display(), app_version = %manifest.app_version, entries = manifest.entries(), self_listed);
    } else {
        tracing::warn!(target: TARGET, event = EVENT_MANIFEST_LOADED, path = %path.display(), app_version = %manifest.app_version, entries = manifest.entries(), self_listed);
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
    tracing::warn!(target: TARGET, event = EVENT_PROTOCOL_VIOLATION, connection_id, pid, frame_type, reason, phase);
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
        tracing::debug!(target: TARGET, event = EVENT_CONNECTION_FAILED, connection_id, pid, cause = cause.as_str(), cursor, used_bytes, in_flight);
        return;
    }
    tracing::warn!(target: TARGET, event = EVENT_CONNECTION_FAILED, connection_id, pid, cause = cause.as_str(), cursor, used_bytes, in_flight);
    if cause.is_server_fault() {
        tracing::error!(target: TARGET, event = EVENT_CONNECTION_FAILED, connection_id, pid, cause = cause.as_str(), cursor, used_bytes, in_flight);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 事件名是对外的日志格式：常量化之后输出的字样必须和原来逐字相同。
    #[test]
    fn event_names_are_unchanged() {
        assert_eq!(TARGET, "iwork.ipc.audit");
        assert_eq!(EVENT_PEER_REJECTED, "peer_rejected");
        assert_eq!(
            EVENT_PEER_ACCEPTED_WITHOUT_SIGNATURE_CHECK,
            "peer_accepted_without_signature_check"
        );
        assert_eq!(EVENT_MANIFEST_LOADED, "manifest_loaded");
        assert_eq!(EVENT_MANIFEST_UNAVAILABLE, "manifest_unavailable");
        assert_eq!(EVENT_PROTOCOL_VIOLATION, "protocol_violation");
        assert_eq!(EVENT_CONNECTION_FAILED, "connection_failed");
    }
}
