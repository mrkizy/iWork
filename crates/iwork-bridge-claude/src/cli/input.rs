//! 用户消息 → stream-json 的一行输入（T06 方案 5.2 第 5 条）。纯函数。
//!
//! 桥接只认两种 ACP 内容块：文字、图片（base64）；资源链接的 uri 是 `file://` 绝对路径时转成一段写明路径的
//! 文字，别的 uri 一律拒绝（`setup_invalid`）。附件怎么变成这两种由附件功能（T10）接。
//! 图片块的写法照 Anthropic 消息格式（`source.type = base64`），2.1.322 上还没录过样本。

use serde_json::{Value, json};

/// ACP 内容块里桥接收的几种（由 S6 的 `acp.rs` 从 ACP 类型转来）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum InputBlock {
    Text(String),
    Image { mime: String, data_base64: String },
    ResourceLink { uri: String },
}

/// 这条用户消息转不了：回 prompt 错误 `setup_invalid`，`reason` 是 `field`。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InputInvalid {
    pub field: &'static str,
}

/// 内容块 → 一行（不带换行符）。
pub fn user_line(blocks: &[InputBlock]) -> Result<String, InputInvalid> {
    let content = blocks
        .iter()
        .map(block)
        .collect::<Result<Vec<Value>, _>>()?;
    Ok(json!({ "type": "user", "message": { "role": "user", "content": content } }).to_string())
}

/// 桥接发给 CLI 的控制请求：`{"type":"control_request","request_id":…,"request":{"subtype":…}}`（一行，不带换行符）。
/// `request_id` 由桥接定，响应按它配对。写法照录制脚本（`tests/fixtures/record.py`）与 t0g 实测过的那种。
pub fn control_request_line(request_id: &str, subtype: &str) -> String {
    json!({ "type": "control_request", "request_id": request_id, "request": { "subtype": subtype } })
        .to_string()
}

fn block(b: &InputBlock) -> Result<Value, InputInvalid> {
    Ok(match b {
        InputBlock::Text(text) => json!({ "type": "text", "text": text }),
        InputBlock::Image { mime, data_base64 } => json!({
            "type": "image",
            "source": { "type": "base64", "media_type": mime, "data": data_base64 },
        }),
        InputBlock::ResourceLink { uri } => {
            let path = file_path(uri).ok_or(InputInvalid {
                field: "prompt.resource_link",
            })?;
            json!({ "type": "text", "text": format!("（用户附加了文件：{path}）") })
        }
    })
}

/// `file:///绝对路径` → 解码后的路径；不是这种形状返回 `None`。
fn file_path(uri: &str) -> Option<String> {
    let rest = uri.strip_prefix("file://")?;
    if !rest.starts_with('/') {
        return None;
    }
    percent_decode(rest)
}

fn percent_decode(s: &str) -> Option<String> {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' {
            let hex = s.get(i + 1..i + 3)?;
            out.push(u8::from_str_radix(hex, 16).ok()?);
            i += 3;
        } else {
            out.push(bytes[i]);
            i += 1;
        }
    }
    String::from_utf8(out).ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn text_and_image() {
        let line = user_line(&[
            InputBlock::Text("看看".into()),
            InputBlock::Image {
                mime: "image/png".into(),
                data_base64: "iVBO".into(),
            },
        ])
        .unwrap();
        assert_eq!(
            serde_json::from_str::<Value>(&line).unwrap(),
            json!({ "type": "user", "message": { "role": "user", "content": [
                { "type": "text", "text": "看看" },
                { "type": "image", "source": { "type": "base64", "media_type": "image/png", "data": "iVBO" } },
            ]}})
        );
        assert!(!line.contains('\n'), "一行");
    }

    #[test]
    fn control_request_shape() {
        let line = control_request_line("init-1", "initialize");
        assert_eq!(
            serde_json::from_str::<Value>(&line).unwrap(),
            json!({ "type": "control_request", "request_id": "init-1", "request": { "subtype": "initialize" } })
        );
        assert!(!line.contains('\n'));
    }

    #[test]
    fn file_link_becomes_text_with_decoded_path() {
        let line = user_line(&[InputBlock::ResourceLink {
            uri: "file:///Users/u/Application%20Support/a.txt".into(),
        }])
        .unwrap();
        assert!(
            line.contains("（用户附加了文件：/Users/u/Application Support/a.txt）"),
            "{line}"
        );
    }

    #[test]
    fn other_links_are_refused() {
        for uri in [
            "iwork-artifact:abc",
            "https://example.org/a",
            "file://relative/a",
            "file:///bad%zz",
        ] {
            assert_eq!(
                user_line(&[InputBlock::ResourceLink { uri: uri.into() }]),
                Err(InputInvalid {
                    field: "prompt.resource_link"
                }),
                "{uri}"
            );
        }
    }
}
