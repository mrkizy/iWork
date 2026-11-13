//! 客户端日志里怎么写会带用户内容的东西（客户端日志落盘到 `client.log`，ADR-0013）。
//!
//! 只记种类、错误码：后台给的说明文字按协议可能带路径（`common.proto` 的 `Error.message`），查询用途里有用户的搜索词，
//! 回包里有消息正文。这些一律不进日志字段；要写进日志的错误、回包、查询用途，都经这里取一个不带内容的名字。

use iwork_ipc::client::ClientError;
use iwork_proto::pb;

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
}
