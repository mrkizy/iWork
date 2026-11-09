//! SQL 助手：枚举 ↔ 列文本的登记表、ID / 哈希 / 时间列的转换、SQLite 错误映射、
//! 「UPDATE 必须命中一行」的检查。
//!
//! 每个落列的枚举在 [`column_text!`] 里写一张「变体 ↔ 文本」表：`to_text` 是穷举 `match`，
//! 加变体忘了加文本编译不过；`from_text` 由同一张表生成，两个方向不会对不上。

use std::str::FromStr;

use iwork_domain::approval::{Decider, ReviewPhase, VoidReason};
use iwork_domain::approval_mode::{ApprovalMode, ProjectDefaultMode};
use iwork_domain::common::{ArtifactArea, Sha256};
use iwork_domain::conversation::ConversationState;
use iwork_domain::event::{DraftDecision, StreamKind};
use iwork_domain::id::{IdParseError, Seq};
use iwork_domain::message::MessageStatus;
use iwork_domain::project::PathStatus;
use iwork_domain::rule::RuleLayer;
use iwork_domain::runtime::{LocalConfigKind, RuntimeState};
use iwork_domain::turn::{InterruptReason, TurnState};
use iwork_domain::user_action::UserActionStateKind;
use iwork_ports::store::{DeliveryState, StorageError};
use rusqlite::ErrorCode;

/// 一个能落进 TEXT 列的枚举。
pub(crate) trait ColumnText: Sized + Copy + 'static {
    /// 类型名，出现在「列里的值认不出来」的错误里。
    const TYPE: &'static str;
    fn to_text(self) -> &'static str;
    fn from_text(text: &str) -> Option<Self>;
}

macro_rules! column_text {
    // 变体写在括号里：同一串记号在 to_text 里当模式、在 from_text 里当表达式
    ($ty:ty, $name:literal { $(($($variant:tt)+) => $text:literal),+ $(,)? }) => {
        impl ColumnText for $ty {
            const TYPE: &'static str = $name;
            fn to_text(self) -> &'static str {
                match self { $($($variant)+ => $text),+ }
            }
            fn from_text(text: &str) -> Option<Self> {
                match text { $($text => Some($($variant)+),)+ _ => None }
            }
        }
    };
}

