//! 运行时在错误响应里给的点名错误码 → [`AgentError`]（T06 方案 2.6 第 7 条）。
//!
//! 约定：错误响应的 `error.data.iwork.code` 是一个枚举字符串，部分错误另带 `reason`（也是枚举）。
//! 这里**只认枚举值**：运行时给的任何自由文字都不回显（可能夹带正文或凭据），`reason` 不像枚举
//! 的一律当 `unknown`。不认识的 `code` 返回 `None`，由调用方按原有逻辑处理（方法名 + 错误码）。
//!
//! 加一种错误码 = [`map`] 里加一行，桥接那边照同一份字符串写。

use iwork_domain::session::SessionLostReason;
use iwork_ports::agent::{AgentError, ModelUnavailableReason};
use serde_json::Value;

use crate::convert::META_KEY;

/// 映射要用到、错误码本身不带的上下文：错误码不回显运行时文字，模型与版本取自后台已知的值。
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ErrorContext {
    /// 这条会话请求的模型；还没开会话时为空
    pub model: String,
    /// 运行时登记的 CLI 版本，没登记 CLI 时用适配器版本；握手完成前为空
    pub version: String,
}

/// `reason` 只接受这种形状：小写字母、数字、下划线、点（字段路径），不超过 40 个字符。别的一律 `unknown`。
fn enum_like(s: &str) -> bool {
    !s.is_empty()
        && s.len() <= 40
        && s.bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_' || b == b'.')
}

/// 从错误响应的 `data` 里取 `(code, reason)`。
fn code_of(data: Option<&Value>) -> Option<(&str, &str)> {
    let iwork = data?.get(META_KEY)?;
    let code = iwork.get("code")?.as_str()?;
    let reason = iwork
        .get("reason")
        .and_then(Value::as_str)
        .filter(|r| enum_like(r))
        .unwrap_or("unknown");
    Some((code, reason))
}

fn model_reason(reason: &str) -> ModelUnavailableReason {
    match reason {
        "no_permission" => ModelUnavailableReason::NoPermission,
        "retired" => ModelUnavailableReason::Retired,
        "overloaded" => ModelUnavailableReason::Overloaded,
        _ => ModelUnavailableReason::Unknown,
    }
}

fn lost(reason: SessionLostReason) -> AgentError {
    AgentError::SessionLost { reason }
}

fn protocol(message: &str) -> AgentError {
    AgentError::Protocol {
        message: message.into(),
    }
}

