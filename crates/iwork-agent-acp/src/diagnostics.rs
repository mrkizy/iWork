//! 运行时消息的诊断记录（技术设计 6.1：原始事件只用于诊断，脱敏、限量）。
//!
//! **只记允许名单里的协议元数据**：消息种类、方法名、请求 id、`session/update` 的更新类型、
//! 错误码、字节数。正文、工具输入输出、环境变量、错误说明一概不记：按键名找敏感字段的
//! 黑名单做法挡不住 `text`、`rawInput.command` 这类值里的凭据。运行时的 stderr 只记行长度。
//!
//! 限量：每条摘要不超过 [`MAX_ENTRY_BYTES`]，整份记录不超过 [`MAX_TOTAL_BYTES`]、[`MAX_ENTRIES`] 条，
//! 超出时丢最旧的。

use std::collections::VecDeque;
use std::sync::Mutex;

use serde_json::{Map, Value, json};

/// 最多保留的条数。
pub const MAX_ENTRIES: usize = 256;
/// 整份记录的字节上限。
pub const MAX_TOTAL_BYTES: usize = 64 * 1024;
/// 单条摘要的字节上限。
pub const MAX_ENTRY_BYTES: usize = 512;
/// 摘要里每个字符串字段（方法名、id、更新类型）保留的最大字符数。
pub const MAX_FIELD_CHARS: usize = 64;

/// 一个运行时进程的诊断记录。
#[derive(Debug, Default)]
pub struct RawLog {
    inner: Mutex<Entries>,
}

#[derive(Debug, Default)]
struct Entries {
    items: VecDeque<String>,
    bytes: usize,
}

impl RawLog {
    /// 记一行从运行时 stdout 读到的协议消息（只记摘要）。
    pub fn record_incoming(&self, line: &str) {
        self.push(summarize(line));
    }

    /// 记一行运行时的 stderr 输出：内容可能是任何东西，只记长度。
    pub fn record_stderr(&self, line_bytes: usize) {
        self.push(json!({ "stream": "stderr", "bytes": line_bytes }).to_string());
    }

    /// 记一条被拒收的入站消息（超长或不是 UTF-8）。内容不记。
    pub fn record_rejected_frame(&self) {
        self.push(json!({ "stream": "stdout", "rejected": true }).to_string());
    }

    /// 记一条约定项读不懂、按拒绝答复了的权限请求。内容不记。
    pub fn record_bad_permission_meta(&self) {
        self.push(json!({ "stream": "stdout", "bad_permission_meta": true }).to_string());
    }

    /// 记一条解不出来的运行时上报（`_meta.iwork.report`）。内容不记，上报本身丢弃。
    pub fn record_bad_report(&self) {
        self.push(json!({ "stream": "stdout", "bad_report": true }).to_string());
    }

    fn push(&self, mut entry: String) {
        if entry.len() > MAX_ENTRY_BYTES {
            // 摘要字段都有长度上限，正常到不了这里；万一到了，截断成合法 UTF-8
            let mut cut = MAX_ENTRY_BYTES;
            while !entry.is_char_boundary(cut) {
                cut -= 1;
            }
            entry.truncate(cut);
        }
        let mut e = self.inner.lock().unwrap_or_else(|p| p.into_inner());
        e.bytes += entry.len();
        e.items.push_back(entry);
        while e.items.len() > MAX_ENTRIES || e.bytes > MAX_TOTAL_BYTES {
            let old = e.items.pop_front().expect("条数或字节超限时队列非空");
            e.bytes -= old.len();
        }
    }

    /// 当前保留的记录，旧的在前。
    pub fn snapshot(&self) -> Vec<String> {
        let e = self.inner.lock().unwrap_or_else(|p| p.into_inner());
        e.items.iter().cloned().collect()
    }

    /// 当前保留的总字节数。
    pub fn total_bytes(&self) -> usize {
        self.inner.lock().unwrap_or_else(|p| p.into_inner()).bytes
    }
}

fn short(value: Option<&Value>) -> Option<Value> {
    let s = match value? {
        Value::String(s) => s.clone(),
        Value::Number(n) => n.to_string(),
        _ => return None,
    };
    Some(Value::String(s.chars().take(MAX_FIELD_CHARS).collect()))
}

