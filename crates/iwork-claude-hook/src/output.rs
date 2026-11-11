//! hook 写给 Claude Code 的结论（t0-findings 2.1 验证过的形状）。用 serde_json 生成，保证是合法 JSON。
//!
//! 拒绝：`{"hookSpecificOutput":{"hookEventName":"PreToolUse","permissionDecision":"deny","permissionDecisionReason":"<原因>"}}`，
//! 退出码 0。允许：`permissionDecision: "allow"`，桥接给了改写后的输入时带 `updatedInput`（JSON 对象）。

use serde_json::{Map, Value, json};

use crate::client::Failure;
use crate::wire::{Decision, Response};

/// 回给 Claude Code 的结论。只有 [`Outcome::from_response`] 收到一条明确的「允许」时才是 `Allow`。
#[derive(Debug, Clone, PartialEq)]
pub enum Outcome {
    Allow {
        updated_input: Option<Map<String, Value>>,
    },
    Deny {
        reason: String,
    },
}

/// 桥接答了拒绝却没给理由时回给模型的话。
pub const DENIED_WITHOUT_REASON: &str = "iWork 拒绝了这次调用";

impl Outcome {
    pub fn deny(reason: impl Into<String>) -> Self {
        Self::Deny {
            reason: reason.into(),
        }
    }

    pub fn from_failure(f: Failure) -> Self {
        Self::deny(f.reason())
    }

    pub fn from_response(r: Response) -> Self {
        match (r.decision, r.updated_input) {
            (Decision::Allow, None) => Self::Allow {
                updated_input: None,
            },
            (Decision::Allow, Some(Value::Object(map))) => Self::Allow {
                updated_input: Some(map),
            },
            // 线格式已挡住；这里再挡一次，不让「不是对象的改写」变成允许
            (Decision::Allow, Some(_)) => Self::from_failure(Failure::BadResponse),
            (Decision::Deny, _) if r.message.is_empty() => Self::deny(DENIED_WITHOUT_REASON),
            (Decision::Deny, _) => Self::deny(r.message),
        }
    }
}

/// PreToolUse 的结论 JSON（一行）。
pub fn pre_tool_use_json(outcome: &Outcome) -> String {
    let mut inner = json!({ "hookEventName": "PreToolUse" });
    match outcome {
        Outcome::Allow { updated_input } => {
            inner["permissionDecision"] = json!("allow");
            if let Some(input) = updated_input {
                inner["updatedInput"] = Value::Object(input.clone());
            }
        }
        Outcome::Deny { reason } => {
            inner["permissionDecision"] = json!("deny");
            inner["permissionDecisionReason"] = json!(reason);
        }
    }
    json!({ "hookSpecificOutput": inner }).to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(s: &str) -> Value {
        serde_json::from_str(s).expect("输出是合法 JSON")
    }

    #[test]
    fn deny_matches_the_verified_shape() {
        let out = parse(&pre_tool_use_json(&Outcome::deny("不行\"\n")));
        assert_eq!(
            out,
            json!({ "hookSpecificOutput": {
                "hookEventName": "PreToolUse",
                "permissionDecision": "deny",
                "permissionDecisionReason": "不行\"\n",
            }})
        );
    }

    #[test]
    fn allow_carries_rewritten_input_only_as_object() {
        let r = Response {
            decision: Decision::Allow,
            message: String::new(),
            updated_input: Some(json!({ "command": "sandbox-exec true" })),
        };
        let out = parse(&pre_tool_use_json(&Outcome::from_response(r)));
        assert_eq!(
            out,
            json!({ "hookSpecificOutput": {
                "hookEventName": "PreToolUse",
                "permissionDecision": "allow",
                "updatedInput": { "command": "sandbox-exec true" },
            }})
        );
        let plain = parse(&pre_tool_use_json(&Outcome::from_response(
            Response::allow(),
        )));
        assert!(plain["hookSpecificOutput"].get("updatedInput").is_none());
        // 不是对象的改写不会变成允许
        let bad = Response {
            decision: Decision::Allow,
            message: String::new(),
            updated_input: Some(json!("rm -rf /")),
        };
        assert_eq!(
            Outcome::from_response(bad),
            Outcome::from_failure(Failure::BadResponse)
        );
    }

    #[test]
    fn deny_relays_bridge_message_or_a_default() {
        assert_eq!(
            Outcome::from_response(Response::deny("用户拒绝")),
            Outcome::deny("用户拒绝")
        );
        assert_eq!(
            Outcome::from_response(Response::deny("")),
            Outcome::deny(DENIED_WITHOUT_REASON)
        );
    }
}
