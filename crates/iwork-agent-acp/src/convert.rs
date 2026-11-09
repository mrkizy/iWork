//! 两个方向的形状转换，全是纯函数：
//! - 下行：`SessionSpec` 里随会话下发的配置 → `agent.proto` 的 `SessionSetup` → ACP `_meta`；
//!   prompt 内容块 → ACP 内容块。
//! - 上行：ACP `session/update` → [`AgentUpdate`]（厂商原始事件在这一层标准化，技术设计 6.1）；
//!   权限请求 ↔ [`PermissionRequest`] / [`PermissionDecision`]。

use std::path::Path;

use agent_client_protocol::v1::schema::{
    ContentBlock as AcpContent, Meta, PermissionOption, PermissionOptionKind,
    RequestPermissionOutcome, RequestPermissionRequest, RequestPermissionResponse, ResourceLink,
    SelectedPermissionOutcome, SessionUpdate, StopReason, TextContent, ToolCallContent,
    ToolCallStatus,
};
use base64::Engine as _;
use base64::engine::general_purpose::STANDARD as BASE64;
use iwork_domain::action::ToolRef;
use iwork_domain::common::{ModelSelection, ToolGroup};
use iwork_domain::event::ToolCallId;
use iwork_domain::session::{
    AllowWrite, CommandPattern, NetworkPolicy, SessionConstraints, SessionToolSet,
};
use iwork_ports::agent::{
    AgentError, AgentUpdate, ContentBlock, PermissionDecision, PermissionRequest, RawToolInput,
    TurnOutcome,
};
use iwork_proto::pb;
use prost::Message as _;
use serde_json::{Map, Value, json};

/// iWork 在 ACP `_meta` 里占用的键。
pub const META_KEY: &str = "iwork";
/// `_meta["iwork"]` 下放会话配置（base64 编码的 `SessionSetup`）的键。
pub const SESSION_SETUP_KEY: &str = "sessionSetup";
/// `_meta["iwork"]` 下放底层 CLI 信息的键（`initialize` 响应）。
pub const CLI_KEY: &str = "cli";
/// `_meta["iwork"]` 下放拒绝理由、改写后输入的键（权限响应）。
pub const DENY_MESSAGE_KEY: &str = "message";
pub const UPDATED_INPUT_KEY: &str = "updatedInput";

// ---------- 下行 ----------

/// 把会话配置编码进 ACP `_meta`。路径不是 UTF-8 时报错：proto 里路径是字符串，
/// 有损转换会让沙箱规则指向别的路径。
pub fn session_setup_meta(
    model: &ModelSelection,
    constraints: &SessionConstraints,
    tool_set: &SessionToolSet,
) -> Result<Meta, AgentError> {
    let setup = pb::SessionSetup {
        model: Some(model_to_proto(model)),
        constraints: Some(constraints_to_proto(constraints)?),
        tool_set: Some(tool_set_to_proto(tool_set)),
    };
    let encoded = BASE64.encode(setup.encode_to_vec());
    let mut meta = Map::new();
    meta.insert(META_KEY.into(), json!({ SESSION_SETUP_KEY: encoded }));
    Ok(meta)
}

/// 从 `_meta` 解出会话配置。桥接与假运行时用同一个约定；这里给测试对拍用。
pub fn decode_session_setup(meta: Option<&Meta>) -> Result<pb::SessionSetup, String> {
    let encoded = meta
        .and_then(|m| m.get(META_KEY))
        .and_then(|v| v.get(SESSION_SETUP_KEY))
        .and_then(Value::as_str)
        .ok_or_else(|| format!("_meta.{META_KEY}.{SESSION_SETUP_KEY} 缺失"))?;
    let bytes = BASE64
        .decode(encoded)
        .map_err(|e| format!("_meta.{META_KEY}.{SESSION_SETUP_KEY} 不是 base64：{e}"))?;
    pb::SessionSetup::decode(bytes.as_slice())
        .map_err(|e| format!("_meta.{META_KEY}.{SESSION_SETUP_KEY} 解码失败：{e}"))
}

fn model_to_proto(m: &ModelSelection) -> pb::ModelSelection {
    pb::ModelSelection {
        model_id: m.model_id.0.clone(),
        effort: m.effort.as_ref().map(|e| e.0.clone()),
    }
}

fn path_str(path: &Path) -> Result<String, AgentError> {
    path.to_str()
        .map(str::to_owned)
        .ok_or_else(|| AgentError::Protocol {
            message: format!("路径不是 UTF-8，不能下发给运行时：{}", path.display()),
        })
}

fn paths(list: &[std::path::PathBuf]) -> Result<Vec<String>, AgentError> {
    list.iter().map(|p| path_str(p)).collect()
}