/// 错误响应的 `data` → [`AgentError`]。没有点名错误码、或错误码不认识时返回 `None`。
pub fn map(data: Option<&Value>, cx: &ErrorContext) -> Option<AgentError> {
    let (code, reason) = code_of(data)?;
    let version = cx.version.clone();
    Some(match code {
        "runtime_not_found" => AgentError::RuntimeNotFound,
        "not_logged_in" => AgentError::NotLoggedIn,
        "model_unavailable" => AgentError::ModelUnavailable {
            model: cx.model.clone(),
            reason: model_reason(reason),
        },
        "cli_too_old" | "version_unverified" => AgentError::Incompatible { version },
        "setup_invalid" => AgentError::Protocol {
            message: format!("会话配置不完整或不认识：{reason}"),
        },
        "hook_inactive" => protocol("审批 hook 没有生效，会话已停止"),
        "isolation_broken" => protocol("用户配置进入了会话，会话已停止"),
        "approval_backstop" => protocol("审批等待兜底触发，会话已停止"),
        // 续接时记录用不了，三类分开报：编排据此用摘要新开会话线（T06 方案 5.2a）
        "session_not_found" => lost(SessionLostReason::NotFound),
        "session_corrupt" => lost(SessionLostReason::Corrupt),
        "session_incompatible" => lost(SessionLostReason::Incompatible),
        "sandbox_unavailable" => protocol("沙箱不可用，会话没有启动"),
        "channel_failed" => protocol("审批通道建立失败"),
        "cli_error" => AgentError::Protocol {
            message: format!("运行时报告了错误（{reason}）"),
        },
        "verification_required" => AgentError::Verifying { version },
        "verification_failed" => AgentError::Incompatible {
            version: format!("{version}：本机验证没过（{reason}）"),
        },
        "cli_exited" => AgentError::ProcessExited {
            message: "运行时底下的 CLI 退出了".into(),
        },
        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    fn cx() -> ErrorContext {
        ErrorContext {
            model: "opus".into(),
            version: "2.1.322".into(),
        }
    }

    fn with(code: &str, reason: Option<&str>) -> Value {
        match reason {
            Some(r) => json!({ "iwork": { "code": code, "reason": r } }),
            None => json!({ "iwork": { "code": code } }),
        }
    }

    fn mapped(code: &str, reason: Option<&str>) -> Option<AgentError> {
        map(Some(&with(code, reason)), &cx())
    }

    fn proto(m: &str) -> Option<AgentError> {
        Some(AgentError::Protocol { message: m.into() })
    }

    /// 方案 2.6 第 7 条表里的每一行。期望值手写，不从实现里取。
    #[test]
    fn bridge_error_codes_map() {
        assert_eq!(
            mapped("runtime_not_found", None),
            Some(AgentError::RuntimeNotFound)
        );
        assert_eq!(mapped("not_logged_in", None), Some(AgentError::NotLoggedIn));
        for (reason, want) in [
            ("no_permission", ModelUnavailableReason::NoPermission),
            ("retired", ModelUnavailableReason::Retired),
            ("overloaded", ModelUnavailableReason::Overloaded),
            ("unknown", ModelUnavailableReason::Unknown),
            ("something_new", ModelUnavailableReason::Unknown),
        ] {
            assert_eq!(
                mapped("model_unavailable", Some(reason)),
                Some(AgentError::ModelUnavailable {
                    model: "opus".into(),
                    reason: want
                }),
                "{reason}"
            );
        }
        assert_eq!(
            mapped("model_unavailable", None),
            Some(AgentError::ModelUnavailable {
                model: "opus".into(),
                reason: ModelUnavailableReason::Unknown
            })
        );
        let incompatible = Some(AgentError::Incompatible {
            version: "2.1.322".into(),
        });
        assert_eq!(mapped("cli_too_old", None), incompatible);
        assert_eq!(mapped("version_unverified", None), incompatible);
        assert_eq!(
            mapped("setup_invalid", Some("tool_set.enabled_groups")),
            proto("会话配置不完整或不认识：tool_set.enabled_groups")
        );
        assert_eq!(
            mapped("hook_inactive", None),
            proto("审批 hook 没有生效，会话已停止")
        );
        assert_eq!(
            mapped("isolation_broken", None),
            proto("用户配置进入了会话，会话已停止")
        );
        assert_eq!(
            mapped("approval_backstop", None),
            proto("审批等待兜底触发，会话已停止")
        );
        for (code, reason) in [
            ("session_not_found", SessionLostReason::NotFound),
            ("session_corrupt", SessionLostReason::Corrupt),
            ("session_incompatible", SessionLostReason::Incompatible),
        ] {
            assert_eq!(
                mapped(code, None),
                Some(AgentError::SessionLost { reason }),
                "{code}"
            );
        }
        assert_eq!(
            mapped("sandbox_unavailable", None),
            proto("沙箱不可用，会话没有启动")
        );
        assert_eq!(mapped("channel_failed", None), proto("审批通道建立失败"));
        assert_eq!(
            mapped("cli_error", Some("error_max_turns")),
            proto("运行时报告了错误（error_max_turns）")
        );
        assert_eq!(
            mapped("verification_required", None),
            Some(AgentError::Verifying {
                version: "2.1.322".into()
            })
        );
        assert_eq!(
            mapped("verification_failed", Some("executed_without_allow")),
            Some(AgentError::Incompatible {
                version: "2.1.322：本机验证没过（executed_without_allow）".into()
            })
        );
        assert_eq!(
            mapped("cli_exited", None),
            Some(AgentError::ProcessExited {
                message: "运行时底下的 CLI 退出了".into()
            })
        );
        // 别的或缺失：交回原有逻辑
        assert_eq!(mapped("brand_new_code", None), None);
        assert_eq!(map(None, &cx()), None);
        assert_eq!(map(Some(&json!({ "other": 1 })), &cx()), None);
        assert_eq!(map(Some(&json!({ "iwork": { "code": 7 } })), &cx()), None);
    }

    /// 运行时塞进 `reason` 的自由文字不回显：不像枚举的一律当 `unknown`。
    #[test]
    fn free_text_reason_is_never_echoed() {
        for bad in [
            "token sk-ant-xxxx",
            "含中文",
            "UPPER",
            "a/b",
            "",
            &"x".repeat(41),
        ] {
            let e = mapped("cli_error", Some(bad)).unwrap();
            assert_eq!(e, proto("运行时报告了错误（unknown）").unwrap(), "{bad:?}");
        }
        let e = mapped("verification_failed", Some("see https://x")).unwrap();
        assert!(!e.to_string().contains("https"), "{e}");
    }

    #[test]
    fn verifying_is_retryable_and_incompatible_is_not() {
        assert!(mapped("verification_required", None).unwrap().retryable());
        assert!(!mapped("verification_failed", None).unwrap().retryable());
    }

    /// 记录用不了不靠原样重试解决（编排走摘要新开），所以不可重试。
    #[test]
    fn session_lost_is_not_retryable() {
        assert!(!mapped("session_not_found", None).unwrap().retryable());
    }
}