column_text!(PathStatus, "PathStatus" {
    (PathStatus::Ok) => "ok",
    (PathStatus::Missing) => "missing",
});
column_text!(ProjectDefaultMode, "ProjectDefaultMode" {
    (ProjectDefaultMode::Manual) => "manual",
    (ProjectDefaultMode::Auto) => "auto",
});
column_text!(ApprovalMode, "ApprovalMode" {
    (ApprovalMode::Manual) => "manual",
    (ApprovalMode::Auto) => "auto",
    (ApprovalMode::FullAccess) => "full_access",
});
column_text!(ConversationState, "ConversationState" {
    (ConversationState::Active) => "active",
    (ConversationState::Deleting) => "deleting",
    (ConversationState::Deleted) => "deleted",
});
column_text!(MessageStatus, "MessageStatus" {
    (MessageStatus::Sent) => "sent",
    (MessageStatus::Streaming) => "streaming",
    (MessageStatus::Completed) => "completed",
    (MessageStatus::Interrupted) => "interrupted",
    (MessageStatus::Failed) => "failed",
});
column_text!(TurnState, "TurnState" {
    (TurnState::Queued) => "queued",
    (TurnState::Running) => "running",
    (TurnState::WaitingApproval) => "waiting_approval",
    (TurnState::Cancelling) => "cancelling",
    (TurnState::Interrupted { reason: InterruptReason::Crash }) => "interrupted_crash",
    (TurnState::Interrupted { reason: InterruptReason::ApprovalWaitLimit }) => "interrupted_approval_wait_limit",
    (TurnState::Completed) => "completed",
    (TurnState::Cancelled) => "cancelled",
    (TurnState::Failed) => "failed",
});
column_text!(Decider, "Decider" {
    (Decider::User) => "user",
    (Decider::ModeSwitch) => "mode_switch",
    (Decider::AiReview) => "ai_review",
});
column_text!(VoidReason, "VoidReason" {
    (VoidReason::TurnCancelled) => "turn_cancelled",
    (VoidReason::ConversationDeleted) => "conversation_deleted",
    (VoidReason::TurnAbandonedAfterInterrupt) => "turn_abandoned_after_interrupt",
    (VoidReason::AssistantDeleted) => "assistant_deleted",
});
column_text!(ReviewPhase, "ReviewPhase" {
    (ReviewPhase::NotReviewed) => "not_reviewed",
    (ReviewPhase::InProgress) => "in_progress",
    (ReviewPhase::Passed) => "passed",
    (ReviewPhase::Failed) => "failed",
    (ReviewPhase::Errored) => "errored",
    (ReviewPhase::TimedOut) => "timed_out",
    (ReviewPhase::Abandoned) => "abandoned",
});
column_text!(UserActionStateKind, "UserActionStateKind" {
    (UserActionStateKind::Pending) => "pending",
    (UserActionStateKind::Starting) => "starting",
    (UserActionStateKind::Executing) => "executing",
    (UserActionStateKind::Stopping) => "stopping",
    (UserActionStateKind::Done) => "done",
    (UserActionStateKind::Failed) => "failed",
    (UserActionStateKind::Unknown) => "unknown",
    (UserActionStateKind::Ended) => "ended",
    (UserActionStateKind::Dismissed) => "dismissed",
    (UserActionStateKind::Voided) => "voided",
});
column_text!(RuleLayer, "RuleLayer" {
    (RuleLayer::Baseline) => "baseline",
    (RuleLayer::DenialMemory) => "denial_memory",
    (RuleLayer::ToolConfig) => "tool_config",
    (RuleLayer::UserDeny) => "user_deny",
    (RuleLayer::MustAsk) => "must_ask",
    (RuleLayer::Remembered) => "remembered",
    (RuleLayer::ProjectAllow) => "project_allow",
    (RuleLayer::Default) => "default",
    (RuleLayer::CredentialCommands) => "credential_commands",
    (RuleLayer::AutorunConfig) => "autorun_config",
    (RuleLayer::SensitiveDirs) => "sensitive_dirs",
    (RuleLayer::WritableCache) => "writable_cache",
    (RuleLayer::WritableDirs) => "writable_dirs",
    (RuleLayer::RejectList) => "reject_list",
});
column_text!(RuntimeState, "RuntimeState" {
    (RuntimeState::Missing) => "missing",
    (RuntimeState::NotLoggedIn) => "not_logged_in",
    (RuntimeState::ReadyVerified) => "ready_verified",
    (RuntimeState::ReadyUnverified) => "ready_unverified",
    (RuntimeState::Incompatible) => "incompatible",
});
column_text!(LocalConfigKind, "LocalConfigKind" {
    (LocalConfigKind::Mcp) => "mcp",
    (LocalConfigKind::Skill) => "skill",
    (LocalConfigKind::Plugin) => "plugin",
});
column_text!(ArtifactArea, "ArtifactArea" {
    (ArtifactArea::Artifacts) => "artifacts",
    (ArtifactArea::Attachments) => "attachments",
    (ArtifactArea::Private) => "private",
});
column_text!(DeliveryState, "DeliveryState" {
    (DeliveryState::Queued) => "queued",
    (DeliveryState::Dispatched) => "dispatched",
    (DeliveryState::AcceptedByRuntime) => "accepted_by_runtime",
    (DeliveryState::Failed) => "failed",
    (DeliveryState::Expired) => "expired",
    (DeliveryState::Cancelled) => "cancelled",
});
column_text!(DraftDecision, "DraftDecision" {
    (DraftDecision::Accept) => "accept",
    (DraftDecision::Discard) => "discard",
});
column_text!(StreamKind, "StreamKind" {
    (StreamKind::Project) => "project",
    (StreamKind::Assistant) => "assistant",
    (StreamKind::Conversation) => "conversation",
    (StreamKind::Turn) => "turn",
    (StreamKind::Approval) => "approval",
    (StreamKind::Audit) => "audit",
    (StreamKind::Rule) => "rule",
    (StreamKind::Runtime) => "runtime",
    (StreamKind::Settings) => "settings",
    (StreamKind::System) => "system",
});

/// 读出的列值不合法：点名表、列与原值。只可能是库被手改或代码缺陷，不可重试。
pub(crate) fn bad_column(table: &str, column: &str, value: impl std::fmt::Display) -> StorageError {
    StorageError::Io {
        message: format!(
            "表 {table} 列 {column} 的值 {value:?} 不合法",
            value = value.to_string()
        ),
    }
}

/// 把列文本解析成枚举；认不出来时点名表与列。
pub(crate) fn enum_col<T: ColumnText>(
    table: &str,
    column: &str,
    text: &str,
) -> Result<T, StorageError> {
    T::from_text(text).ok_or_else(|| bad_column(table, column, format!("{text}（{}）", T::TYPE)))
}

/// 把列文本解析成强类型 ID；不是规范 ULID 时点名表与列。
pub(crate) fn id_col<T: FromStr<Err = IdParseError>>(
    table: &str,
    column: &str,
    text: &str,
) -> Result<T, StorageError> {
    text.parse()
        .map_err(|e: IdParseError| bad_column(table, column, e))
}

/// 64 位小写十六进制（与 `Sha256` 的 `Display` 相同）。
pub(crate) fn sha_text(sha: &Sha256) -> String {
    sha.to_string()
}

pub(crate) fn sha_col(table: &str, column: &str, text: &str) -> Result<Sha256, StorageError> {
    parse_sha(text).ok_or_else(|| bad_column(table, column, text))
}