/// 一行协议消息的摘要。不是 JSON 的行只记长度。
pub fn summarize(line: &str) -> String {
    let mut out = Map::new();
    out.insert("bytes".into(), json!(line.len()));
    let Ok(Value::Object(msg)) = serde_json::from_str::<Value>(line) else {
        out.insert("kind".into(), json!("non_json"));
        return Value::Object(out).to_string();
    };
    let kind = match (msg.contains_key("method"), msg.contains_key("id")) {
        (true, true) => "request",
        (true, false) => "notification",
        (false, _) if msg.contains_key("error") => "error",
        (false, _) => "response",
    };
    out.insert("kind".into(), json!(kind));
    if let Some(v) = short(msg.get("method")) {
        out.insert("method".into(), v);
    }
    if let Some(v) = short(msg.get("id")) {
        out.insert("id".into(), v);
    }
    let update = msg
        .get("params")
        .and_then(|p| p.get("update"))
        .and_then(|u| u.get("sessionUpdate"));
    if let Some(v) = short(update) {
        out.insert("update".into(), v);
    }
    if let Some(code) = msg
        .get("error")
        .and_then(|e| e.get("code"))
        .and_then(Value::as_i64)
    {
        out.insert("errorCode".into(), json!(code));
    }
    Value::Object(out).to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_protocol_metadata_survives() {
        let line = json!({
            "jsonrpc": "2.0", "method": "session/update",
            "params": {
                "sessionId": "s1",
                "update": {
                    "sessionUpdate": "tool_call", "toolCallId": "t1",
                    "rawInput": { "command": "curl -H 'X: hunter2' https://x" },
                    "env": { "CUSTOM_AUTH": "sk-live-123" },
                    "content": [{ "type": "text", "text": "我的密码是 p@ss" }]
                }
            }
        })
        .to_string();
        let out = summarize(&line);
        for secret in ["hunter2", "sk-live-123", "p@ss", "CUSTOM_AUTH", "curl"] {
            assert!(!out.contains(secret), "{secret} 泄漏：{out}");
        }
        let v: Value = serde_json::from_str(&out).unwrap();
        assert_eq!(v["kind"], "notification");
        assert_eq!(v["method"], "session/update");
        assert_eq!(v["update"], "tool_call");
        assert_eq!(v["bytes"], line.len());
    }

    #[test]
    fn error_responses_keep_code_not_message() {
        let line = r#"{"jsonrpc":"2.0","id":"7","error":{"code":-32603,"message":"token=abc"}}"#;
        let v: Value = serde_json::from_str(&summarize(line)).unwrap();
        assert_eq!(v["kind"], "error");
        assert_eq!(v["errorCode"], -32603);
        assert!(!summarize(line).contains("abc"));
    }

    #[test]
    fn non_json_lines_keep_only_length() {
        let v: Value = serde_json::from_str(&summarize("password=hunter2")).unwrap();
        assert_eq!(v, json!({ "bytes": 16, "kind": "non_json" }));
    }

    #[test]
    fn long_field_values_are_cut() {
        let method = "m".repeat(10_000);
        let out = summarize(&json!({ "method": method, "id": "x".repeat(5000) }).to_string());
        assert!(out.len() < MAX_ENTRY_BYTES, "{}", out.len());
    }

    #[test]
    fn many_short_fields_do_not_grow_an_entry() {
        let mut params = Map::new();
        for i in 0..100_000 {
            params.insert(format!("k{i}"), json!("v"));
        }
        let line = json!({ "method": "x", "params": params }).to_string();
        assert!(line.len() > 1_000_000);
        assert!(summarize(&line).len() < 200);
    }

    #[test]
    fn log_respects_entry_count_and_byte_budget() {
        let log = RawLog::default();
        for i in 0..10_000 {
            log.record_incoming(&json!({ "method": "m".repeat(64), "id": i }).to_string());
            assert!(log.total_bytes() <= MAX_TOTAL_BYTES);
        }
        let snap = log.snapshot();
        assert!(snap.len() <= MAX_ENTRIES);
        assert_eq!(
            snap.iter().map(String::len).sum::<usize>(),
            log.total_bytes()
        );
        // 留下的是最新的
        assert!(snap.last().unwrap().contains("9999"), "{:?}", snap.last());
    }

    #[test]
    fn byte_budget_evicts_before_entry_count_does() {
        let log = RawLog::default();
        for _ in 0..300 {
            log.push("x".repeat(MAX_ENTRY_BYTES));
        }
        assert!(log.total_bytes() <= MAX_TOTAL_BYTES);
        assert_eq!(log.snapshot().len(), MAX_TOTAL_BYTES / MAX_ENTRY_BYTES);
        // 单条超长的被截到上限
        log.push("y".repeat(MAX_ENTRY_BYTES * 10));
        assert_eq!(log.snapshot().last().unwrap().len(), MAX_ENTRY_BYTES);
    }
}
