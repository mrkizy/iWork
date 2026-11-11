//! 运行时给的工具输入（JSON 文本）：取字段、规范化成摘要用的文字（T07 方案 3.1、6.2）。

use serde_json::Value;

/// 解析好的工具输入。解析失败时为空对象：取不到必要字段的调用按「解析不了」处理。
#[derive(Debug, Clone)]
pub struct ToolInput {
    value: Value,
    /// 原文能否解析为 JSON 对象
    pub well_formed: bool,
}

impl ToolInput {
    pub fn parse(raw: &str) -> Self {
        match serde_json::from_str::<Value>(raw) {
            Ok(value @ Value::Object(_)) => Self {
                value,
                well_formed: true,
            },
            _ => Self {
                value: Value::Object(Default::default()),
                well_formed: false,
            },
        }
    }

    /// 字符串字段；不存在或不是字符串返回 `None`。
    pub fn str(&self, field: &str) -> Option<&str> {
        self.value.get(field).and_then(Value::as_str)
    }

    /// 布尔字段；不存在或不是布尔按 `false`。
    pub fn flag(&self, field: &str) -> bool {
        self.value
            .get(field)
            .and_then(Value::as_bool)
            .unwrap_or(false)
    }

    /// 规范化 JSON：对象键排序、无空白。原文解析不了时用原文本身（仍能区分不同输入）。
    pub fn canonical(&self, raw: &str) -> String {
        if !self.well_formed {
            return raw.to_owned();
        }
        let mut out = String::new();
        write_canonical(&self.value, &mut out);
        out
    }
}

fn write_canonical(v: &Value, out: &mut String) {
    match v {
        Value::Object(map) => {
            let mut keys: Vec<&String> = map.keys().collect();
            keys.sort();
            out.push('{');
            for (i, k) in keys.into_iter().enumerate() {
                if i > 0 {
                    out.push(',');
                }
                out.push_str(&Value::String(k.clone()).to_string());
                out.push(':');
                write_canonical(&map[k], out);
            }
            out.push('}');
        }
        Value::Array(items) => {
            out.push('[');
            for (i, item) in items.iter().enumerate() {
                if i > 0 {
                    out.push(',');
                }
                write_canonical(item, out);
            }
            out.push(']');
        }
        scalar => out.push_str(&scalar.to_string()),
    }
}

/// URL 的主机名（`WebFetch` 的目标）。取不出来返回整串。
pub fn url_host(url: &str) -> String {
    let rest = url.split_once("://").map_or(url, |(_, r)| r);
    let host = rest.split(['/', '?', '#']).next().unwrap_or(rest);
    let host = host.rsplit_once('@').map_or(host, |(_, h)| h);
    host.to_owned()
}
