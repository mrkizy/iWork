//! stream-json 输出的一行 → [`CliEvent`]（T06 方案 6.1）。只取用到的字段，别的不看；不认识的 `type` 落到
//! [`CliEvent::Unknown`]（只计数，CLI 升级加了新事件不该让会话挂掉）。字段名以 2.1.322 的录制样本为准
//! （`tests/fixtures/2.1.322`）。
//!
//! 一行不是合法 JSON、或超过 [`MAX_LINE_BYTES`]：致命（[`LineError`]），结束 CLI、回 `cli_exited`——不信任输出
//! 格式已经坏掉的 CLI。
//!
//! **决定成败的字段不拿默认值兜底**（S5 审查 1/F-3）：`result.is_error`、`tool_result.is_error` 缺失或不是布尔值时
//! 记成「不知道」（`None`），落到失败那一边，绝不当成没出错。别的字段缺失记 0 / `None`，不影响成败。

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
    /// `plugins`；这个键没有或不是数组为 `None`（会话内容核对把它当成失效，3.5）
    pub plugins: Option<Vec<InitPlugin>>,
    /// `memory_paths.auto`（3.1a 的改道核对）
    pub memory_auto: MemoryAuto,
}

/// `init.plugins` 的一项（2.1.322：`name`、`path`、`source`，有的带 `version`）。缺的或不是字符串记 `None`。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InitPlugin {
    pub name: Option<String>,
    pub source: Option<String>,
    pub path: Option<String>,
}

/// `init.memory_paths.auto` 报了什么。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MemoryAuto {
    /// 没有这个键（设了开关之后报不报没实测，3.5：「没有」算过）
    Absent,
    Path(String),
    /// 有这个键但不是字符串
    Malformed,
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
    /// `is_error` 不是明确的 `false`（缺失、不是布尔值、是 `true`）都算失败
    pub is_error: bool,
    /// 结果里的文字（字符串原样；内容块数组只取文字块，按出现次序接起来）
    pub text: String,
}

#[derive(Debug, Clone, PartialEq)]
pub struct TurnResult {
    pub facts: ResultFacts,
    pub session_id: Option<String>,
    /// 这一个 `result` 新增的用量（方案 6.3）
    pub usage: Usage,
    /// `model_usage_since_last_result` 整个没有（或不是对象）：token 退到 `result.usage`、费用记 `None`，要记诊断
    pub usage_fallback: bool,
}

/// 这一个 `result` 新增的用量（方案 6.3）：token 与费用都取 `model_usage_since_last_result`，各模型相加。
/// 累计值（`total_cost_usd`、`modelUsage`）不进来。缺的 token 记 0；费用缺一个模型就记 `None`，不拿一部分顶全部。
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
    /// 数值缺失、不是数而没报的窗口个数（方案 6.4：不拿 0 顶替，记诊断）
    pub incomplete: u32,
}

#[derive(Debug, Clone, PartialEq)]
pub struct RateWindow {
    /// 窗口名：`unifiedWindows` 的键，或顶层的 `rateLimitType`；不透明
    pub window: String,
    /// 0–1
    pub utilization: f64,
    /// 秒
    pub resets_at: i64,
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
                plugins: v.get("plugins").and_then(Value::as_array).map(|a| {
                    a.iter()
                        .map(|p| InitPlugin {
                            name: s(p, "name"),
                            source: s(p, "source"),
                            path: s(p, "path"),
                        })
                        .collect()
                }),
                memory_auto: match v.get("memory_paths").and_then(|m| m.get("auto")) {
                    None => MemoryAuto::Absent,
                    Some(Value::String(p)) => MemoryAuto::Path(p.clone()),
                    Some(_) => MemoryAuto::Malformed,
                },
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
                // 只有明确的 false 才算成功
                is_error: b.get("is_error").and_then(Value::as_bool) != Some(false),
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
    let (usage, usage_fallback) = per_result_usage(v);
    TurnResult {
        facts: ResultFacts {
            subtype: s(v, "subtype").unwrap_or_default(),
            // 缺失或不是布尔值：不知道，不当成没出错
            is_error: v.get("is_error").and_then(Value::as_bool),
            api_error_status: v
                .get("api_error_status")
                .and_then(Value::as_u64)
                .and_then(|n| u16::try_from(n).ok()),
            terminal_reason: s(v, "terminal_reason"),
        },
        session_id: s(v, "session_id"),
        usage,
        usage_fallback,
    }
}

