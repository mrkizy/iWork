//! 运行时原始消息的诊断记录（技术设计 6.1：原始事件只用于诊断，脱敏、限量）。
//!
//! 只留最近 [`MAX_ENTRIES`] 条；每条先脱敏再截断。核心看不到这里的内容，只有诊断包会读。

use std::collections::VecDeque;
use std::sync::Mutex;

use serde_json::Value;

/// 最多保留的条数，超过时丢最旧的。
pub const MAX_ENTRIES: usize = 256;
/// 单个字符串值保留的最大字节数，超出部分截掉并注明原长度。
pub const MAX_STRING_BYTES: usize = 512;

/// 键名（小写后）含这些片段的值一律替换为 `<redacted>`：凭据、令牌与个人信息不进诊断。
const SENSITIVE_KEY_PARTS: &[&str] = &[
    "token",
    "secret",
    "password",
    "apikey",
    "api_key",
    "authorization",
    "cookie",
    "credential",
    "email",
];

const REDACTED: &str = "<redacted>";

/// 一个运行时进程的原始消息记录。
#[derive(Debug, Default)]
pub struct RawLog {
    entries: Mutex<VecDeque<String>>,
}

impl RawLog {
    /// 记一行从运行时读到的原始文本。
    pub fn record(&self, line: &str) {
        let entry = sanitize_line(line);
        let mut entries = self.entries.lock().unwrap_or_else(|p| p.into_inner());
        if entries.len() == MAX_ENTRIES {
            entries.pop_front();
        }
        entries.push_back(entry);
    }

    /// 当前保留的记录，旧的在前。
    pub fn snapshot(&self) -> Vec<String> {
        let entries = self.entries.lock().unwrap_or_else(|p| p.into_inner());
        entries.iter().cloned().collect()
    }
}

/// 脱敏并截断一行。不是 JSON 的行只记长度：看不出结构就没法确定哪部分是敏感的。
pub fn sanitize_line(line: &str) -> String {
    match serde_json::from_str::<Value>(line) {
        Ok(mut value) => {
            sanitize_value(&mut value);
            value.to_string()
        }
        Err(_) => format!("<非 JSON 行，{} 字节>", line.len()),
    }
}

fn is_sensitive(key: &str) -> bool {
    let key = key.to_ascii_lowercase();
    SENSITIVE_KEY_PARTS.iter().any(|part| key.contains(part))
}

fn sanitize_value(value: &mut Value) {
    match value {
        Value::Object(map) => {
            for (key, v) in map.iter_mut() {
                if is_sensitive(key) {
                    *v = Value::String(REDACTED.into());
                } else {
                    sanitize_value(v);
                }
            }
        }
        Value::Array(items) => items.iter_mut().for_each(sanitize_value),
        Value::String(s) if s.len() > MAX_STRING_BYTES => {
            let mut cut = MAX_STRING_BYTES;
            while !s.is_char_boundary(cut) {
                cut -= 1;
            }
            let original = s.len();
            s.truncate(cut);
            s.push_str(&format!("…<截断，原长 {original} 字节>"));
        }
        _ => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sensitive_keys_are_redacted_at_any_depth() {
        let line = r#"{"params":{"auth":{"accessToken":"sk-123","Email":"a@b.c"},"text":"hi"}}"#;
        let out = sanitize_line(line);
        assert!(!out.contains("sk-123"), "{out}");
        assert!(!out.contains("a@b.c"), "{out}");
        assert!(
            out.contains(r#""text":"hi""#),
            "对照：普通字段原样保留：{out}"
        );
    }

    #[test]
    fn long_strings_are_truncated_on_char_boundary() {
        let long = "字".repeat(400); // 1200 字节
        let out = sanitize_line(&serde_json::json!({ "text": long }).to_string());
        let v: Value = serde_json::from_str(&out).unwrap();
        let text = v["text"].as_str().unwrap();
        assert!(text.contains("原长 1200 字节"), "{text}");
        assert!(text.len() < 700);
    }

    #[test]
    fn non_json_lines_keep_only_length() {
        assert_eq!(sanitize_line("password=hunter2"), "<非 JSON 行，16 字节>");
    }

    #[test]
    fn log_keeps_only_the_most_recent_entries() {
        let log = RawLog::default();
        for i in 0..MAX_ENTRIES + 3 {
            log.record(&format!(r#"{{"n":{i}}}"#));
        }
        let snap = log.snapshot();
        assert_eq!(snap.len(), MAX_ENTRIES);
        assert_eq!(snap[0], r#"{"n":3}"#);
        assert_eq!(
            snap.last().unwrap(),
            &format!(r#"{{"n":{}}}"#, MAX_ENTRIES + 2)
        );
    }
}
