//! domain ↔ proto 转换（方案 3.6）。
//!
//! - domain → proto：`*_to_proto`。返回 `Result`：路径不是合法 UTF-8 时 proto 的 string 装不下，
//!   宁可报错也不做有损转换。
//! - proto → domain：`*_from_proto`。在边界上校验一次：ID 解析、哈希十六进制与长度、
//!   枚举值为 UNSPECIFIED 或未知、必填子消息缺失、时间戳精度不是毫秒，都报 [`ConvertError`]
//!   并点名消息与字段。
//!
//! proto 有、domain 没有的字段（投影字段如 `unread`、`Approval.decided_at` 等）：
//! 转 proto 时填默认值（数字 0、可选字段不设），转 domain 时忽略。逐个字段的处理写在
//! 各转换函数的注释里。同一程序写进去的事件解不回来只可能是代码缺陷，
//! 所以 [`ConvertError`] 映射到 `INTERNAL`。

mod event;
mod model;

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::str::FromStr;

use iwork_domain::common::{Sha256, Timestamp};
use iwork_domain::id::IdParseError;
use iwork_proto::pb;

pub use event::{event_from_proto, event_to_proto};
pub use model::*;

/// 转换失败。`kind` 是 proto 消息名，`field` 是字段名。
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ConvertError {
    #[error("{kind}.{field} 缺失")]
    MissingField {
        kind: &'static str,
        field: &'static str,
    },
    #[error("{kind}.{field} 的值 {value} 是未指定或不支持的枚举值")]
    UnsupportedEnum {
        kind: &'static str,
        field: &'static str,
        value: i32,
    },
    #[error("{kind}.{field} 不合法：{reason}")]
    InvalidValue {
        kind: &'static str,
        field: &'static str,
        reason: String,
    },
}

impl ConvertError {
    pub fn kind(&self) -> &'static str {
        match self {
            Self::MissingField { kind, .. }
            | Self::UnsupportedEnum { kind, .. }
            | Self::InvalidValue { kind, .. } => kind,
        }
    }

    pub fn field(&self) -> &'static str {
        match self {
            Self::MissingField { field, .. }
            | Self::UnsupportedEnum { field, .. }
            | Self::InvalidValue { field, .. } => field,
        }
    }

    /// 对外错误：`INTERNAL`、不可重试，`details` 点名消息与字段。
    pub fn to_error(&self) -> pb::Error {
        pb::Error {
            code: pb::ErrorCode::Internal.into(),
            message: self.to_string(),
            retryable: false,
            details: BTreeMap::from([
                (crate::details::KIND.to_owned(), self.kind().to_owned()),
                (crate::details::FIELD.to_owned(), self.field().to_owned()),
            ]),
        }
    }
}

/// 取必填子消息。
pub(crate) fn required<T>(
    kind: &'static str,
    field: &'static str,
    value: Option<T>,
) -> Result<T, ConvertError> {
    value.ok_or(ConvertError::MissingField { kind, field })
}

/// 解析 ID 文本。
pub(crate) fn parse_id<T: FromStr<Err = IdParseError>>(
    kind: &'static str,
    field: &'static str,
    text: &str,
) -> Result<T, ConvertError> {
    text.parse()
        .map_err(|e: IdParseError| ConvertError::InvalidValue {
            kind,
            field,
            reason: e.to_string(),
        })
}

pub(crate) fn parse_opt_id<T: FromStr<Err = IdParseError>>(
    kind: &'static str,
    field: &'static str,
    text: Option<&str>,
) -> Result<Option<T>, ConvertError> {
    text.map(|t| parse_id(kind, field, t)).transpose()
}

pub(crate) fn parse_ids<T: FromStr<Err = IdParseError>>(
    kind: &'static str,
    field: &'static str,
    texts: &[String],
) -> Result<Vec<T>, ConvertError> {
    texts.iter().map(|t| parse_id(kind, field, t)).collect()
}

pub(crate) fn ids_to_strings<T: ToString>(ids: &[T]) -> Vec<String> {
    ids.iter().map(ToString::to_string).collect()
}

/// 路径转 proto string。不是合法 UTF-8 的路径报错，不做有损转换。
pub(crate) fn path_to_proto(
    kind: &'static str,
    field: &'static str,
    path: &Path,
) -> Result<String, ConvertError> {
    path.to_str()
        .map(str::to_owned)
        .ok_or_else(|| ConvertError::InvalidValue {
            kind,
            field,
            reason: format!("路径不是合法 UTF-8：{}", path.display()),
        })
}

