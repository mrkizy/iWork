//! 握手门：主版本不同即拒绝（设计 3.3；T01 方案 3.7「握手门」）。
//!
//! 主版本就是 proto 包名，主版本不同的两端连 `Command` 都不是同一个消息类型，连上只能解出垃圾，
//! 所以必须在处理任何业务帧之前拦下。新旧两端跑的是同一个函数，两个方向（旧客户端 × 新后台、
//! 新客户端 × 旧后台）行为一致。

use crate::pb::{Error, ErrorCode, Hello};

/// 拒绝时 `Error.details` 里客户端主版本的键。
pub const DETAIL_CLIENT_MAJOR: &str = "client_major";
/// 拒绝时 `Error.details` 里后台主版本的键。
pub const DETAIL_SERVER_MAJOR: &str = "server_major";

/// 检查客户端的 `Hello` 能否与主版本为 `server_major` 的后台通信。
///
/// 只比主版本：次版本只做加法，不同次版本照常通过。
/// 不一致时返回 `PROTOCOL_VERSION_MISMATCH`，`details` 带双方主版本号；调用方把它放进
/// `Response{request_id = 0}` 发出后断开，客户端据此显示「正在切换版本」。
pub fn check_hello(hello: &Hello, server_major: u32) -> Result<(), Error> {
    if hello.protocol_major == server_major {
        return Ok(());
    }
    Err(Error {
        code: ErrorCode::ProtocolVersionMismatch.into(),
        message: format!(
            "协议主版本不一致：客户端 v{}，后台服务 v{}，正在切换版本",
            hello.protocol_major, server_major
        ),
        // 版本不一致不会因为重试而消失，要等一端换版本。
        retryable: false,
        details: [
            (
                DETAIL_CLIENT_MAJOR.to_owned(),
                hello.protocol_major.to_string(),
            ),
            (DETAIL_SERVER_MAJOR.to_owned(), server_major.to_string()),
        ]
        .into(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hello(major: u32, minor: u32) -> Hello {
        Hello {
            protocol_major: major,
            protocol_minor: minor,
            ..Hello::default()
        }
    }

    fn assert_mismatch(result: Result<(), Error>, client: &str, server: &str) {
        let err = result.expect_err("主版本不同必须拒绝");
        assert_eq!(err.code(), ErrorCode::ProtocolVersionMismatch);
        assert!(!err.retryable);
        assert_eq!(
            err.details.get(DETAIL_CLIENT_MAJOR).map(String::as_str),
            Some(client)
        );
        assert_eq!(
            err.details.get(DETAIL_SERVER_MAJOR).map(String::as_str),
            Some(server)
        );
    }

    #[test]
    fn old_client_rejected_by_new_server() {
        assert_mismatch(check_hello(&hello(1, 0), 2), "1", "2");
    }

    #[test]
    fn new_client_rejected_by_old_server() {
        assert_mismatch(check_hello(&hello(2, 0), 1), "2", "1");
    }

    #[test]
    fn same_major_different_minor_accepted() {
        assert_eq!(check_hello(&hello(1, 3), 1), Ok(()));
        assert_eq!(check_hello(&hello(1, 0), 1), Ok(()));
    }
}