fn constraints_to_proto(c: &SessionConstraints) -> Result<pb::SessionConstraints, AgentError> {
    let kind = match &c.allow_write {
        AllowWrite::Whitelist(list) => pb::allow_write::Kind::Whitelist(pb::WritablePaths {
            paths: paths(list)?,
        }),
        AllowWrite::Anywhere => pb::allow_write::Kind::Anywhere(pb::AnywhereWritable {}),
    };
    let network = match c.network {
        NetworkPolicy::Open => pb::NetworkPolicy::Open,
    };
    Ok(pb::SessionConstraints {
        deny_read_write: paths(&c.deny_read_write)?,
        read_only: paths(&c.read_only)?,
        allow_write: Some(pb::AllowWrite { kind: Some(kind) }),
        excluded_commands: c.excluded_commands.iter().map(command_to_proto).collect(),
        allow_unsandboxed_retry: c.allow_unsandboxed_retry,
        network: network.into(),
    })
}

fn command_to_proto(c: &CommandPattern) -> pb::CommandPattern {
    pb::CommandPattern {
        program: c.program.clone(),
        subcommand: c.subcommand.clone(),
    }
}

fn tool_set_to_proto(t: &SessionToolSet) -> pb::SessionToolSet {
    let group = |g: &ToolGroup| -> i32 {
        match g {
            ToolGroup::ReadAndSearch => pb::ToolGroup::ReadAndSearch,
            ToolGroup::EditFiles => pb::ToolGroup::EditFiles,
            ToolGroup::RunCommands => pb::ToolGroup::RunCommands,
            ToolGroup::Web => pb::ToolGroup::Web,
        }
        .into()
    };
    pb::SessionToolSet {
        enabled_groups: t.enabled_groups.iter().map(group).collect(),
        include_always_on: t.include_always_on,
    }
}

/// prompt 内容块。附件只发引用（`iwork-artifact:` + 摘要），怎么交给运行时读取由附件功能（T10）定。
pub fn content_to_acp(block: &ContentBlock) -> AcpContent {
    match block {
        ContentBlock::Text(text) => AcpContent::Text(TextContent::new(text.clone())),
        ContentBlock::Attachment {
            artifact,
            mime,
            name,
        } => AcpContent::ResourceLink(
            ResourceLink::new(name.clone(), format!("iwork-artifact:{}", artifact.sha256))
                .mime_type(mime.clone()),
        ),
    }
}

// ---------- 上行 ----------

/// 一次 prompt 的上行转换状态：正文偏移要跨多条 `session/update` 累计。
#[derive(Debug, Default)]
pub struct UpdateMapper {
    text_offset: u64,
}

impl UpdateMapper {
    /// 转一条 `session/update`。与回合无关的更新（思考、计划、命令清单、模式等）返回 `None`，
    /// 只进诊断记录。
    pub fn map(&mut self, update: SessionUpdate) -> Option<AgentUpdate> {
        match update {
            SessionUpdate::AgentMessageChunk(chunk) => match chunk.content {
                AcpContent::Text(t) if !t.text.is_empty() => {
                    let offset = self.text_offset;
                    self.text_offset += t.text.len() as u64;
                    Some(AgentUpdate::Text {
                        offset,
                        text: t.text,
                    })
                }
                _ => None,
            },
            SessionUpdate::ToolCall(call) => Some(AgentUpdate::ToolCallStarted {
                tool_call_id: ToolCallId(call.tool_call_id.0.to_string()),
                tool: ToolRef(call.name.unwrap_or(call.title)),
                raw_input: raw_input(call.raw_input.as_ref()),
            }),
            SessionUpdate::ToolCallUpdate(update) => {
                let ok = match update.fields.status? {
                    ToolCallStatus::Completed => true,
                    ToolCallStatus::Failed => false,
                    _ => return None,
                };
                Some(AgentUpdate::ToolCallFinished {
                    tool_call_id: ToolCallId(update.tool_call_id.0.to_string()),
                    ok,
                    output: tool_output(
                        update.fields.content.as_deref(),
                        update.fields.raw_output.as_ref(),
                    ),
                })
            }
            _ => None,
        }
    }
}

fn raw_input(value: Option<&Value>) -> RawToolInput {
    RawToolInput(value.unwrap_or(&Value::Null).to_string())
}

/// 工具输出：优先取文字内容；没有文字时用 `rawOutput` 的 JSON 文本。
fn tool_output(content: Option<&[ToolCallContent]>, raw: Option<&Value>) -> String {
    let text: Vec<&str> = content
        .unwrap_or_default()
        .iter()
        .filter_map(|c| match c {
            ToolCallContent::Content(c) => match &c.content {
                AcpContent::Text(t) => Some(t.text.as_str()),
                _ => None,
            },
            _ => None,
        })
        .collect();
    if !text.is_empty() {
        return text.join("\n");
    }
    raw.map(Value::to_string).unwrap_or_default()
}

