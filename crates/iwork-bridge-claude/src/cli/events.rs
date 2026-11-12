//! stream-json 输出的一行 → [`CliEvent`]（T06 方案 6.1）。只取用到的字段，别的不看；不认识的 `type` 落到
//! [`CliEvent::Unknown`]（只计数，CLI 升级加了新事件不该让会话挂掉）。字段名以 2.1.322 的录制样本为准
//! （`tests/fixtures/2.1.322`）。
//!
//! 一行不是合法 JSON、或超过 [`MAX_LINE_BYTES`]：致命（[`LineError`]），结束 CLI、回 `cli_exited`——不信任输出
//! 格式已经坏掉的 CLI。

use serde_json::Value;

use super::errors::ResultFacts;

/// 一行输出的上限：超过就当 CLI 坏了。
pub const MAX_LINE_BYTES: usize = 8 * 1024 * 1024;

/// 一行解析不了。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LineError {
    TooLong { bytes: usize },
    NotJson,
}

/// 一条 stream-json 事件里桥接要用的东西。
#[derive(Debug, Clone, PartialEq)]
pub enum CliEvent {
    /// `system/init`：每一轮开头都有一个（S0 实测）
    Init(Init),
    /// 别的 `system` 子类型（`hook_started` 之类），不处理
    OtherSystem,
    /// `stream_event` 里的正文增量；`message_id` 是顶层的 `api_message_id`，和整条消息的 `message.id` 同一个
    TextDelta {
        message_id: Option<String>,
        text: String,
    },
    /// `stream_event` 里的思考增量
    ThinkingDelta {
        text: String,
    },
    /// 别的 `stream_event`（消息开始、块开始结束……），不处理
    OtherStream,
    /// 一条完整的助手消息
    Assistant(AssistantMessage),
    /// `user`：工具结果（内容块数组）；`content` 是字符串的（如 `set_model` 之后那条，N4）没有工具结果
    User {
        tool_results: Vec<ToolResult>,
    },
    /// 一轮结束
    Result(TurnResult),
    RateLimit(RateLimitInfo),
    /// 桥接自己发的控制请求的响应
    ControlResponse(ControlResponse),
    Unknown,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Init {
    pub session_id: Option<String>,
    pub model: Option<String>,
    pub permission_mode: Option<String>,
    pub tools: Vec<String>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct AssistantMessage {
    /// 消息 id：同一条消息的增量与整条按它对账
    pub id: Option<String>,
    pub model: Option<String>,
    /// 子代理里的消息带它
    pub parent_tool_use_id: Option<String>,
    pub blocks: Vec<AssistantBlock>,
}

#[derive(Debug, Clone, PartialEq)]
pub enum AssistantBlock {
    Text(String),
    ToolUse {
        id: String,
        name: String,
        input: Value,
    },
    /// 思考等别的块，不映射
    Other,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ToolResult {
    pub tool_use_id: String,
    pub is_error: bool,
    /// 结果里的文字（字符串原样；内容块数组只取文字块，按出现次序接起来）
    pub text: String,
}

#[derive(Debug, Clone, PartialEq)]
pub struct TurnResult {
    pub facts: ResultFacts,
    pub session_id: Option<String>,
    pub usage: Usage,
}

/// `result.usage` 与 `total_cost_usd`（方案 6.3）。缺的记 0 / `None`，不估算。
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Usage {
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub cache_read_tokens: u64,
    pub cache_write_tokens: u64,
    pub cost_usd: Option<f64>,
}

/// `rate_limit_event.rate_limit_info`（方案 6.4；S0：`utilization` 是 0–1 的小数，`resetsAt` 是秒级时间戳）。
#[derive(Debug, Clone, PartialEq)]
pub struct RateLimitInfo {
    pub windows: Vec<RateWindow>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct RateWindow {
    /// `rateLimitType`，不透明
    pub window: String,
    /// 0–1
    pub utilization: Option<f64>,
    /// 秒
    pub resets_at: Option<i64>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct ControlResponse {
    pub request_id: Option<String>,
    pub success: bool,
    /// `initialize` 响应里的 `current_permission_mode`（方案 3.5）。只取这一个键：响应里有 `account`，不保留
    pub current_permission_mode: Option<String>,
}

fn s(v: &Value, key: &str) -> Option<String> {
    v.get(key).and_then(Value::as_str).map(str::to_owned)
}

fn u(v: &Value, key: &str) -> u64 {
    v.get(key).and_then(Value::as_u64).unwrap_or(0)
}

/// 解析一行。
pub fn parse_line(line: &str) -> Result<CliEvent, LineError> {
    if line.len() > MAX_LINE_BYTES {
        return Err(LineError::TooLong { bytes: line.len() });
    }
    let v: Value = serde_json::from_str(line).map_err(|_| LineError::NotJson)?;
    if !v.is_object() {
        return Err(LineError::NotJson);
    }
    Ok(match v.get("type").and_then(Value::as_str) {
        Some("system") if v.get("subtype").and_then(Value::as_str) == Some("init") => {
            CliEvent::Init(Init {
                session_id: s(&v, "session_id"),
                model: s(&v, "model"),
                permission_mode: s(&v, "permissionMode"),
                tools: v
                    .get("tools")
                    .and_then(Value::as_array)
                    .map(|a| {
                        a.iter()
                            .filter_map(Value::as_str)
                            .map(str::to_owned)
                            .collect()
                    })
                    .unwrap_or_default(),
            })
        }
        Some("system") => CliEvent::OtherSystem,
        Some("stream_event") => stream_event(&v, s(&v, "api_message_id")),
        Some("assistant") => CliEvent::Assistant(assistant(&v)),
        Some("user") => CliEvent::User {
            tool_results: tool_results(&v),
        },
        Some("result") => CliEvent::Result(result(&v)),
        Some("rate_limit_event") => CliEvent::RateLimit(rate_limit(&v)),
        Some("control_response") => CliEvent::ControlResponse(control_response(&v)),
        _ => CliEvent::Unknown,
    })
}

fn stream_event(v: &Value, message_id: Option<String>) -> CliEvent {
    let Some(delta) = v
        .get("event")
        .filter(|e| e.get("type").and_then(Value::as_str) == Some("content_block_delta"))
        .and_then(|e| e.get("delta"))
    else {
        return CliEvent::OtherStream;
    };
    match delta.get("type").and_then(Value::as_str) {
        Some("text_delta") => s(delta, "text").map_or(CliEvent::OtherStream, |text| {
            CliEvent::TextDelta { message_id, text }
        }),
        Some("thinking_delta") => s(delta, "thinking").map_or(CliEvent::OtherStream, |text| {
            CliEvent::ThinkingDelta { text }
        }),
        _ => CliEvent::OtherStream,
    }
}

fn assistant(v: &Value) -> AssistantMessage {
    let msg = v.get("message").cloned().unwrap_or(Value::Null);
    let blocks = msg
        .get("content")
        .and_then(Value::as_array)
        .map(|a| {
            a.iter()
                .map(|b| match b.get("type").and_then(Value::as_str) {
                    Some("text") => AssistantBlock::Text(s(b, "text").unwrap_or_default()),
                    Some("tool_use") => match (s(b, "id"), s(b, "name")) {
                        (Some(id), Some(name)) => AssistantBlock::ToolUse {
                            id,
                            name,
                            input: b.get("input").cloned().unwrap_or(Value::Null),
                        },
                        _ => AssistantBlock::Other,
                    },
                    _ => AssistantBlock::Other,
                })
                .collect()
        })
        .unwrap_or_default();
    AssistantMessage {
        id: s(&msg, "id"),
        model: s(&msg, "model"),
        parent_tool_use_id: s(v, "parent_tool_use_id"),
        blocks,
    }
}

/// `content` 是字符串的 user 事件没有工具结果（N4）。
fn tool_results(v: &Value) -> Vec<ToolResult> {
    let Some(blocks) = v
        .get("message")
        .and_then(|m| m.get("content"))
        .and_then(Value::as_array)
    else {
        return Vec::new();
    };
    blocks
        .iter()
        .filter(|b| b.get("type").and_then(Value::as_str) == Some("tool_result"))
        .filter_map(|b| {
            Some(ToolResult {
                tool_use_id: s(b, "tool_use_id")?,
                is_error: b.get("is_error").and_then(Value::as_bool).unwrap_or(false),
                text: match b.get("content") {
                    Some(Value::String(t)) => t.clone(),
                    Some(Value::Array(parts)) => parts
                        .iter()
                        .filter(|p| p.get("type").and_then(Value::as_str) == Some("text"))
                        .filter_map(|p| p.get("text").and_then(Value::as_str))
                        .collect::<Vec<_>>()
                        .join(""),
                    _ => String::new(),
                },
            })
        })
        .collect()
}

fn result(v: &Value) -> TurnResult {
    let usage = v.get("usage").cloned().unwrap_or(Value::Null);
    TurnResult {
        facts: ResultFacts {
            subtype: s(v, "subtype").unwrap_or_default(),
            is_error: v.get("is_error").and_then(Value::as_bool).unwrap_or(false),
            api_error_status: v
                .get("api_error_status")
                .and_then(Value::as_u64)
                .and_then(|n| u16::try_from(n).ok()),
            terminal_reason: s(v, "terminal_reason"),
        },
        session_id: s(v, "session_id"),
        usage: Usage {
            input_tokens: u(&usage, "input_tokens"),
            output_tokens: u(&usage, "output_tokens"),
            cache_read_tokens: u(&usage, "cache_read_input_tokens"),
            cache_write_tokens: u(&usage, "cache_creation_input_tokens"),
            cost_usd: v.get("total_cost_usd").and_then(Value::as_f64),
        },
    }
}

fn window(name: String, v: &Value) -> RateWindow {
    RateWindow {
        window: name,
        utilization: v.get("utilization").and_then(Value::as_f64),
        resets_at: v.get("resetsAt").and_then(Value::as_i64),
    }
}

/// `unifiedWindows`（2.1.322：以窗口名为键的对象）有内容时每项一个窗口，否则用 `rate_limit_info` 自己那一个
/// （窗口名是它的 `rateLimitType`）。
fn rate_limit(v: &Value) -> RateLimitInfo {
    let info = v.get("rate_limit_info").cloned().unwrap_or(Value::Null);
    let unified: Vec<RateWindow> = info
        .get("unifiedWindows")
        .and_then(Value::as_object)
        .map(|m| m.iter().map(|(name, w)| window(name.clone(), w)).collect())
        .unwrap_or_default();
    RateLimitInfo {
        windows: if unified.is_empty() {
            s(&info, "rateLimitType")
                .map(|name| window(name, &info))
                .into_iter()
                .collect()
        } else {
            unified
        },
    }
}

fn control_response(v: &Value) -> ControlResponse {
    let r = v.get("response").cloned().unwrap_or(Value::Null);
    ControlResponse {
        request_id: s(&r, "request_id"),
        success: r.get("subtype").and_then(Value::as_str) == Some("success"),
        current_permission_mode: r
            .get("response")
            .and_then(|inner| s(inner, "current_permission_mode")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// N4（t0-findings 2.4 G7g）：`set_model` 之后那条 user 事件的 `content` 是字符串，不是内容块数组。
    /// 2.1.322 的录制里切到同一个模型时 CLI 没发这条（未实录），这里按 S0 记下的形状造一条。
    #[test]
    fn user_with_string_content_has_no_tool_results() {
        let line = r#"{"type":"user","message":{"role":"user","content":"<local-command-stdout>Set model to haiku</local-command-stdout>"}}"#;
        assert_eq!(
            parse_line(line),
            Ok(CliEvent::User {
                tool_results: vec![]
            })
        );
    }

    #[test]
    fn bad_lines_are_fatal() {
        assert_eq!(parse_line("not json"), Err(LineError::NotJson));
        assert_eq!(parse_line("[1,2]"), Err(LineError::NotJson));
        let long = format!("\"{}\"", "x".repeat(MAX_LINE_BYTES));
        assert!(matches!(parse_line(&long), Err(LineError::TooLong { .. })));
    }

    #[test]
    fn unknown_type_is_counted_not_fatal() {
        assert_eq!(parse_line(r#"{"type":"brand_new"}"#), Ok(CliEvent::Unknown));
        assert_eq!(parse_line(r#"{"no_type":1}"#), Ok(CliEvent::Unknown));
    }
}