/// 方案 6.3 的取数规则。返回（用量，是不是走了退路）。
fn per_result_usage(v: &Value) -> (Usage, bool) {
    let Some(models) = v
        .get("model_usage_since_last_result")
        .and_then(Value::as_object)
    else {
        // 退路：token 取 result.usage（样本里它和增量相等），费用不知道
        let usage = v.get("usage").cloned().unwrap_or(Value::Null);
        return (
            Usage {
                input_tokens: u(&usage, "input_tokens"),
                output_tokens: u(&usage, "output_tokens"),
                cache_read_tokens: u(&usage, "cache_read_input_tokens"),
                cache_write_tokens: u(&usage, "cache_creation_input_tokens"),
                cost_usd: None,
            },
            true,
        );
    };
    // 空对象（一次 API 调用都没成）：token 全 0、费用 0
    let mut total = Usage {
        cost_usd: Some(0.0),
        ..Usage::default()
    };
    for m in models.values() {
        total.input_tokens += u(m, "inputTokens");
        total.output_tokens += u(m, "outputTokens");
        total.cache_read_tokens += u(m, "cacheReadInputTokens");
        total.cache_write_tokens += u(m, "cacheCreationInputTokens");
        total.cost_usd = match (total.cost_usd, m.get("costUSD").and_then(Value::as_f64)) {
            (Some(sum), Some(c)) => Some(sum + c),
            _ => None,
        };
    }
    (total, false)
}

/// 一个窗口；`utilization` 或 `resetsAt` 缺失、不是数就不报（返回 `None`），不拿 0 顶替。
fn window(name: String, v: &Value) -> Option<RateWindow> {
    Some(RateWindow {
        window: name,
        utilization: v.get("utilization").and_then(Value::as_f64)?,
        resets_at: v.get("resetsAt").and_then(Value::as_i64)?,
    })
}

