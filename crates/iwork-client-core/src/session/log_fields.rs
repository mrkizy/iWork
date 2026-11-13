//! 客户端日志里怎么写会带用户内容的东西（客户端日志落盘到 `client.log`，ADR-0013）。
//!
//! 只记种类、错误码：后台给的说明文字按协议可能带路径（`common.proto` 的 `Error.message`），查询用途里有用户的搜索词，
//! 回包里有消息正文，握手被拒时 `ConnectError::Protocol` 装的就是后台的说明文字。这些一律不进日志字段；要写进日志的
//! 错误、回包、查询用途、断开原因，都经这里取一个不带内容的名字。错误枚举不整个 Debug：里面可能包着后台的文字。
//! 完整说明照旧给界面（诊断页、意图回执），不经这里。

use iwork_ipc::client::{ClientError, CloseReason, ConnectError};
use iwork_proto::pb;

use super::dial::DialError;

/// 错误码的小写名（`ERROR_CODE_NOT_FOUND` → `not_found`）；不认识的码写成 `code_<数字>`。界面与日志共用这一个写法。
pub(crate) fn error_code_name(code: i32) -> String {
    pb::ErrorCode::try_from(code)
        .map(|c| {
            c.as_str_name()
                .trim_start_matches("ERROR_CODE_")
                .to_ascii_lowercase()
        })
        .unwrap_or_else(|_| format!("code_{code}"))
}

/// 一个 ipc 调用错误的种类：后台报错只记错误码，不记说明文字。
pub(crate) fn client_error(e: &ClientError) -> String {
    match e {
        ClientError::Closed => "closed".into(),
        ClientError::Server(s) => format!("server:{}", error_code_name(s.code)),
        ClientError::WrongLane => "wrong_lane".into(),
        ClientError::ChunkTooLarge => "chunk_too_large".into(),
        ClientError::UnexpectedResponse => "unexpected_response".into(),
    }
}

/// 建连失败的种类：只带错误码一类的结构化值（对端被拒的原因种类、两端主版本、io 错误种类），不带任何说明文字。
pub(crate) fn connect_error(e: &ConnectError) -> String {
    match e {
        ConnectError::Io(io) => format!("io:{:?}", io.kind()),
        ConnectError::PeerRejected(r) => format!("peer_rejected:{}", r.kind()),
        ConnectError::ClosedBeforeWelcome => "closed_before_welcome".into(),
        ConnectError::VersionMismatch {
            client_major,
            server_major,
        } => format!("version_mismatch:client={client_major},server={server_major}"),
        ConnectError::Timeout => "timeout".into(),
        ConnectError::Protocol(_) => "protocol".into(),
    }
}

/// 拨号失败的种类：建连错误按 [`connect_error`]；后台没就绪记停在哪一步；本机环境问题只记种类（说明里有路径）。
pub(crate) fn dial_error(e: &DialError) -> String {
    match e {
        DialError::Connect(c) => format!("connect:{}", connect_error(c)),
        DialError::NotReady { stage, .. } => format!("not_ready:{stage}"),
        DialError::Environment(_) => "environment".into(),
    }
}

/// 连接为什么断了：只记种类（io 与解码错误的文字不进日志）。
pub(crate) fn close_reason(r: &CloseReason) -> &'static str {
    match r {
        CloseReason::PeerClosed => "peer_closed",
        CloseReason::Io(_) => "io",
        CloseReason::Protocol(_) => "protocol",
    }
}

/// 回包是哪一种：不记回包内容。
pub(crate) fn response(body: Option<&pb::response::Body>) -> String {
    match body {
        None => "empty".into(),
        Some(pb::response::Body::Accepted(_)) => "accepted".into(),
        Some(pb::response::Body::Result(_)) => "result".into(),
        Some(pb::response::Body::Subscribed(_)) => "subscribed".into(),
        Some(pb::response::Body::Error(e)) => format!("error:{}", error_code_name(e.code)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_carry_no_message_text() {
        let e = pb::Error {
            code: pb::ErrorCode::NotFound as i32,
            message: "SECRET /Users/me/project".into(),
            retryable: false,
            details: Default::default(),
        };
        assert_eq!(
            client_error(&ClientError::Server(e.clone())),
            "server:not_found"
        );
        let body = pb::response::Body::Error(e);
        assert_eq!(response(Some(&body)), "error:not_found");
        assert_eq!(error_code_name(9999), "code_9999");
    }

    #[test]
    fn dial_errors_carry_no_server_text() {
        let protocol = DialError::Connect(ConnectError::Protocol("SECRET /Users/me/x".into()));
        assert_eq!(dial_error(&protocol), "connect:protocol");
        let env = DialError::Environment("SECRET /Users/me/x".into());
        assert_eq!(dial_error(&env), "environment");
        let mismatch = DialError::Connect(ConnectError::VersionMismatch {
            client_major: 1,
            server_major: 2,
        });
        assert_eq!(
            dial_error(&mismatch),
            "connect:version_mismatch:client=1,server=2"
        );
        assert_eq!(
            close_reason(&CloseReason::Protocol("SECRET".into())),
            "protocol"
        );
    }
}
