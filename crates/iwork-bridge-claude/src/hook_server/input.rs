//! Claude Code 交给 PreToolUse hook 的 stdin（t0-findings 2.4 G1b）：桥接只取顶层这几个键，`tool_input` 原样
//! 转给后台，不解析它的任何字段（方案 4.1）。纯函数。

use serde_json::Value;

/// 一次工具调用的 hook 输入。
#[derive(Debug, Clone, PartialEq)]
pub struct HookInput {
    pub tool_name: String,
    pub tool_input: Value,
    pub tool_use_id: String,
    /// CLI 报的当前工作目录（`cd` 之后会变，G1c）；没给为 `None`
    pub cwd: Option<String>,
}

/// 解析不了：桥接答拒绝，不交给后台（交过去也问不出东西）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BadHookInput(pub &'static str);

pub fn parse(stdin: &[u8]) -> Result<HookInput, BadHookInput> {
    let v: Value = serde_json::from_slice(stdin).map_err(|_| BadHookInput("不是 JSON"))?;
    let text = |key: &'static str| {
        v.get(key)
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty())
            .map(str::to_owned)
            .ok_or(BadHookInput(key))
    };
    Ok(HookInput {
        tool_name: text("tool_name")?,
        tool_use_id: text("tool_use_id")?,
        tool_input: v
            .get("tool_input")
            .cloned()
            .ok_or(BadHookInput("tool_input"))?,
        cwd: v.get("cwd").and_then(Value::as_str).map(str::to_owned),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn takes_the_top_level_keys_only() {
        let input = parse(
            br#"{"session_id":"s","tool_name":"Bash","tool_input":{"command":"ls"},"tool_use_id":"toolu_1","cwd":"/w","permission_mode":"dontAsk"}"#,
        )
        .unwrap();
        assert_eq!(input.tool_name, "Bash");
        assert_eq!(input.tool_use_id, "toolu_1");
        assert_eq!(input.tool_input, serde_json::json!({"command":"ls"}));
        assert_eq!(input.cwd.as_deref(), Some("/w"));
    }

    #[test]
    fn missing_keys_are_named() {
        assert_eq!(parse(b"nope"), Err(BadHookInput("不是 JSON")));
        assert_eq!(
            parse(br#"{"tool_input":{},"tool_use_id":"t"}"#),
            Err(BadHookInput("tool_name"))
        );
        assert_eq!(
            parse(br#"{"tool_name":"Bash","tool_input":{}}"#),
            Err(BadHookInput("tool_use_id"))
        );
        assert_eq!(
            parse(br#"{"tool_name":"Bash","tool_use_id":"t"}"#),
            Err(BadHookInput("tool_input"))
        );
    }
}
