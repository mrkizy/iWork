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

/// 后台自己固定下来的几个编号：时间戳为 0，随机部分以「IWORK」五个字节开头，和生成器给的编号（带当时的毫秒数）
/// 撞不上。
const fn fixed(n: u128) -> RawUlid {
    RawUlid::from_parts(0, (0x49_57_4F_52_4B << 40) | n)
}

impl AssistantId {
    /// 内置助手 iWork 的编号（T10a 方案 2.8 第 1a 步）。全仓只有这一个助手用它；判「有没有内置助手」看
    /// `Assistant.builtin`，不看编号。
    pub const BUILTIN_IWORK: Self = Self::from_raw(fixed(1));
}

impl RuntimeId {
    /// M1 唯一的运行时的编号（2.8 第 1 步）。不管探测到没有，启动时都提交这一行运行时；内置助手的运行时字段填它。
    pub const DEFAULT_RUNTIME: Self = Self::from_raw(fixed(2));
}

impl CommandId {
    /// 由名字空间与一个编号算出的命令编号（FNV-1a 128 位）：后台替同一件事提交时用它，重复提交只会得到
    /// `replayed`（内置助手的登记、删除的最后一笔、通知的定夺）。同样的输入永远是同一个编号，跨版本不变。
    pub fn derived(namespace: &str, id: RawUlid) -> Self {
        const OFFSET: u128 = 0x6c62_272e_07bb_0142_62b8_2175_6295_c58d;
        const PRIME: u128 = 0x0000_0000_0100_0000_0000_0000_0000_013b;
        let mut h = OFFSET;
        for b in namespace
            .as_bytes()
            .iter()
            .chain(&[0u8])
            .chain(&id.to_u128().to_be_bytes())
        {
            h ^= u128::from(*b);
            h = h.wrapping_mul(PRIME);
        }
        Self::from_raw(RawUlid::from_u128(h))
    }
}

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

#[cfg(test)]
mod derived_tests {
    use super::*;

    /// 同样的输入同一个编号；名字空间或编号不同就不同。期望值写死：算法要是变了，旧库里落过盘的那一笔就认不出来了。
    #[test]
    fn derived_is_stable_and_separates_inputs() {
        let a = CommandId::derived("builtin-assistant", AssistantId::BUILTIN_IWORK.to_raw());
        assert_eq!(
            a,
            CommandId::derived("builtin-assistant", AssistantId::BUILTIN_IWORK.to_raw())
        );
        assert_ne!(
            a,
            CommandId::derived("delete", AssistantId::BUILTIN_IWORK.to_raw())
        );
        assert_ne!(
            a,
            CommandId::derived("builtin-assistant", RuntimeId::DEFAULT_RUNTIME.to_raw())
        );
        assert_eq!(
            CommandId::derived("x", RawUlid::from_u128(0))
                .to_raw()
                .to_u128(),
            DERIVED_X_ZERO
        );
    }
    // 用 Python 照 FNV-1a 128 独立算的值
    const DERIVED_X_ZERO: u128 = 0xf9af_5675_4a3f_dcf3_89ed_c9ca_f0f7_8f2d;
}