/// 方案 6.4：`unifiedWindows`（以窗口名为键的对象）每项一个窗口；顶层那一个（窗口名是 `rateLimitType`）不在
/// 里面就另报一条；`unifiedWindows` 没有或是空的，只报顶层那一个。
fn rate_limit(v: &Value) -> RateLimitInfo {
    let info = v.get("rate_limit_info").cloned().unwrap_or(Value::Null);
    let mut candidates: Vec<(String, &Value)> = info
        .get("unifiedWindows")
        .and_then(Value::as_object)
        .map(|m| m.iter().map(|(name, w)| (name.clone(), w)).collect())
        .unwrap_or_default();
    if let Some(top) = s(&info, "rateLimitType")
        && !candidates.iter().any(|(name, _)| *name == top)
    {
        candidates.push((top, &info));
    }
    let total = candidates.len();
    let windows: Vec<RateWindow> = candidates
        .into_iter()
        .filter_map(|(name, w)| window(name, w))
        .collect();
    RateLimitInfo {
        incomplete: u32::try_from(total - windows.len()).unwrap_or(u32::MAX),
        windows,
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

    fn result_of(extra: &str) -> TurnResult {
        let line = format!(
            r#"{{"type":"result","subtype":"success","is_error":false,"usage":{{"input_tokens":7,"output_tokens":8,"cache_read_input_tokens":9,"cache_creation_input_tokens":10}},"total_cost_usd":99.0,"modelUsage":{{"m":{{"inputTokens":1000,"costUSD":99.0}}}}{extra}}}"#
        );
        match parse_line(&line).unwrap() {
            CliEvent::Result(r) => r,
            other => panic!("{other:?}"),
        }
    }

    /// 方案 6.3：各模型相加；累计值（total_cost_usd、modelUsage）不进来。期望值手写。
    #[test]
    fn usage_sums_models_since_last_result() {
        let r = result_of(
            r#","model_usage_since_last_result":{"a":{"inputTokens":1,"outputTokens":2,"cacheReadInputTokens":3,"cacheCreationInputTokens":4,"costUSD":0.25},"b":{"inputTokens":10,"outputTokens":20,"cacheReadInputTokens":30,"cacheCreationInputTokens":40,"costUSD":0.5}}"#,
        );
        assert_eq!(
            r.usage,
            Usage {
                input_tokens: 11,
                output_tokens: 22,
                cache_read_tokens: 33,
                cache_write_tokens: 44,
                cost_usd: Some(0.75),
            }
        );
        assert!(!r.usage_fallback);
    }

    #[test]
    fn usage_edge_cases() {
        // 键在、对象空：token 0、费用 0（bad_model 样本）
        let empty = result_of(r#","model_usage_since_last_result":{}"#);
        assert_eq!(
            empty.usage,
            Usage {
                cost_usd: Some(0.0),
                ..Usage::default()
            }
        );
        assert!(!empty.usage_fallback);
        // 有一个模型缺 costUSD：费用不知道，token 照加
        let partial = result_of(
            r#","model_usage_since_last_result":{"a":{"inputTokens":1,"costUSD":0.25},"b":{"inputTokens":2}}"#,
        );
        assert_eq!(
            (partial.usage.input_tokens, partial.usage.cost_usd),
            (3, None)
        );
        // 键整个没有：token 退到 result.usage、费用 None、记退路；不拿累计值顶替
        let missing = result_of("");
        assert_eq!(
            missing.usage,
            Usage {
                input_tokens: 7,
                output_tokens: 8,
                cache_read_tokens: 9,
                cache_write_tokens: 10,
                cost_usd: None,
            }
        );
        assert!(missing.usage_fallback);
    }

    /// S5 审查 1/F-3：错误标记缺失或不是布尔值时记成「不知道」，工具结果算失败。
    #[test]
    fn error_flags_are_not_defaulted_to_success() {
        let line = r#"{"type":"result","subtype":"success","is_error":"true","api_error_status":404,"terminal_reason":"api_error"}"#;
        let CliEvent::Result(r) = parse_line(line).unwrap() else {
            panic!()
        };
        assert_eq!(r.facts.is_error, None);
        let line = r#"{"type":"result","subtype":"success"}"#;
        let CliEvent::Result(r) = parse_line(line).unwrap() else {
            panic!()
        };
        assert_eq!(r.facts.is_error, None, "缺失也一样");
        for flag in [r#""is_error":"false","#, "", r#""is_error":1,"#] {
            let line = format!(
                r#"{{"type":"user","message":{{"content":[{{"type":"tool_result","tool_use_id":"t",{flag}"content":"x"}}]}}}}"#
            );
            let CliEvent::User { tool_results } = parse_line(&line).unwrap() else {
                panic!()
            };
            assert!(tool_results[0].is_error, "{flag}");
        }
        let ok = r#"{"type":"user","message":{"content":[{"type":"tool_result","tool_use_id":"t","is_error":false,"content":"x"}]}}"#;
        let CliEvent::User { tool_results } = parse_line(ok).unwrap() else {
            panic!()
        };
        assert!(!tool_results[0].is_error, "对照：明确的 false 才算成功");
    }

    fn quota(info: &str) -> RateLimitInfo {
        match parse_line(&format!(
            r#"{{"type":"rate_limit_event","rate_limit_info":{info}}}"#
        ))
        .unwrap()
        {
            CliEvent::RateLimit(r) => r,
            other => panic!("{other:?}"),
        }
    }

    /// 方案 6.4：顶层窗口不在 unifiedWindows 里另报一条；名字已在就不重复；没有 unifiedWindows 只报顶层；
    /// 数值缺失或不是数的窗口不报、记个数。
    #[test]
    fn quota_windows() {
        let w = |name: &str, u: f64, r: i64| RateWindow {
            window: name.into(),
            utilization: u,
            resets_at: r,
        };
        let both = quota(
            r#"{"rateLimitType":"five_hour","utilization":0.1,"resetsAt":100,"unifiedWindows":{"seven_day":{"utilization":0.74,"resetsAt":200}}}"#,
        );
        assert_eq!(
            both.windows,
            [w("seven_day", 0.74, 200), w("five_hour", 0.1, 100)]
        );
        let same = quota(
            r#"{"rateLimitType":"seven_day","utilization":0.74,"resetsAt":200,"unifiedWindows":{"seven_day":{"utilization":0.74,"resetsAt":200}}}"#,
        );
        assert_eq!(same.windows, [w("seven_day", 0.74, 200)]);
        let top_only = quota(r#"{"rateLimitType":"seven_day","utilization":0.5,"resetsAt":7}"#);
        assert_eq!(top_only.windows, [w("seven_day", 0.5, 7)]);
        let gaps = quota(
            r#"{"rateLimitType":"five_hour","resetsAt":100,"unifiedWindows":{"a":{"utilization":"0.3","resetsAt":1},"b":{"utilization":0.2,"resetsAt":2}}}"#,
        );
        assert_eq!(
            (gaps.windows.clone(), gaps.incomplete),
            (vec![w("b", 0.2, 2)], 2)
        );
    }

    fn init_of(extra: &str) -> Init {
        let line =
            format!(r#"{{"type":"system","subtype":"init","permissionMode":"dontAsk"{extra}}}"#);
        match parse_line(&line) {
            Ok(CliEvent::Init(i)) => i,
            other => panic!("{other:?}"),
        }
    }

    /// 3.5 会话内容核对要的两个字段：`plugins` 的每一项与 `memory_paths.auto`。
    #[test]
    fn init_plugins_and_memory_path() {
        let i = init_of(
            r#","plugins":[{"name":"a","source":"a@builtin","path":"a","version":"1"},{"name":"b","path":7}],"memory_paths":{"auto":"/d/m"}"#,
        );
        assert_eq!(
            i.plugins,
            Some(vec![
                InitPlugin {
                    name: Some("a".into()),
                    source: Some("a@builtin".into()),
                    path: Some("a".into()),
                },
                InitPlugin {
                    name: Some("b".into()),
                    source: None,
                    path: None,
                },
            ])
        );
        assert_eq!(i.memory_auto, MemoryAuto::Path("/d/m".into()));

        let i = init_of("");
        assert_eq!((i.plugins, i.memory_auto), (None, MemoryAuto::Absent));
        let i = init_of(r#","plugins":{"n":8},"memory_paths":{"auto":1}"#);
        assert_eq!((i.plugins, i.memory_auto), (None, MemoryAuto::Malformed));
        let i = init_of(r#","memory_paths":{}"#);
        assert_eq!(i.memory_auto, MemoryAuto::Absent);
    }

    #[test]
    fn unknown_type_is_counted_not_fatal() {
        assert_eq!(parse_line(r#"{"type":"brand_new"}"#), Ok(CliEvent::Unknown));
        assert_eq!(parse_line(r#"{"no_type":1}"#), Ok(CliEvent::Unknown));
    }
}
