//! 一轮怎么结束（T06 方案 6.5）：**只有明确成功才算成功**。一张登记表 `(匹配条件) → 结局`，匹配条件只用
//! `result` 里稳定的结构化字段（结局类别、错误标记、HTTP 状态码、`terminal_reason`）；没登记的一律按错误处理。
//!
//! CLI 给的错误文字不进这里：结局里只有枚举值（错误码与 `reason`），不回显任何自由文字。
//!
//! 「未实录」的行（没有权限、过载、未登录）样本里没有，命不中时落到 `cli_error`，回合照样失败，只是原因不够细。
//! 续接失败的三类（`session_not_found` / `session_corrupt` / `session_incompatible`）的特征等 G10 实测，这里还没登记：
//! 那之前续接失败落到 `cli_error` 或 `cli_exited`（方案 19 节阻塞项）。

/// `result` 事件里决定结局的字段（`events.rs` 填）。
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ResultFacts {
    /// 结局类别（`subtype`）
    pub subtype: String,
    pub is_error: bool,
    /// API 报的 HTTP 状态码（`api_error_status`）
    pub api_error_status: Option<u16>,
    /// `terminal_reason`
    pub terminal_reason: Option<String>,
}

/// 一轮的结局。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TurnEnd {
    /// `stopReason: end_turn`
    Completed,
    /// `stopReason: cancelled`
    Cancelled,
    /// 以点名错误码结束 `session/prompt`（`error.data.iwork.code`，可带 `reason`）
    Error {
        code: &'static str,
        reason: Option<&'static str>,
    },
}

/// 登记过的结局类别（原样进 `cli_error` 的 `reason`）；没登记的写 `unknown`。
/// 来源：2.1.322 的录制样本与 S0 实测（t0-findings 2.4 G6、G7f）。
pub const KNOWN_SUBTYPES: [&str; 2] = ["success", "error_during_execution"];

/// 一行登记：条件 → 结局。`note` 写依据（实录样本或「未实录」）。
struct Row {
    matches: fn(&ResultFacts) -> bool,
    end: TurnEnd,
    #[allow(dead_code, reason = "依据只给读代码的人看")]
    note: &'static str,
}

const fn model_unavailable(reason: &'static str) -> TurnEnd {
    TurnEnd::Error {
        code: "model_unavailable",
        reason: Some(reason),
    }
}

/// 登记表，自上而下第一行命中的生效。
const ROWS: [Row; 4] = [
    Row {
        matches: |r| r.subtype == "success" && !r.is_error,
        end: TurnEnd::Completed,
        note: "实录：text_multiturn 等",
    },
    // N5：不存在的模型 subtype 照样是 success，靠 is_error + 404 + api_error 认（G6；实录 bad_model）
    Row {
        matches: |r| {
            r.is_error
                && r.api_error_status == Some(404)
                && r.terminal_reason.as_deref() == Some("api_error")
        },
        end: model_unavailable("retired"),
        note: "实录：bad_model",
    },
    Row {
        matches: |r| r.is_error && r.api_error_status == Some(403),
        end: model_unavailable("no_permission"),
        note: "未实录：按 API 的 403 写",
    },
    Row {
        matches: |r| r.is_error && r.api_error_status == Some(529),
        end: model_unavailable("overloaded"),
        note: "未实录：按 API 的 529 overloaded_error 写",
    },
];

/// `result` → 结局。`cancel_requested`：桥接发过中断，或后台发过 `session/cancel`——这时不管 CLI 报什么，
/// 都按取消收尾（用户要的是停下）。
pub fn turn_end(result: &ResultFacts, cancel_requested: bool) -> TurnEnd {
    if cancel_requested {
        return TurnEnd::Cancelled;
    }
    if let Some(row) = ROWS.iter().find(|row| (row.matches)(result)) {
        return row.end.clone();
    }
    TurnEnd::Error {
        code: "cli_error",
        reason: Some(
            KNOWN_SUBTYPES
                .iter()
                .find(|k| **k == result.subtype)
                .copied()
                .unwrap_or("unknown"),
        ),
    }
}

/// 这一轮没等到 `result` 进程就没了。
pub const CLI_EXITED: TurnEnd = TurnEnd::Error {
    code: "cli_exited",
    reason: None,
};

#[cfg(test)]
mod tests {
    use super::*;

    fn r(
        subtype: &str,
        is_error: bool,
        status: Option<u16>,
        terminal: Option<&str>,
    ) -> ResultFacts {
        ResultFacts {
            subtype: subtype.into(),
            is_error,
            api_error_status: status,
            terminal_reason: terminal.map(Into::into),
        }
    }

    /// 期望值手写，一行一个登记。
    #[test]
    fn registry_rows() {
        assert_eq!(
            turn_end(&r("success", false, None, None), false),
            TurnEnd::Completed
        );
        assert_eq!(
            turn_end(&r("success", true, Some(404), Some("api_error")), false),
            model_unavailable("retired"),
            "N5：subtype=success 但 is_error=true"
        );
        assert_eq!(
            turn_end(&r("success", true, Some(403), Some("api_error")), false),
            model_unavailable("no_permission")
        );
        assert_eq!(
            turn_end(&r("success", true, Some(529), None), false),
            model_unavailable("overloaded")
        );
    }

    /// 只有明确成功才算成功：带错误标记的 success、没登记的类别，都是错误；类别原样或 unknown。
    #[test]
    fn unregistered_is_an_error() {
        assert_eq!(
            turn_end(&r("success", true, None, None), false),
            TurnEnd::Error {
                code: "cli_error",
                reason: Some("success")
            }
        );
        assert_eq!(
            turn_end(
                &r(
                    "error_during_execution",
                    true,
                    None,
                    Some("aborted_streaming")
                ),
                false
            ),
            TurnEnd::Error {
                code: "cli_error",
                reason: Some("error_during_execution")
            }
        );
        assert_eq!(
            turn_end(&r("error_something_new", true, None, None), false),
            TurnEnd::Error {
                code: "cli_error",
                reason: Some("unknown")
            },
            "没登记的类别不回显原文"
        );
        // 404 但不是 api_error 的终止原因：不当成模型不存在
        assert_eq!(
            turn_end(&r("success", true, Some(404), None), false),
            TurnEnd::Error {
                code: "cli_error",
                reason: Some("success")
            }
        );
    }

    #[test]
    fn cancel_wins() {
        assert_eq!(
            turn_end(
                &r(
                    "error_during_execution",
                    true,
                    None,
                    Some("aborted_streaming")
                ),
                true
            ),
            TurnEnd::Cancelled
        );
        assert_eq!(
            turn_end(&r("success", false, None, None), true),
            TurnEnd::Cancelled
        );
    }
}
