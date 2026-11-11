//! 假运行时的脚本格式（JSON）。格式说明见本 crate 的 README.md，改这里要同步改它。

use serde::Deserialize;
use serde_json::Value;

/// 整个脚本。
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Script {
    #[serde(default)]
    pub agent: AgentInfo,
    /// 第 n 次 prompt 执行第 n 个回合；回合用完后再 prompt，返回协议错误
    pub turns: Vec<Turn>,
}

/// `initialize` 响应里报告的身份与能力。
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct AgentInfo {
    pub name: String,
    pub version: String,
    /// 底层 CLI；报了就放进 `_meta.iwork.cli`
    pub cli: Option<CliInfo>,
    pub load_session: bool,
    pub system_prompt: bool,
    pub image: bool,
    /// 报了就放进 `_meta.iwork.permissionWaitLimitSecs`
    pub permission_wait_limit_secs: Option<Value>,
    /// 报了就放进 `_meta.iwork.sandboxPlatform`（原样，测试也用它造坏值）
    pub sandbox_platform: Option<Value>,
    /// 非空就放进 `_meta.iwork.effortLevels`
    pub effort_levels: Vec<String>,
    /// 非空就放进 `_meta.iwork.models`
    pub models: Vec<ModelInfo>,
    /// 报了就放进 `session/new`、`session/load` 响应的 `_meta.iwork.sandbox`
    pub session_sandbox: Option<String>,
    /// 报了就放进 `session/load` 响应的 `_meta.iwork.cliSessionId`
    pub resumed_session_id: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ModelInfo {
    pub id: String,
    pub label: String,
}

impl Default for AgentInfo {
    fn default() -> Self {
        Self {
            name: "iwork-fake-agent".into(),
            version: env!("CARGO_PKG_VERSION").into(),
            cli: None,
            load_session: true,
            system_prompt: true,
            image: false,
            permission_wait_limit_secs: None,
            sandbox_platform: None,
            effort_levels: Vec::new(),
            models: Vec::new(),
            session_sandbox: None,
            resumed_session_id: None,
        }
    }
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CliInfo {
    pub path: String,
    pub version: String,
}

/// 一个回合：按顺序执行步骤，全部执行完以 `stop_reason` 结束。
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Turn {
    pub steps: Vec<Step>,
    #[serde(default)]
    pub stop_reason: StopReason,
}

#[derive(Debug, Default, Clone, Copy, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StopReason {
    #[default]
    EndTurn,
    MaxTokens,
    Refusal,
}

/// 一步。除 `hang` 外，每步开始前与等待期间都检查取消：收到取消就以 `cancelled` 结束回合。
#[derive(Debug, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum Step {
    /// 输出一段正文。`chunk_chars` 为每片字符数（不填则整段一片）；`rate_per_sec` 为每秒片数（不填则不限速）
    Text {
        text: String,
        chunk_chars: Option<usize>,
        rate_per_sec: Option<f64>,
    },
    /// 把同一片正文重复输出 `count` 次（压测用）。`stamp` 为真时每片前面加上发出时刻
    /// （Unix 纪元起的纳秒数）和 `|`，接收方据此算延迟
    RepeatText {
        chunk: String,
        count: u64,
        rate_per_sec: Option<f64>,
        #[serde(default)]
        stamp: bool,
    },
    /// 往 stderr 写一行（检查后台服务不把它原样记下）
    Stderr {
        text: String,
    },
    /// 绕过 ACP 直接往 stdout 写一行坏数据：`oversize` 写 `bytes` 个 `x`，`invalid_utf8` 写非 UTF-8
    /// 字节。写完就挂起，等后台服务结束进程
    BadLine {
        kind: BadLineKind,
        #[serde(default)]
        bytes: usize,
    },
    /// 开始一次工具调用
    ToolCall {
        id: String,
        name: String,
        #[serde(default)]
        title: Option<String>,
        #[serde(default)]
        input: Value,
    },
    /// 结束一次工具调用
    ToolResult {
        id: String,
        ok: bool,
        #[serde(default)]
        output: String,
    },
    /// 为某次工具调用请求权限，按结果走不同分支；客户端答「已取消」时按取消结束回合
    Permission {
        tool_call_id: String,
        /// 报了就放进请求的 `_meta.iwork.cwd`
        #[serde(default)]
        cwd: Option<String>,
        #[serde(default)]
        allowed: Vec<Step>,
        #[serde(default)]
        denied: Vec<Step>,
    },
    Sleep {
        ms: u64,
    },
    /// 立即以给定退出码退出进程，不回任何响应
    Crash {
        #[serde(default = "default_crash_code")]
        exit_code: i32,
    },
    /// 永远不结束，也不理会取消（只能被结束进程）
    Hang,
    /// 启动一个子进程且不等它（检查进程树清理用）；子进程号写进记录文件
    SpawnChild {
        program: String,
        #[serde(default)]
        args: Vec<String>,
    },
    /// 上报实际模型（`_meta.iwork.report`，挂在一条空的 `session_info_update` 上）
    ReportModel {
        model_id: String,
        #[serde(default)]
        effort: Option<String>,
        /// 给了就把上报挂在一条带这段正文的 `agent_message_chunk` 上（违反约定的挂法，测上报与更新都不丢）
        #[serde(default)]
        on_text: Option<String>,
    },
    /// 上报一个回合的用量
    ReportUsage {
        input_tokens: u64,
        output_tokens: u64,
        #[serde(default)]
        cache_read_tokens: u64,
        #[serde(default)]
        cache_write_tokens: u64,
        #[serde(default)]
        cost_usd: Option<f64>,
    },
    /// 上报额度窗口
    ReportQuota {
        windows: Vec<QuotaWindow>,
    },
    /// 上报一个原样的 `_meta.iwork.report` 值（造解不出来的上报）
    ReportRaw {
        value: Value,
    },
    /// 以 JSON-RPC 错误结束这次 prompt，`data` 为 `{"iwork": {"code", "reason"?}}`
    Error {
        code: String,
        #[serde(default)]
        reason: Option<String>,
    },
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct QuotaWindow {
    pub window: String,
    pub used_pct: u32,
    /// Unix 纪元起的秒数
    #[serde(default)]
    pub resets_at_secs: Option<i64>,
}

#[derive(Debug, Clone, Copy, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BadLineKind {
    Oversize,
    InvalidUtf8,
}

fn default_crash_code() -> i32 {
    101
}

impl Script {
    pub fn parse(text: &str) -> Result<Self, String> {
        serde_json::from_str(text).map_err(|e| format!("脚本解析失败：{e}"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn readme_example_parses() {
        let readme = include_str!("../README.md");
        let start = readme.find("```json").expect("README 里有 json 示例") + "```json".len();
        let end = start + readme[start..].find("```").unwrap();
        let script = Script::parse(&readme[start..end]).unwrap();
        assert_eq!(script.turns.len(), 2);
        assert!(matches!(script.turns[0].steps[2], Step::Permission { .. }));
    }

    #[test]
    fn unknown_step_type_is_rejected_with_name() {
        let err = Script::parse(r#"{"turns":[{"steps":[{"type":"explode"}]}]}"#).unwrap_err();
        assert!(err.contains("explode"), "{err}");
    }
}