/// prompt 响应的停止原因 → 回合结局。只有对端明说取消的才算取消，其余都是正常结束。
pub fn outcome_of(stop: StopReason) -> TurnOutcome {
    match stop {
        StopReason::Cancelled => TurnOutcome::Cancelled,
        _ => TurnOutcome::Completed,
    }
}

/// ACP 权限请求 → 端口的权限请求。
///
/// `bypass_sandbox`：运行时在原始输入里声明要绕过沙箱（agy 的 `rawInput.BypassSandbox`，
/// AGENTS.md 安全不变量），按「越界」交给审批判定。
pub fn permission_request(req: &RequestPermissionRequest) -> PermissionRequest {
    let fields = &req.tool_call.fields;
    let tool = fields
        .name
        .clone()
        .or_else(|| fields.title.clone())
        .unwrap_or_else(|| req.tool_call.tool_call_id.0.to_string());
    let bypass_sandbox = fields
        .raw_input
        .as_ref()
        .and_then(|v| v.get("BypassSandbox"))
        .and_then(Value::as_bool)
        .unwrap_or(false);
    PermissionRequest {
        tool: ToolRef(tool),
        raw_input: raw_input(fields.raw_input.as_ref()),
        bypass_sandbox,
    }
}

/// 端口的判定 → ACP 权限响应。按选项的 `kind` 选，不按位置或名字。
///
/// 失败即关闭：对端没给「允许一次」选项时放行无从表达，回 `Cancelled`（对端按未获准处理）；
/// 没给「拒绝一次」时同样回 `Cancelled`。拒绝理由、改写后的输入放进 `_meta["iwork"]`。
pub fn permission_response(
    options: &[PermissionOption],
    decision: &PermissionDecision,
) -> RequestPermissionResponse {
    let pick = |kind: PermissionOptionKind| {
        options
            .iter()
            .find(|o| o.kind == kind)
            .map(|o| o.option_id.clone())
    };
    let (option, extra) = match decision {
        PermissionDecision::Allow => (pick(PermissionOptionKind::AllowOnce), None),
        PermissionDecision::AllowRewritten { new_input } => (
            pick(PermissionOptionKind::AllowOnce),
            Some((UPDATED_INPUT_KEY, Value::String(new_input.0.clone()))),
        ),
        PermissionDecision::Deny { message } => (
            pick(PermissionOptionKind::RejectOnce),
            Some((DENY_MESSAGE_KEY, Value::String(message.clone()))),
        ),
    };
    let outcome = match option {
        Some(id) => RequestPermissionOutcome::Selected(SelectedPermissionOutcome::new(id)),
        None => return RequestPermissionResponse::new(RequestPermissionOutcome::Cancelled),
    };
    let mut response = RequestPermissionResponse::new(outcome);
    if let Some((key, value)) = extra {
        let mut inner = Map::new();
        inner.insert(key.into(), value);
        let mut meta = Map::new();
        meta.insert(META_KEY.into(), Value::Object(inner));
        response = response.meta(meta);
    }
    response
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeSet;
    use std::path::PathBuf;

    use agent_client_protocol::v1::schema::{
        ContentChunk, ToolCall, ToolCallUpdate, ToolCallUpdateFields,
    };
    use iwork_domain::common::{EffortLevel, ModelId};

    use super::*;

    fn chunk(text: &str) -> SessionUpdate {
        SessionUpdate::AgentMessageChunk(ContentChunk::new(AcpContent::Text(TextContent::new(
            text,
        ))))
    }

    #[test]
    fn text_offsets_count_utf8_bytes_across_chunks() {
        let mut m = UpdateMapper::default();
        let a = m.map(chunk("你好")).unwrap();
        let b = m.map(chunk("ab")).unwrap();
        assert_eq!(
            a,
            AgentUpdate::Text {
                offset: 0,
                text: "你好".into()
            }
        );
        assert_eq!(
            b,
            AgentUpdate::Text {
                offset: 6,
                text: "ab".into()
            }
        );
    }

    #[test]
    fn tool_call_and_terminal_update_map_to_start_and_finish() {
        let mut m = UpdateMapper::default();
        let started = m
            .map(SessionUpdate::ToolCall(
                ToolCall::new("t1", "运行 ls")
                    .name("Bash".to_string())
                    .raw_input(json!({"command": "ls"})),
            ))
            .unwrap();
        assert_eq!(
            started,
            AgentUpdate::ToolCallStarted {
                tool_call_id: ToolCallId("t1".into()),
                tool: ToolRef("Bash".into()),
                raw_input: RawToolInput(r#"{"command":"ls"}"#.into()),
            }
        );
        // 进行中的更新不产生事件
        let progress = ToolCallUpdate::new(
            "t1",
            ToolCallUpdateFields::new().status(ToolCallStatus::InProgress),
        );
        assert_eq!(m.map(SessionUpdate::ToolCallUpdate(progress)), None);
        let done = ToolCallUpdate::new(
            "t1",
            ToolCallUpdateFields::new()
                .status(ToolCallStatus::Failed)
                .content(vec!["boom".into()]),
        );
        assert_eq!(
            m.map(SessionUpdate::ToolCallUpdate(done)),
            Some(AgentUpdate::ToolCallFinished {
                tool_call_id: ToolCallId("t1".into()),
                ok: false,
                output: "boom".into(),
            })
        );
    }

    fn options() -> Vec<PermissionOption> {
        vec![
            PermissionOption::new("yes", "允许", PermissionOptionKind::AllowOnce),
            PermissionOption::new("no", "拒绝", PermissionOptionKind::RejectOnce),
        ]
    }

    fn selected(r: &RequestPermissionResponse) -> Option<String> {
        match &r.outcome {
            RequestPermissionOutcome::Selected(s) => Some(s.option_id.0.to_string()),
            _ => None,
        }
    }

    #[test]
    fn decisions_pick_options_by_kind_and_carry_message() {
        let allow = permission_response(&options(), &PermissionDecision::Allow);
        assert_eq!(selected(&allow).as_deref(), Some("yes"));
        let deny = permission_response(
            &options(),
            &PermissionDecision::Deny {
                message: "不行".into(),
            },
        );
        assert_eq!(selected(&deny).as_deref(), Some("no"));
        assert_eq!(deny.meta.unwrap()[META_KEY][DENY_MESSAGE_KEY], "不行");
    }

    #[test]
    fn missing_option_fails_closed_as_cancelled() {
        let only_reject = vec![PermissionOption::new(
            "no",
            "拒绝",
            PermissionOptionKind::RejectOnce,
        )];
        let r = permission_response(&only_reject, &PermissionDecision::Allow);
        assert!(matches!(r.outcome, RequestPermissionOutcome::Cancelled));
        let only_allow = vec![PermissionOption::new(
            "yes",
            "允许",
            PermissionOptionKind::AllowOnce,
        )];
        let r = permission_response(
            &only_allow,
            &PermissionDecision::Deny {
                message: "x".into(),
            },
        );
        assert!(matches!(r.outcome, RequestPermissionOutcome::Cancelled));
    }

    #[test]
    fn session_setup_round_trips_through_meta() {
        let model = ModelSelection {
            model_id: ModelId("m1".into()),
            effort: Some(EffortLevel("high".into())),
        };
        let constraints = SessionConstraints {
            deny_read_write: vec![PathBuf::from("/home/u/.ssh")],
            read_only: vec![PathBuf::from("/data/attachments")],
            allow_write: AllowWrite::Whitelist(vec![PathBuf::from("/work")]),
            excluded_commands: vec![CommandPattern {
                program: "gh".into(),
                subcommand: Some("pr".into()),
            }],
            allow_unsandboxed_retry: true,
            network: NetworkPolicy::Open,
        };
        let tool_set = SessionToolSet {
            enabled_groups: BTreeSet::from([ToolGroup::EditFiles, ToolGroup::Web]),
            include_always_on: false,
        };
        let meta = session_setup_meta(&model, &constraints, &tool_set).unwrap();
        let setup = decode_session_setup(Some(&meta)).unwrap();
        assert_eq!(setup.model.unwrap().model_id, "m1");
        let c = setup.constraints.unwrap();
        assert_eq!(c.deny_read_write, vec!["/home/u/.ssh".to_string()]);
        assert_eq!(
            c.allow_write.unwrap().kind,
            Some(pb::allow_write::Kind::Whitelist(pb::WritablePaths {
                paths: vec!["/work".into()]
            }))
        );
        assert_eq!(
            setup.tool_set.unwrap().enabled_groups,
            vec![
                i32::from(pb::ToolGroup::EditFiles),
                i32::from(pb::ToolGroup::Web)
            ]
        );
        assert!(decode_session_setup(None).is_err());
    }

    #[cfg(unix)]
    #[test]
    fn non_utf8_path_is_rejected_not_mangled() {
        use std::ffi::OsStr;
        use std::os::unix::ffi::OsStrExt;
        let bad = PathBuf::from(OsStr::from_bytes(b"/tmp/\xff"));
        let err = path_str(&bad).unwrap_err();
        assert!(matches!(err, AgentError::Protocol { .. }));
    }
}
