//! 续接摘要（T06 方案 5.2a；需求第 12 章 v1.78）：运行时的会话记录用不了、新开一条会话时，用 iWork 自己存的
//! 对话拼一段上文，放在新会话的第一条输入里。纯函数，不调用 AI，不碰 I/O。
//!
//! 摘要只盖这一轮请求**之前**的消息：这一轮的请求要是进了摘要，就会被开头那句「都已完成，不要重做」盖住。
//! 续接的回合另用 [`request_reminder`] 把请求单独交给模型。

use iwork_ports::read_model::{SummaryLine, SummarySource, SummaryText, SummaryWho};

/// 摘要开头固定的一句。
pub const SUMMARY_HEADER: &str = "[iWork] 以下是此前对话的摘要。其中提到的操作都已完成，不要重做。";

/// 单条超长时截尾后加的标记。
const TRUNCATED: &str = "…（已截断）";

/// 条与条之间的分隔。
const SEPARATOR: &str = "\n\n";

/// 给「（中间省略 N 条）」那一行留的字符数：N 是 u32，整行不会超过它。
const OMISSION_RESERVE: usize = 24;

/// 摘要的字数上限（按字符数算）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SummaryLimits {
    total: usize,
    per_line: usize,
}

impl SummaryLimits {
    /// 初值（方案 5.2a）：整份 12 000 字符，单条 2 000 字符。
    pub const DEFAULT: Self = Self::new(12_000, 2_000);

    /// `total` 至少要装得下开头那句、第一条用户消息、最新的一条和省略行；装不下是调用方写错了常量，直接 panic。
    pub const fn new(total: usize, per_line: usize) -> Self {
        let header = SUMMARY_HEADER.len(); // 字节数 ≥ 字符数，往宽里算
        let floor = header + 2 * (per_line + SEPARATOR.len()) + OMISSION_RESERVE + SEPARATOR.len();
        assert!(
            per_line > TRUNCATED.len() && total >= floor,
            "摘要上限装不下开头、第一条与最新一条"
        );
        Self { total, per_line }
    }

    pub const fn total(self) -> usize {
        self.total
    }

    pub const fn per_line(self) -> usize {
        self.per_line
    }
}

/// 拼摘要。这条会话线上这一轮之前什么都没有时返回 `None`（不发摘要）。
///
/// 先放第一条用户消息；再从最新往前倒推，能装几条装几条，输出按时间正序；中间跳过了内容就插一行
/// 「（中间省略 N 条）」。整份不超 `limits.total()`，每条不超 `limits.per_line()`。
pub fn resume_summary(source: &SummarySource, limits: SummaryLimits) -> Option<String> {
    if source.first_user.is_none() && source.earlier.is_empty() {
        return None;
    }
    let first = source
        .first_user
        .as_ref()
        .map(|l| entry(l, limits.per_line));
    let mut used = chars(SUMMARY_HEADER) + OMISSION_RESERVE + SEPARATOR.len();
    if let Some(first) = &first {
        used += SEPARATOR.len() + chars(first);
    }
    // 从最新往前装，装不下就停：装进去的一定是紧挨着这一轮的连续一段，省略的只在中间
    let mut recent = Vec::new();
    for line in &source.earlier {
        let text = entry(line, limits.per_line);
        let cost = SEPARATOR.len() + chars(&text);
        if used + cost > limits.total {
            break;
        }
        used += cost;
        recent.push(text);
    }
    let omitted = source.unfetched as usize + (source.earlier.len() - recent.len());

    let mut parts = vec![SUMMARY_HEADER.to_owned()];
    parts.extend(first);
    if omitted > 0 {
        parts.push(format!("（中间省略 {omitted} 条）"));
    }
    parts.extend(recent.into_iter().rev());
    Some(parts.join(SEPARATOR))
}

/// 续接的回合要把这一轮用户的请求重新交给模型（它不在摘要里）。请求原文照给，不截断。
pub fn request_reminder(request: &SummaryLine) -> String {
    format!("[iWork] 这一轮用户的请求是：{}", body(request))
}

/// 一条消息在摘要里的样子：「谁：正文（附件：…）」，超出单条上限截尾加标记。
fn entry(line: &SummaryLine, per_line: usize) -> String {
    let who = match &line.who {
        SummaryWho::User => "用户".to_owned(),
        SummaryWho::Assistant { name } => format!("助手（{name}）"),
    };
    let full = format!("{who}：{}", body(line));
    if chars(&full) <= per_line {
        return full;
    }
    let keep = per_line - chars(TRUNCATED);
    let mut out: String = full.chars().take(keep).collect();
    out.push_str(TRUNCATED);
    out
}

/// 正文加附件名。存成制品的长正文只记一行，不去读它。
fn body(line: &SummaryLine) -> String {
    let mut out = match (&line.text, &line.who) {
        (SummaryText::Inline(text), _) => text.clone(),
        (SummaryText::Stored, SummaryWho::User) => "（一条较长的消息，未收入摘要）".to_owned(),
        (SummaryText::Stored, SummaryWho::Assistant { .. }) => {
            "（一条较长的回复，未收入摘要）".to_owned()
        }
    };
    if !line.attachments.is_empty() {
        out.push_str(&format!("（附件：{}）", line.attachments.join("、")));
    }
    out
}

fn chars(s: &str) -> usize {
    s.chars().count()
}

#[cfg(test)]
#[path = "resume_summary_tests.rs"]
mod tests;
