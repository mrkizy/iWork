//! hook ↔ 桥接的线格式（T06 方案 2.4）。两个方向同一种帧：
//!
//! ```text
//! "IWH1"（4 字节）｜u32 小端长度｜JSON 正文
//! ```
//!
//! 请求在 JSON 头之后再跟一段原样的 stdin：`u32` 小端长度｜字节。一次工具调用一条连接、一问一答。
//! 读的一方先看魔数、再看长度上限、再按长度读满，任何一步不对都是 [`WireError`]，调用方按拒绝处理。

use std::io::{self, Read, Write};

use serde::{Deserialize, Serialize};
use serde_json::Value;

/// 帧的魔数。
pub const MAGIC: [u8; 4] = *b"IWH1";
/// 请求头（JSON）的字节上限。
pub const MAX_HEADER_BYTES: u32 = 4 * 1024;
/// 请求正文（Claude Code 给 hook 的 stdin）与响应的字节上限。同一份工具输入还要放进 ACP 的
/// `tool_call` 更新和权限请求，后台对单条 ACP 消息的上限是 8 MiB，超了会结束整个运行时。
pub const MAX_BODY_BYTES: u32 = 4 * 1024 * 1024;
/// 令牌的字节数（线上是两倍长的小写十六进制）。
pub const TOKEN_BYTES: usize = 32;

/// hook 被拉起的事件。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum HookKind {
    /// 每次工具调用之前：问「能不能做」
    PreToolUse,
    /// 会话开始：向桥接报到（T06 方案 3.5），不问决定
    SessionStart,
}

/// 请求头。令牌只在这里出现，任何日志都不许打印这个结构。
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RequestHeader {
    pub kind: HookKind,
    /// 64 位小写十六进制
    pub token: String,
    /// hook 自己的进程号（桥接可以和连接的对端进程号对照）
    pub pid: u32,
}

impl std::fmt::Debug for RequestHeader {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RequestHeader")
            .field("kind", &self.kind)
            .field("token", &"<redacted>")
            .field("pid", &self.pid)
            .finish()
    }
}

/// 桥接的决定。只有这两个值；别的取值解析失败，hook 按拒绝处理。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Decision {
    Allow,
    Deny,
}

/// 桥接的答复。`updated_input` 只在允许时有意义，必须是 JSON 对象（[`Response::validate`]）。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Response {
    pub decision: Decision,
    /// 回给模型的说明；`session_start` 的答复没有
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub message: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub updated_input: Option<Value>,
}

impl Response {
    pub fn allow() -> Self {
        Self {
            decision: Decision::Allow,
            message: String::new(),
            updated_input: None,
        }
    }

    pub fn deny(message: impl Into<String>) -> Self {
        Self {
            decision: Decision::Deny,
            message: message.into(),
            updated_input: None,
        }
    }

    /// 结构上能解析、语义上也说得通：改写后的输入只许出现在允许里，且必须是 JSON 对象。
    fn validate(self) -> Result<Self, WireError> {
        match (&self.decision, &self.updated_input) {
            (_, None) | (Decision::Allow, Some(Value::Object(_))) => Ok(self),
            _ => Err(WireError::Malformed("updated_input")),
        }
    }
}

/// 线格式错误。说明里不带收到的内容（可能是任何东西）。
#[derive(Debug)]
pub enum WireError {
    Io(io::Error),
    /// 对端在一帧读完之前关了连接
    Eof,
    BadMagic,
    TooLarge {
        what: &'static str,
        limit: u32,
    },
    /// JSON 解析不了，或字段取值不对；参数是哪一部分
    Malformed(&'static str),
}

impl std::fmt::Display for WireError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Io(e) => write!(f, "读写失败：{e}"),
            Self::Eof => f.write_str("对端提前关闭了连接"),
            Self::BadMagic => f.write_str("魔数不对"),
            Self::TooLarge { what, limit } => write!(f, "{what} 超过 {limit} 字节"),
            Self::Malformed(what) => write!(f, "{what} 格式不对"),
        }
    }
}

impl std::error::Error for WireError {}

impl From<io::Error> for WireError {
    fn from(e: io::Error) -> Self {
        if e.kind() == io::ErrorKind::UnexpectedEof {
            Self::Eof
        } else {
            Self::Io(e)
        }
    }
}

fn read_len(r: &mut impl Read, what: &'static str, limit: u32) -> Result<usize, WireError> {
    let mut len = [0u8; 4];
    r.read_exact(&mut len)?;
    let len = u32::from_le_bytes(len);
    if len > limit {
        return Err(WireError::TooLarge { what, limit });
    }
    Ok(len as usize)
}

