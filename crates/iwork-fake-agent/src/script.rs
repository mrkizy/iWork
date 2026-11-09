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