/// 解析 64 位小写十六进制；长度或字符不对返回 `None`。
pub(crate) fn parse_sha(text: &str) -> Option<Sha256> {
    let bytes = text.as_bytes();
    if bytes.len() != 64 {
        return None;
    }
    let nibble = |c: u8| match c {
        b'0'..=b'9' => Some(c - b'0'),
        b'a'..=b'f' => Some(c - b'a' + 10),
        _ => None,
    };
    let mut out = [0u8; 32];
    for (i, [hi, lo]) in bytes.as_chunks::<2>().0.iter().enumerate() {
        out[i] = nibble(*hi)? << 4 | nibble(*lo)?;
    }
    Some(Sha256(out))
}

/// `Seq` ↔ INTEGER。SQLite 的整数是有符号 64 位；seq 从 1 递增，远到不了上限。
pub(crate) fn seq_val(seq: Seq) -> i64 {
    i64::try_from(seq.0).expect("seq 超出 i64")
}

pub(crate) fn seq_col(value: i64) -> Seq {
    Seq(u64::try_from(value).expect("库里的 seq 是负数"))
}

/// 无符号计数 ↔ INTEGER（大小、token 数）。超过 i64 的值在 SQLite 里存不下，按错误报。
pub(crate) fn u64_val(what: &'static str, value: u64) -> Result<i64, StorageError> {
    i64::try_from(value).map_err(|_| StorageError::Io {
        message: format!("{what} 的值 {value} 超出 SQLite 整数范围"),
    })
}

pub(crate) fn u64_col(table: &str, column: &str, value: i64) -> Result<u64, StorageError> {
    u64::try_from(value).map_err(|_| bad_column(table, column, value))
}

/// SQLite 错误 → 存储错误。锁等不到映射成可重试的 `Busy`，其余都是 `Io`。
pub(crate) fn sql_err(err: rusqlite::Error) -> StorageError {
    match err.sqlite_error_code() {
        Some(ErrorCode::DatabaseBusy | ErrorCode::DatabaseLocked) => StorageError::Busy,
        _ => StorageError::Io {
            message: err.to_string(),
        },
    }
}

/// `UPDATE` / `DELETE` 必须命中一行：0 行说明目标实体不存在，是 app 层逻辑错误，不吞（方案 2.6）。
pub(crate) fn expect_hit(
    changed: usize,
    seq: Seq,
    table: &str,
    id: impl std::fmt::Display,
) -> Result<(), StorageError> {
    if changed == 0 {
        Err(StorageError::Projection {
            seq,
            message: format!("{table} 里没有 {id}"),
        })
    } else {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use iwork_domain::machine::Finite;

    fn roundtrip_all<T: ColumnText + Finite + PartialEq + std::fmt::Debug>() {
        let all = T::all();
        assert!(!all.is_empty());
        let mut texts: Vec<_> = all.iter().map(|v| v.to_text()).collect();
        for v in &all {
            assert_eq!(T::from_text(v.to_text()), Some(*v), "{} 往返", T::TYPE);
        }
        texts.sort_unstable();
        texts.dedup();
        assert_eq!(texts.len(), all.len(), "{} 的文本有重复", T::TYPE);
        assert_eq!(T::from_text("no_such_value"), None, "对照：认不出的文本");
    }

    /// 可穷举的枚举用 `Finite::all()` 的全量列表穷举；其余枚举的 `to_text` 是穷举 match，
    /// 加变体漏了文本编译不过，这里再查一遍表里的文本两两不同。
    #[test]
    fn enum_columns_roundtrip() {
        roundtrip_all::<PathStatus>();
        roundtrip_all::<ProjectDefaultMode>();
        roundtrip_all::<ApprovalMode>();
        roundtrip_all::<ConversationState>();
        roundtrip_all::<MessageStatus>();
        roundtrip_all::<TurnState>();
        roundtrip_all::<Decider>();
        roundtrip_all::<VoidReason>();
        roundtrip_all::<ReviewPhase>();
        roundtrip_all::<UserActionStateKind>();

        let layers = [
            RuleLayer::Baseline,
            RuleLayer::DenialMemory,
            RuleLayer::ToolConfig,
            RuleLayer::UserDeny,
            RuleLayer::MustAsk,
            RuleLayer::Remembered,
            RuleLayer::ProjectAllow,
            RuleLayer::Default,
            RuleLayer::CredentialCommands,
            RuleLayer::AutorunConfig,
            RuleLayer::SensitiveDirs,
            RuleLayer::WritableCache,
            RuleLayer::WritableDirs,
            RuleLayer::RejectList,
        ];
        for l in layers {
            assert_eq!(RuleLayer::from_text(l.to_text()), Some(l));
        }
        for d in [
            DeliveryState::Queued,
            DeliveryState::Dispatched,
            DeliveryState::AcceptedByRuntime,
            DeliveryState::Failed,
            DeliveryState::Expired,
            DeliveryState::Cancelled,
        ] {
            assert_eq!(DeliveryState::from_text(d.to_text()), Some(d));
        }
    }

    #[test]
    fn sha_text_roundtrip_and_rejects_bad_input() {
        let sha = Sha256([0xab; 32]);
        assert_eq!(parse_sha(&sha_text(&sha)), Some(sha));
        assert_eq!(parse_sha(&"AB".repeat(32)), None, "大写不是规范写法");
        assert_eq!(parse_sha("abc"), None);
    }
}