fn read_exact_vec(r: &mut impl Read, len: usize) -> Result<Vec<u8>, WireError> {
    let mut buf = vec![0u8; len];
    r.read_exact(&mut buf)?;
    Ok(buf)
}

/// 读一帧：魔数、长度（不超过 `limit`）、JSON 正文。
fn read_frame<T: for<'de> Deserialize<'de>>(
    r: &mut impl Read,
    what: &'static str,
    limit: u32,
) -> Result<T, WireError> {
    let mut magic = [0u8; 4];
    r.read_exact(&mut magic)?;
    if magic != MAGIC {
        return Err(WireError::BadMagic);
    }
    let len = read_len(r, what, limit)?;
    let body = read_exact_vec(r, len)?;
    serde_json::from_slice(&body).map_err(|_| WireError::Malformed(what))
}

fn frame(json: &[u8]) -> Vec<u8> {
    let len = u32::try_from(json.len()).expect("调用方已按上限检查过长度");
    let mut out = Vec::with_capacity(8 + json.len());
    out.extend_from_slice(&MAGIC);
    out.extend_from_slice(&len.to_le_bytes());
    out.extend_from_slice(json);
    out
}

/// 写一个请求：头一帧，再跟 stdin 原文。超上限的在写之前就报错。
pub fn write_request(
    w: &mut impl Write,
    header: &RequestHeader,
    body: &[u8],
) -> Result<(), WireError> {
    let json = serde_json::to_vec(header).expect("请求头总能序列化");
    if json.len() > MAX_HEADER_BYTES as usize {
        return Err(WireError::TooLarge {
            what: "请求头",
            limit: MAX_HEADER_BYTES,
        });
    }
    if body.len() > MAX_BODY_BYTES as usize {
        return Err(WireError::TooLarge {
            what: "请求正文",
            limit: MAX_BODY_BYTES,
        });
    }
    let mut out = frame(&json);
    out.extend_from_slice(&(body.len() as u32).to_le_bytes());
    out.extend_from_slice(body);
    w.write_all(&out)?;
    w.flush()?;
    Ok(())
}

/// 读请求头（桥接用）。头读完就可以先验令牌，再决定读不读正文。
pub fn read_request_header(r: &mut impl Read) -> Result<RequestHeader, WireError> {
    read_frame(r, "请求头", MAX_HEADER_BYTES)
}

/// 读请求正文（桥接用，在头之后）。
pub fn read_request_body(r: &mut impl Read) -> Result<Vec<u8>, WireError> {
    let len = read_len(r, "请求正文", MAX_BODY_BYTES)?;
    read_exact_vec(r, len)
}

/// 写一个答复（桥接用）。
pub fn write_response(w: &mut impl Write, response: &Response) -> Result<(), WireError> {
    let json = serde_json::to_vec(response).expect("答复总能序列化");
    if json.len() > MAX_BODY_BYTES as usize {
        return Err(WireError::TooLarge {
            what: "答复",
            limit: MAX_BODY_BYTES,
        });
    }
    w.write_all(&frame(&json))?;
    w.flush()?;
    Ok(())
}

/// 读一个答复（hook 用）：帧不对、JSON 不对、`decision` 取值不认识、改写后的输入不是对象，都是错误。
pub fn read_response(r: &mut impl Read) -> Result<Response, WireError> {
    read_frame::<Response>(r, "答复", MAX_BODY_BYTES)?.validate()
}

/// 令牌的线上写法：64 位小写十六进制。
pub fn token_hex(token: &[u8; TOKEN_BYTES]) -> String {
    token.iter().map(|b| format!("{b:02x}")).collect()
}

/// 解析 64 位十六进制（大小写都认）。长度或字符不对返回 `None`。
pub fn parse_token(text: &str) -> Option<[u8; TOKEN_BYTES]> {
    let bytes = text.as_bytes();
    if bytes.len() != TOKEN_BYTES * 2 {
        return None;
    }
    let nibble = |c: u8| (c as char).to_digit(16).map(|d| d as u8);
    let mut out = [0u8; TOKEN_BYTES];
    for (i, pair) in bytes.as_chunks::<2>().0.iter().enumerate() {
        out[i] = (nibble(pair[0])? << 4) | nibble(pair[1])?;
    }
    Some(out)
}

/// 桥接核对令牌：定长、逐字节比较、不提前返回（方案 2.4 第 4 步）。格式不对直接不符。
pub fn token_matches(expected: &[u8; TOKEN_BYTES], got: &str) -> bool {
    let Some(got) = parse_token(got) else {
        return false;
    };
    let diff = expected
        .iter()
        .zip(got.iter())
        .fold(0u8, |acc, (a, b)| acc | (a ^ b));
    // black_box 防止编译器把整个比较优化成提前返回
    std::hint::black_box(diff) == 0
}