pub(crate) fn path_from_proto(text: &str) -> PathBuf {
    PathBuf::from(text)
}

/// SHA-256 转 64 位小写十六进制。
pub(crate) fn sha256_to_proto(hash: &Sha256) -> String {
    hash.to_string()
}

/// 只接受 64 位小写十六进制：同一个哈希只有一种文本，按字符串比较才可靠。
pub(crate) fn sha256_from_hex(
    kind: &'static str,
    field: &'static str,
    text: &str,
) -> Result<Sha256, ConvertError> {
    let invalid = |reason: String| ConvertError::InvalidValue {
        kind,
        field,
        reason,
    };
    if text.len() != 64 {
        return Err(invalid(format!("长度应为 64，实际 {}", text.len())));
    }
    let mut bytes = [0u8; 32];
    for (i, pair) in text.as_bytes().chunks(2).enumerate() {
        let digit = |c: u8| match c {
            b'0'..=b'9' => Some(c - b'0'),
            b'a'..=b'f' => Some(c - b'a' + 10),
            _ => None,
        };
        match (digit(pair[0]), digit(pair[1])) {
            (Some(hi), Some(lo)) => bytes[i] = hi << 4 | lo,
            _ => return Err(invalid(format!("第 {} 个字符起不是小写十六进制", i * 2))),
        }
    }
    Ok(Sha256(bytes))
}

pub(crate) fn sha256_from_bytes(
    kind: &'static str,
    field: &'static str,
    bytes: &[u8],
) -> Result<Sha256, ConvertError> {
    <[u8; 32]>::try_from(bytes)
        .map(Sha256)
        .map_err(|_| ConvertError::InvalidValue {
            kind,
            field,
            reason: format!("长度应为 32 字节，实际 {}", bytes.len()),
        })
}

const NANOS_PER_MILLI: i32 = 1_000_000;

pub(crate) fn timestamp_to_proto(ts: Timestamp) -> prost_types::Timestamp {
    prost_types::Timestamp {
        seconds: ts.0.div_euclid(1000),
        nanos: (ts.0.rem_euclid(1000) as i32) * NANOS_PER_MILLI,
    }
}

/// domain 时间是毫秒；带亚毫秒精度的时间戳不是本程序写的，报错而不截断。
pub(crate) fn timestamp_from_proto(
    kind: &'static str,
    field: &'static str,
    ts: &prost_types::Timestamp,
) -> Result<Timestamp, ConvertError> {
    let invalid = |reason: String| ConvertError::InvalidValue {
        kind,
        field,
        reason,
    };
    if !(0..1_000_000_000).contains(&ts.nanos) || ts.nanos % NANOS_PER_MILLI != 0 {
        return Err(invalid(format!(
            "nanos {} 不是 0–999 毫秒的整数倍",
            ts.nanos
        )));
    }
    ts.seconds
        .checked_mul(1000)
        .and_then(|ms| ms.checked_add(i64::from(ts.nanos / NANOS_PER_MILLI)))
        .map(Timestamp)
        .ok_or_else(|| invalid(format!("seconds {} 溢出", ts.seconds)))
}

pub(crate) fn required_timestamp(
    kind: &'static str,
    field: &'static str,
    ts: Option<&prost_types::Timestamp>,
) -> Result<Timestamp, ConvertError> {
    timestamp_from_proto(kind, field, required(kind, field, ts)?)
}

pub(crate) fn opt_timestamp(
    kind: &'static str,
    field: &'static str,
    ts: Option<&prost_types::Timestamp>,
) -> Result<Option<Timestamp>, ConvertError> {
    ts.map(|t| timestamp_from_proto(kind, field, t)).transpose()
}

/// 为一对无载荷枚举生成两个方向的转换函数。proto 一侧的 UNSPECIFIED、未登记的值、
/// 解不出的整数都报 [`ConvertError::UnsupportedEnum`]。
macro_rules! enum_pair {
    ($to:ident, $from:ident, $domain:ty, $proto:ty, { $($d:path => $p:path),* $(,)? }) => {
        pub fn $to(value: $domain) -> i32 {
            match value {
                $($d => $p as i32),*
            }
        }

        pub fn $from(
            kind: &'static str,
            field: &'static str,
            value: i32,
        ) -> Result<$domain, ConvertError> {
            match <$proto>::try_from(value) {
                $(Ok($p) => Ok($d),)*
                _ => Err(ConvertError::UnsupportedEnum { kind, field, value }),
            }
        }
    };
}
pub(crate) use enum_pair;
