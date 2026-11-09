//! AI 审查系统回合的纯部分（T07 方案 6.6；技术设计 7.2 第 7 步、7.2b）：提示词与输出解析。
//!
//! 审查回合不开任何工具，只依据输入的动作、任务目标与上下文摘要作判断。输入输出全文不进事件，
//! 理由进 `ApprovalReviewUpdated`。

/// 审查的输入。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReviewInput {
    /// 全部子动作的一行摘要
    pub actions: Vec<String>,
    pub command_text: String,
    /// 本对话最近一条用户消息
    pub task_goal: String,
    /// 最近不超过 2000 字符的助手输出摘要
    pub recent_context: String,
}

/// 审查结论。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReviewOutcome {
    pub pass: bool,
    pub reason: String,
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("审查输出无法解析：{0}")]
pub struct ReviewParseError(pub String);

/// 上下文摘要的上限（字符）。
pub const CONTEXT_LIMIT: usize = 2000;

const SYSTEM: &str = "你是 iWork 的审批审查员。一个编程助手请求执行一个越出常规边界的动作。\
你看不到也不能调用任何工具，只能依据下面给出的动作、任务目标与上下文判断：这个动作是否是完成任务目标\
所必需、且不会造成用户不希望的后果。拿不准就判不通过，交给用户决定。\
只输出一行 JSON：{\"pass\": true 或 false, \"reason\": \"一句中文理由\"}，不要输出其他内容。";

/// (系统提示词, 用户提示词)。
pub fn review_prompt(input: &ReviewInput) -> (String, String) {
    let context: String = input
        .recent_context
        .chars()
        .rev()
        .take(CONTEXT_LIMIT)
        .collect::<Vec<_>>()
        .into_iter()
        .rev()
        .collect();
    let user = format!(
        "任务目标：\n{}\n\n最近的上下文：\n{}\n\n请求的命令或工具调用：\n{}\n\n归一后的动作：\n{}\n",
        input.task_goal,
        context,
        input.command_text,
        input
            .actions
            .iter()
            .map(|a| format!("- {a}"))
            .collect::<Vec<_>>()
            .join("\n")
    );
    (SYSTEM.to_owned(), user)
}

/// 只认最后一行非空文字里的 JSON `{"pass": bool, "reason": "..."}`。
pub fn parse_review_output(text: &str) -> Result<ReviewOutcome, ReviewParseError> {
    let line = text
        .lines()
        .rev()
        .map(str::trim)
        .find(|l| !l.is_empty())
        .ok_or_else(|| ReviewParseError("输出为空".into()))?;
    let value: serde_json::Value = serde_json::from_str(line)
        .map_err(|e| ReviewParseError(format!("最后一行不是 JSON：{e}")))?;
    let pass = value
        .get("pass")
        .and_then(serde_json::Value::as_bool)
        .ok_or_else(|| ReviewParseError("缺布尔字段 pass".into()))?;
    let reason = value
        .get("reason")
        .and_then(serde_json::Value::as_str)
        .unwrap_or_default()
        .to_owned();
    Ok(ReviewOutcome { pass, reason })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse() {
        assert_eq!(
            parse_review_output("想了想\n{\"pass\": true, \"reason\": \"需要\"}\n"),
            Ok(ReviewOutcome {
                pass: true,
                reason: "需要".into()
            })
        );
        assert!(parse_review_output("{\"pass\": \"yes\"}").is_err());
        assert!(parse_review_output("").is_err());
        assert!(parse_review_output("{\"pass\": true}\n好的").is_err());
    }

    #[test]
    fn prompt_contains_inputs_and_trims_context() {
        let input = ReviewInput {
            actions: vec!["Read ~/Documents/x".into()],
            command_text: "cat ~/Documents/x".into(),
            task_goal: "整理笔记".into(),
            recent_context: "a".repeat(CONTEXT_LIMIT + 10),
        };
        let (system, user) = review_prompt(&input);
        assert!(system.contains("JSON"));
        assert!(
            user.contains("整理笔记")
                && user.contains("cat ~/Documents/x")
                && user.contains("- Read ~/Documents/x")
        );
        assert!(user.matches('a').count() <= CONTEXT_LIMIT + 10);
        assert!(!user.contains(&"a".repeat(CONTEXT_LIMIT + 1)));
    }
}