#[cfg(test)]
mod tests {
    use super::*;

    fn header() -> RequestHeader {
        RequestHeader {
            kind: HookKind::PreToolUse,
            token: token_hex(&[0xab; TOKEN_BYTES]),
            pid: 42,
        }
    }

    #[test]
    fn request_round_trips() {
        let mut buf = Vec::new();
        write_request(&mut buf, &header(), br#"{"tool_name":"Bash"}"#).unwrap();
        let mut r = buf.as_slice();
        assert_eq!(read_request_header(&mut r).unwrap(), header());
        assert_eq!(
            read_request_body(&mut r).unwrap(),
            br#"{"tool_name":"Bash"}"#
        );
        assert!(r.is_empty());
    }

    #[test]
    fn response_round_trips_with_and_without_rewrite() {
        for resp in [
            Response::allow(),
            Response::deny("不行"),
            Response {
                decision: Decision::Allow,
                message: String::new(),
                updated_input: Some(serde_json::json!({ "command": "true" })),
            },
        ] {
            let mut buf = Vec::new();
            write_response(&mut buf, &resp).unwrap();
            assert_eq!(read_response(&mut buf.as_slice()).unwrap(), resp);
        }
    }

    fn raw_frame(magic: &[u8], len: u32, body: &[u8]) -> Vec<u8> {
        let mut v = magic.to_vec();
        v.extend_from_slice(&len.to_le_bytes());
        v.extend_from_slice(body);
        v
    }

    /// 方案 4.6 第 8、9 行：魔数、长度、JSON、`decision` 取值、改写后的输入，任何一样不对都是错误。
    #[test]
    fn bad_responses_are_errors() {
        let ok = br#"{"decision":"allow"}"#;
        let cases: Vec<(&str, Vec<u8>)> = vec![
            ("魔数", raw_frame(b"IWH2", ok.len() as u32, ok)),
            ("超长", raw_frame(&MAGIC, MAX_BODY_BYTES + 1, ok)),
            ("截断", raw_frame(&MAGIC, ok.len() as u32 + 5, ok)),
            ("JSON", raw_frame(&MAGIC, 5, b"{oops")),
            ("取值", {
                let b = br#"{"decision":"maybe"}"#;
                raw_frame(&MAGIC, b.len() as u32, b)
            }),
            ("大小写", {
                let b = br#"{"decision":"Allow"}"#;
                raw_frame(&MAGIC, b.len() as u32, b)
            }),
            ("多字段", {
                let b = br#"{"decision":"allow","bypass":true}"#;
                raw_frame(&MAGIC, b.len() as u32, b)
            }),
            ("改写不是对象", {
                let b = br#"{"decision":"allow","updated_input":"rm -rf /"}"#;
                raw_frame(&MAGIC, b.len() as u32, b)
            }),
            ("拒绝带改写", {
                let b = br#"{"decision":"deny","updated_input":{"command":"x"}}"#;
                raw_frame(&MAGIC, b.len() as u32, b)
            }),
            ("空", Vec::new()),
        ];
        for (name, bytes) in cases {
            assert!(read_response(&mut bytes.as_slice()).is_err(), "{name}");
        }
    }

    #[test]
    fn oversize_request_is_refused_before_writing() {
        let mut buf = Vec::new();
        let big = vec![b'x'; MAX_BODY_BYTES as usize + 1];
        assert!(matches!(
            write_request(&mut buf, &header(), &big),
            Err(WireError::TooLarge { .. })
        ));
        assert!(buf.is_empty(), "什么都没写出去");
        // 恰好到上限的可以
        let edge = vec![b'x'; MAX_BODY_BYTES as usize];
        write_request(&mut buf, &header(), &edge).unwrap();
    }

    #[test]
    fn token_hex_round_trips_and_compares() {
        let token = [7u8; TOKEN_BYTES];
        let hex = token_hex(&token);
        assert_eq!(hex.len(), 64);
        assert_eq!(parse_token(&hex), Some(token));
        assert!(token_matches(&token, &hex));
        assert!(token_matches(&token, &hex.to_uppercase()));
        let mut other = token;
        other[31] ^= 1;
        assert!(!token_matches(&token, &token_hex(&other)));
        for bad in ["", "07", &hex[..63], &format!("{hex}0"), &"zz".repeat(32)] {
            assert!(!token_matches(&token, bad), "{bad:?}");
        }
    }

    #[test]
    fn header_debug_never_prints_token() {
        let h = header();
        let shown = format!("{h:?}");
        assert!(!shown.contains(&h.token), "{shown}");
        assert!(shown.contains("redacted"));
    }
}
