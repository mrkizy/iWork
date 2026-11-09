//! 实体 ID：ULID 强类型 newtype 与事件序号。
//!
//! 每种实体一个 newtype，把 `ProjectId` 传进要 `ConversationId` 的地方在编译期就报错。
//! ULID 按时间有序，按 ID 排序即按创建顺序，键集分页不需要额外的时间索引。
//! domain 不生成 ID（不读时钟、不取随机数）：生成器是 `iwork-ports::IdGenerator`，
//! 它产出 [`RawUlid`]，再由各 ID 的 `from_raw` 包成强类型。

use core::fmt;
use core::str::FromStr;

use ulid::Ulid;

/// `IdGenerator` 端口产出的原始 128 位 ULID（高 48 位毫秒时间戳，低 80 位随机部分）。
///
/// 端口实现（daemon）要保证单调：同一毫秒内用 [`RawUlid::increment`] 递增随机部分，
/// 时钟回拨时沿用上一次的毫秒数继续递增。domain 自己不对单调性做任何假设。
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct RawUlid(u128);

impl RawUlid {
    /// 由毫秒时间戳与随机部分拼出 ULID；时间戳超出 48 位、随机部分超出 80 位的高位被截掉。
    pub const fn from_parts(timestamp_ms: u64, random: u128) -> Self {
        Self(Ulid::from_parts(timestamp_ms, random).0)
    }

    pub const fn from_u128(value: u128) -> Self {
        Self(value)
    }

    pub const fn to_u128(self) -> u128 {
        self.0
    }

    pub const fn timestamp_ms(self) -> u64 {
        Ulid(self.0).timestamp_ms()
    }

    pub const fn random(self) -> u128 {
        Ulid(self.0).random()
    }

    /// 随机部分加一、时间戳不变；随机部分已到最大值时返回 `None`（调用方应等到下一毫秒）。
    pub const fn increment(self) -> Option<Self> {
        match Ulid(self.0).increment() {
            Ok(next) => Some(Self(next.0)),
            Err(_) => None,
        }
    }
}

/// 字符串解析成 ID 失败。`kind` 点名是哪种 ID，`input` 是原文，读日志的人能直接定位。
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("无法解析 {kind} ID {input:?}：{reason}")]
pub struct IdParseError {
    pub kind: &'static str,
    pub input: String,
    pub reason: IdParseReason,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IdParseReason {
    /// 长度不是 26
    InvalidLength,
    /// 含 Crockford base32 以外的字符
    InvalidChar,
    /// 能解码但不是规范写法（小写，或首字符超过 7 导致高位溢出）。
    /// 存储与查询按字符串比较 ID，非规范写法会让同一个 ID 有两种文本，所以拒收。
    NotCanonical,
}

impl fmt::Display for IdParseReason {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::InvalidLength => "长度必须是 26 个字符",
            Self::InvalidChar => "含 Crockford base32 以外的字符",
            Self::NotCanonical => "不是规范写法（须为大写，且首字符不超过 7）",
        })
    }
}

/// 解析 26 字符规范 ULID 文本，失败时带上 ID 种类与原文。
fn parse_ulid(kind: &'static str, input: &str) -> Result<Ulid, IdParseError> {
    let fail = |reason| IdParseError {
        kind,
        input: input.to_owned(),
        reason,
    };
    let ulid = Ulid::from_string(input).map_err(|e| {
        fail(match e {
            ulid::DecodeError::InvalidLength => IdParseReason::InvalidLength,
            ulid::DecodeError::InvalidChar => IdParseReason::InvalidChar,
        })
    })?;
    let mut buf = [0u8; ulid::ULID_LEN];
    if ulid.array_to_str(&mut buf) != input {
        return Err(fail(IdParseReason::NotCanonical));
    }
    Ok(ulid)
}

macro_rules! define_id {
    ($(#[$meta:meta])* $name:ident, $kind:literal) => {
        $(#[$meta])*
        #[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
        pub struct $name(Ulid);

        impl $name {
            /// ID 种类名，用于错误信息与日志。
            pub const KIND: &'static str = $kind;

            /// 把 `IdGenerator` 端口产出的原始值包成本类型。
            pub const fn from_raw(raw: RawUlid) -> Self {
                Self(Ulid(raw.0))
            }

            pub const fn to_raw(self) -> RawUlid {
                RawUlid(self.0 .0)
            }
        }

        /// 26 字符规范 ULID 文本（大写 Crockford base32）。
        impl fmt::Display for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                fmt::Display::fmt(&self.0, f)
            }
        }

        impl fmt::Debug for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                write!(f, concat!(stringify!($name), "({})"), self.0)
            }
        }

        /// 只接受 26 字符规范写法；失败返回点名种类与原文的 [`IdParseError`]。
        impl FromStr for $name {
            type Err = IdParseError;
            fn from_str(s: &str) -> Result<Self, Self::Err> {
                parse_ulid($kind, s).map(Self)
            }
        }
    };
}

define_id!(
    /// 项目。也写在仓库的 `.iwork/project.toml` 里，路径变了靠它重新关联。
    ProjectId,
    "project"
);
define_id!(
    /// 助手（含内置的 iWork）。
    AssistantId,
    "assistant"
);
define_id!(
    /// 对话（项目对话或助手私聊）。
    ConversationId,
    "conversation"
);
define_id!(
    /// 消息。
    MessageId,
    "message"
);
define_id!(
    /// 回合：一次 prompt 到结束。
    TurnId,
    "turn"
);
define_id!(
    /// 审批请求。
    ApprovalId,
    "approval"
);
define_id!(
    /// 亲手执行卡片。
    UserActionId,
    "user_action"
);
define_id!(
    /// 记住的规则或内置规则条目。
    RuleId,
    "rule"
);
define_id!(
    /// 附件。
    AttachmentId,
    "attachment"
);
define_id!(
    /// 本机检测到的一个运行时（Agent CLI）。
    RuntimeId,
    "runtime"
);
define_id!(
    /// 命令的幂等键，客户端生成。
    CommandId,
    "command"
);
define_id!(
    /// 后台作业（克隆、草稿等）。
    JobId,
    "job"
);

/// 一次 AI 审查的身份。
///
/// AI 审查是一次系统回合（技术设计 7.2b），所以审查身份就是那个回合的 [`TurnId`]；
/// 单独包一层是为了让「审查身份」与「被审批的回合」在类型上不能互换。
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct ReviewId(pub TurnId);

impl fmt::Display for ReviewId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Display::fmt(&self.0, f)
    }
}

/// 事件日志的全局序号，从 1 开始单调递增；0 表示「还没有任何事件」。
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Default)]
pub struct Seq(pub u64);

impl fmt::Display for Seq {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Display::fmt(&self.0, f)
    }
}
