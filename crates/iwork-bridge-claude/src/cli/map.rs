//! CLI 的 stream-json 输出 → 桥接要发给 ACP 的东西（T06 方案 6.1 的映射表）。纯，带状态：一个桥接一个 CLI，
//! 一个 [`StreamMapper`]。ACP 的具体消息（`session/update` 各种更新、`RuntimeReport` 的 Protobuf）由 S6 的 `acp.rs`
//! 从 [`Out`] 生成。
//!
//! - 正文：`stream_event` 的增量当场发；整条 `assistant` 消息里的文字按消息 id 只补增量没发到的那一截（只开了
//!   部分消息时全靠它，开了时多半什么都不补）。同一个消息 id 的 `assistant` 事件可能有好几条（每块一条，样本
//!   `tool_denied`），按到达次序接起来再比。
//! - 工具调用：`tool_use` 块 → [`Out::ToolCall`]，按 id 去重；工具结果 → [`Out::ToolCallUpdate`]，文字超 64 KiB 截断。
//! - `system/init`：更新会话事实、核对权限模式、按 6.2 的规则报模型（`facts.rs`）。
//! - `result`：先报用量，再按登记表定结局（`errors.rs`），回合结束。
//! - 回合之外到的正文、工具调用不发（后台对回合之外的更新本来就丢），记一条诊断。

use std::collections::{BTreeMap, BTreeSet};

use serde_json::Value;

use super::errors::{CLI_EXITED, TurnEnd, turn_end};
use super::events::{
    AssistantBlock, CliEvent, ControlResponse, LineError, RateLimitInfo, Usage, parse_line,
};
use super::facts::{self, Facts, IsolationBroken};

/// 工具结果文字的上限（后台现在不存工具输出，只给界面看）。
pub const TOOL_OUTPUT_LIMIT: usize = 64 * 1024;
/// 截断后接的标记。
pub const TRUNCATED_MARK: &str = "\n…（输出超过 64 KiB，已截断）";

/// 发给 ACP 一侧的一件事。
#[derive(Debug, Clone, PartialEq)]
pub enum Out {
    /// `agent_message_chunk`
    Text(String),
    /// `agent_thought_chunk`
    Thought(String),
    /// `tool_call{toolCallId, title=name, rawInput, status: pending}`
    ToolCall {
        id: String,
        name: String,
        raw_input: Value,
    },
    /// `tool_call_update{status: completed | failed, content}`
    ToolCallUpdate {
        id: String,
        failed: bool,
        content: String,
    },
    Report(Report),
    /// 桥接自己发的控制请求的响应（由进程薄壳按 `request_id` 配对）
    Control(ControlResponse),
    /// 这一轮结束
    End(TurnEnd),
    /// 只进桥接的诊断记录
    Diagnostic(Diagnostic),
}

/// `RuntimeReport` 的三种（方案 2.5）。
#[derive(Debug, Clone, PartialEq)]
pub enum Report {
    Model { id: String, effort: &'static str },
    Usage(Usage),
    Quota(Vec<QuotaWindow>),
}

/// 一个额度窗口（`QuotaWindowReport`）：使用率换成 0–100 的整数，重置时间是秒级时间戳。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct QuotaWindow {
    pub window: String,
    pub used_pct: u8,
    pub resets_at_secs: i64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Diagnostic {
    Facts(facts::Diagnostic),
    /// 不认识的 `type`（只计数）
    UnknownEvent,
    /// 回合之外到的正文或工具活动
    OutsideTurn,
    /// 整条消息的文字和已发的增量对不上（不补发，免得重复）
    TextMismatch,
    /// 额度事件里有窗口的数值缺失或不是数，没报（方案 6.4）
    QuotaWindowIncomplete,
    /// `result` 里没有 `model_usage_since_last_result`：token 退到 `result.usage`、费用不报（方案 6.3；每条会话记一次）
    UsageFallback,
}

/// 致命：结束 CLI。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Fatal {
    /// 输出格式坏了 → `cli_exited`
    BadLine(LineError),
    /// 权限模式对不上 → `isolation_broken`
    Isolation(IsolationBroken),
}

/// 一个回合里的去重状态。
#[derive(Debug, Default)]
struct Turn {
    cancel_requested: bool,
    /// 消息 id → 已经发出去的正文
    sent: BTreeMap<String, String>,
    /// 消息 id → 整条消息里到目前为止的正文
    seen: BTreeMap<String, String>,
    tool_calls: BTreeSet<String>,
}

#[derive(Debug)]
pub struct StreamMapper {
    facts: Facts,
    /// 启动时给的思考强度：上报模型时一起带上
    effort: &'static str,
    turn: Option<Turn>,
    /// 用量退路的诊断这条会话记过没有
    usage_fallback_noted: bool,
}

impl StreamMapper {
    /// `expect`：这次给出去的插件目录与记忆目录，每个 `init` 拿它核对会话内容（3.5）。
    pub fn new(effort: &'static str, expect: facts::SessionExpect) -> Self {
        Self {
            facts: Facts::new(expect),
            effort,
            turn: None,
            usage_fallback_noted: false,
        }
    }

    pub fn facts(&self) -> &facts::SessionFacts {
        &self.facts.session
    }

    /// `session/prompt` 到了、用户消息写给 CLI 之前：先报缓存的模型与攒下的额度。
    pub fn begin_turn(&mut self) -> Vec<Out> {
        self.turn = Some(Turn::default());
        let start = self.facts.begin_turn();
        let mut out = Vec::new();
        if let Some(id) = start.model {
            out.push(self.model_report(id));
        }
        if let Some(q) = start.quota {
            out.push(quota_report(q));
        }
        out
    }

    /// 桥接发了中断或后台发了 `session/cancel`：这一轮按取消收尾。
    pub fn request_cancel(&mut self) {
        if let Some(t) = &mut self.turn {
            t.cancel_requested = true;
        }
    }

    /// CLI 没了：回合里就以 `cli_exited` 结束。
    pub fn on_exit(&mut self) -> Vec<Out> {
        if self.turn.take().is_none() {
            return Vec::new();
        }
        let mut out = Vec::new();
        if let Some(d) = self.facts.end_turn() {
            out.push(Out::Diagnostic(Diagnostic::Facts(d)));
        }
        out.push(Out::End(CLI_EXITED));
        out
    }

    /// hook 那边先到的工具调用（4.1：`tool_call` 由先到的一方发，按 id 去重）。这一轮没报过就返回要发的
    /// [`Out::ToolCall`]，之后 CLI 输出里同一个 id 的 `tool_use` 不再报；回合之外返回 `None`。
    pub fn note_tool_call(&mut self, id: &str, name: &str, raw_input: &Value) -> Option<Out> {
        let turn = self.turn.as_mut()?;
        turn.tool_calls
            .insert(id.to_owned())
            .then(|| Out::ToolCall {
                id: id.to_owned(),
                name: name.to_owned(),
                raw_input: raw_input.clone(),
            })
    }

    /// 现在在不在一轮里。
    pub fn in_turn(&self) -> bool {
        self.turn.is_some()
    }

    /// 一行输出。
    pub fn on_line(&mut self, line: &str) -> Result<Vec<Out>, Fatal> {
        let event = parse_line(line).map_err(Fatal::BadLine)?;
        self.on_event(event)
    }

    fn model_report(&self, id: String) -> Out {
        Out::Report(Report::Model {
            id,
            effort: self.effort,
        })
    }

    fn on_event(&mut self, event: CliEvent) -> Result<Vec<Out>, Fatal> {
        let mut out = Vec::new();
        match event {
            CliEvent::Init(init) => {
                let (model, diag) = self.facts.on_init(&init).map_err(Fatal::Isolation)?;
                if let Some(id) = model {
                    out.push(self.model_report(id));
                }
                if let Some(d) = diag {
                    out.push(Out::Diagnostic(Diagnostic::Facts(d)));
                }
            }
            CliEvent::RateLimit(info) => {
                if info.incomplete > 0 {
                    out.push(Out::Diagnostic(Diagnostic::QuotaWindowIncomplete));
                }
                if !info.windows.is_empty()
                    && let Some(info) = self.facts.on_rate_limit(info)
                {
                    out.push(quota_report(info));
                }
            }
            CliEvent::ControlResponse(r) => out.push(Out::Control(r)),
            CliEvent::Unknown => out.push(Out::Diagnostic(Diagnostic::UnknownEvent)),
            CliEvent::OtherSystem | CliEvent::OtherStream => {}
            CliEvent::TextDelta { .. }
            | CliEvent::ThinkingDelta { .. }
            | CliEvent::Assistant(_)
            | CliEvent::User { .. }
            | CliEvent::Result(_) => match self.turn.as_mut() {
                None => out.push(Out::Diagnostic(Diagnostic::OutsideTurn)),
                Some(turn) => {
                    if let CliEvent::Result(r) = &event {
                        let end = turn_end(&r.facts, turn.cancel_requested);
                        if r.usage_fallback && !self.usage_fallback_noted {
                            self.usage_fallback_noted = true;
                            out.push(Out::Diagnostic(Diagnostic::UsageFallback));
                        }
                        out.push(Out::Report(Report::Usage(r.usage.clone())));
                        self.turn = None;
                        if let Some(d) = self.facts.end_turn() {
                            out.push(Out::Diagnostic(Diagnostic::Facts(d)));
                        }
                        out.push(Out::End(end));
                    } else {
                        in_turn(turn, event, &mut out);
                    }
                }
            },
        }
        Ok(out)
    }
}

/// 回合里的正文与工具活动。
fn in_turn(turn: &mut Turn, event: CliEvent, out: &mut Vec<Out>) {
    match event {
        CliEvent::TextDelta { message_id, text } => {
            if text.is_empty() {
                return;
            }
            if let Some(id) = message_id {
                turn.sent.entry(id).or_default().push_str(&text);
            }
            out.push(Out::Text(text));
        }
        // 空的思考增量（只带签名的思考块）不发
        CliEvent::ThinkingDelta { text } if text.is_empty() => {}
        CliEvent::ThinkingDelta { text } => out.push(Out::Thought(text)),
        CliEvent::Assistant(msg) => {
            let mut text = String::new();
            for block in msg.blocks {
                match block {
                    AssistantBlock::Text(t) => text.push_str(&t),
                    AssistantBlock::ToolUse { id, name, input } => {
                        if turn.tool_calls.insert(id.clone()) {
                            out.push(Out::ToolCall {
                                id,
                                name,
                                raw_input: input,
                            });
                        }
                    }
                    AssistantBlock::Other => {}
                }
            }
            if text.is_empty() {
                return;
            }
            let Some(id) = msg.id else {
                // 没有 id 对不了账：增量里发过的话会重复，只能照发
                out.push(Out::Text(text));
                return;
            };
            let seen = turn.seen.entry(id.clone()).or_default();
            seen.push_str(&text);
            let sent = turn.sent.entry(id).or_default();
            if let Some(rest) = seen.strip_prefix(sent.as_str()) {
                if !rest.is_empty() {
                    out.push(Out::Text(rest.to_owned()));
                    sent.clone_from(seen);
                }
            } else if !sent.starts_with(seen.as_str()) {
                out.push(Out::Diagnostic(Diagnostic::TextMismatch));
            }
        }
        CliEvent::User { tool_results } => {
            for r in tool_results {
                out.push(Out::ToolCallUpdate {
                    id: r.tool_use_id,
                    failed: r.is_error,
                    content: truncate(r.text),
                });
            }
        }
        _ => {}
    }
}

fn truncate(mut text: String) -> String {
    if text.len() <= TOOL_OUTPUT_LIMIT {
        return text;
    }
    let mut cut = TOOL_OUTPUT_LIMIT;
    while !text.is_char_boundary(cut) {
        cut -= 1;
    }
    text.truncate(cut);
    text.push_str(TRUNCATED_MARK);
    text
}

/// 使用率 0–1 → 0–100，四舍五入后夹到范围内（方案 6.4）。
fn quota_report(info: RateLimitInfo) -> Out {
    Out::Report(Report::Quota(
        info.windows
            .into_iter()
            .map(|w| QuotaWindow {
                window: w.window,
                used_pct: (w.utilization * 100.0).round().clamp(0.0, 100.0) as u8,
                resets_at_secs: w.resets_at,
            })
            .collect(),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 4.1：`tool_call` 由先到的一方发，按 id 去重。hook 先到就由它报，之后 CLI 输出里的同一个调用不再报；
    /// 回合之外不报。
    #[test]
    fn hook_first_tool_call_is_reported_once() {
        let mut m = StreamMapper::new("low", facts::SessionExpect::default());
        let input = serde_json::json!({"command": "ls"});
        assert_eq!(m.note_tool_call("t1", "Bash", &input), None, "回合之外");
        m.begin_turn();
        assert!(matches!(
            m.note_tool_call("t1", "Bash", &input),
            Some(Out::ToolCall { ref id, .. }) if id == "t1"
        ));
        assert_eq!(
            m.note_tool_call("t1", "Bash", &input),
            None,
            "同一个只报一次"
        );
        let line = serde_json::json!({ "type": "assistant", "message": { "id": "m1",
            "content": [{ "type": "tool_use", "id": "t1", "name": "Bash", "input": input }] } })
        .to_string();
        let outs = m.on_line(&line).unwrap();
        assert!(
            !outs.iter().any(|o| matches!(o, Out::ToolCall { .. })),
            "CLI 那边晚到的不再报：{outs:?}"
        );
    }

    #[test]
    fn long_tool_output_is_truncated_on_char_boundary() {
        let text = "汉".repeat(TOOL_OUTPUT_LIMIT); // 每个 3 字节
        let got = truncate(text);
        assert!(got.ends_with(TRUNCATED_MARK));
        assert!(got.len() <= TOOL_OUTPUT_LIMIT + TRUNCATED_MARK.len());
        assert_eq!(truncate("短".into()), "短", "对照：不超不动");
    }

    #[test]
    fn utilization_is_clamped_percent() {
        let info = RateLimitInfo {
            windows: vec![
                super::super::events::RateWindow {
                    window: "a".into(),
                    utilization: 1.7,
                    resets_at: 9,
                },
                super::super::events::RateWindow {
                    window: "b".into(),
                    utilization: -0.2,
                    resets_at: 5,
                },
            ],
            incomplete: 0,
        };
        let Out::Report(Report::Quota(w)) = quota_report(info) else {
            panic!()
        };
        assert_eq!(
            (w[0].used_pct, w[1].used_pct, w[1].resets_at_secs),
            (100, 0, 5)
        );
    }
}
