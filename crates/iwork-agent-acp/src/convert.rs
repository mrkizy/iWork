//! 两个方向的形状转换，全是纯函数：
//! - 下行：`SessionSpec` 里随会话下发的配置 → `agent.proto` 的 `SessionSetup` → ACP `_meta`；
//!   prompt 内容块 → ACP 内容块。
//! - 上行：ACP `session/update` → [`AgentUpdate`]（厂商原始事件在这一层标准化，技术设计 6.1）；
//!   权限请求 ↔ [`PermissionRequest`] / [`PermissionDecision`]；`_meta["iwork"]` 下的约定项
//!   （T06 方案 2.5 一张表）在边界上一次解析成类型，值不对就报错、不猜。

use std::path::{Path, PathBuf};
use std::time::Duration;

use agent_client_protocol::v1::schema::{
    ContentBlock as AcpContent, Meta, PermissionOption, PermissionOptionKind,
    RequestPermissionOutcome, RequestPermissionRequest, RequestPermissionResponse, ResourceLink,
    SelectedPermissionOutcome, SessionUpdate, StopReason, TextContent, ToolCallContent,
    ToolCallStatus,
};
use base64::Engine as _;
use base64::engine::general_purpose::STANDARD as BASE64;
use iwork_domain::action::ToolRef;
use iwork_domain::common::{EffortLevel, ModelId, ModelSelection, Timestamp, ToolGroup, Usage};
use iwork_domain::event::ToolCallId;
use iwork_domain::session::{
    AllowWrite, CommandPattern, NetworkPolicy, SessionConstraints, SessionToolSet,
};
use iwork_ports::agent::{
    AgentError, AgentUpdate, ContentBlock, ModelOption, PermissionDecision, PermissionRequest,
    QuotaReading, RawToolInput, SandboxPlatform, SandboxStatus, SessionSpec, TurnOutcome,
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
/// `initialize` 响应：运行时等一个权限请求最多多少秒（正整数）。没有这个键 = 不限。
pub const PERMISSION_WAIT_LIMIT_KEY: &str = "permissionWaitLimitSecs";
/// `initialize` 响应：平台上有没有沙箱，`"supported"` / `"unsupported"`。
pub const SANDBOX_PLATFORM_KEY: &str = "sandboxPlatform";
/// `initialize` 响应：思考强度可选值（字符串数组）。
pub const EFFORT_LEVELS_KEY: &str = "effortLevels";
/// `initialize` 响应：可选模型（`[{id, label}]`）。
pub const MODELS_KEY: &str = "models";
/// `session/new`、`session/load` 响应：这条会话的沙箱，`"enforced"` / `"unavailable"`。
pub const SANDBOX_KEY: &str = "sandbox";
/// `session/load` 响应：续接后运行时报的会话 id（与请求里的不同时才带）。
pub const CLI_SESSION_ID_KEY: &str = "cliSessionId";
/// `session/update` 通知：base64 编码的 `RuntimeReport`。
pub const REPORT_KEY: &str = "report";
/// `session/request_permission` 请求：这次调用的工作目录。
pub const CWD_KEY: &str = "cwd";

/// 取 `_meta["iwork"][key]`。没有 `_meta` 或没有 `iwork` 是「没报」；`iwork` 在而不是对象是错误：
/// 约定项一个都读不出来，不能当成一个都没报。
fn iwork_value<'a>(meta: Option<&'a Meta>, key: &str) -> Result<Option<&'a Value>, String> {
    match meta.and_then(|m| m.get(META_KEY)) {
        None => Ok(None),
        Some(Value::Object(iwork)) => Ok(iwork.get(key)),
        Some(_) => Err(format!("_meta.{META_KEY} 须是对象")),
    }
}

// ---------- 下行 ----------

/// 把会话配置编码进 ACP `_meta`。路径不是 UTF-8 时报错：proto 里路径是字符串，
/// 有损转换会让沙箱规则指向别的路径。
pub fn session_setup_meta(spec: &SessionSpec) -> Result<Meta, AgentError> {
    let setup = session_setup(
        &spec.model,
        &spec.constraints,
        &spec.tool_set,
        &spec.disabled_tools,
        &spec.plugin_dirs,
    )?;
    let encoded = BASE64.encode(setup.encode_to_vec());
    let mut meta = Map::new();
    meta.insert(META_KEY.into(), json!({ SESSION_SETUP_KEY: encoded }));
    Ok(meta)
}

fn session_setup(
    model: &ModelSelection,
    constraints: &SessionConstraints,
    tool_set: &SessionToolSet,
    disabled_tools: &[ToolRef],
    plugin_dirs: &[PathBuf],
) -> Result<pb::SessionSetup, AgentError> {
    Ok(pb::SessionSetup {
        model: Some(model_to_proto(model)),
        constraints: Some(constraints_to_proto(constraints)?),
        tool_set: Some(tool_set_to_proto(tool_set)),
        disabled_tools: disabled_tools.iter().map(|t| t.0.clone()).collect(),
        plugin_dirs: paths(plugin_dirs)?,
    })
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

fn paths(list: &[PathBuf]) -> Result<Vec<String>, AgentError> {
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
///
/// `_meta.iwork` 不是对象、`cwd` 不是绝对路径字符串时返回错误（点名键）：判定方会拿 `cwd` 解析
/// 相对路径，读错了比没有更糟。调用方按拒绝答复运行时，不交给处理方。
pub fn permission_request(
    req: &RequestPermissionRequest,
    updates_before: u64,
) -> Result<PermissionRequest, String> {
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
    // 没给是「运行时没报」，判定方用会话目录；给了就必须是绝对路径
    let cwd = match iwork_value(req.meta.as_ref(), CWD_KEY)? {
        None => None,
        Some(v) => match v.as_str().map(PathBuf::from) {
            Some(p) if p.is_absolute() => Some(p),
            _ => return Err(format!("_meta.{META_KEY}.{CWD_KEY} 须是绝对路径字符串")),
        },
    };
    Ok(PermissionRequest {
        tool: ToolRef(tool),
        raw_input: raw_input(fields.raw_input.as_ref()),
        bypass_sandbox,
        updates_before,
        cwd,
    })
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

// ---------- `_meta["iwork"]` 的约定项（T06 方案 2.5） ----------

/// `initialize` 响应里 iWork 约定的几项。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InitMeta {
    /// `None` = 运行时能一直等
    pub permission_wait_limit: Option<Duration>,
    pub sandbox_platform: SandboxPlatform,
    pub effort_levels: Vec<EffortLevel>,
    pub model_options: Vec<ModelOption>,
}

/// 解析 `initialize` 响应的 `_meta["iwork"]`。键缺失按「没报」处理（平台沙箱按不支持）；
/// 键在而值不对就报错：等待上限读错了，编排可能等过头，让运行时自己按超时处理。
/// 报错只点名键，不回显收到的值。
pub fn init_meta(meta: Option<&Meta>) -> Result<InitMeta, String> {
    let bad = |key: &str, want: &str| format!("_meta.{META_KEY}.{key} 须是{want}");
    let permission_wait_limit = match iwork_value(meta, PERMISSION_WAIT_LIMIT_KEY)? {
        None => None,
        Some(v) => match v.as_u64() {
            Some(secs) if secs > 0 => Some(Duration::from_secs(secs)),
            _ => return Err(bad(PERMISSION_WAIT_LIMIT_KEY, "正整数")),
        },
    };
    let sandbox_platform = match iwork_value(meta, SANDBOX_PLATFORM_KEY)? {
        None => SandboxPlatform::Unsupported,
        Some(v) => match v.as_str() {
            Some("supported") => SandboxPlatform::Supported,
            Some("unsupported") => SandboxPlatform::Unsupported,
            _ => {
                return Err(bad(
                    SANDBOX_PLATFORM_KEY,
                    " \"supported\" 或 \"unsupported\"",
                ));
            }
        },
    };
    let effort_levels = match iwork_value(meta, EFFORT_LEVELS_KEY)? {
        None => Vec::new(),
        Some(v) => non_empty_strings(v)
            .ok_or_else(|| bad(EFFORT_LEVELS_KEY, "非空字符串数组"))?
            .into_iter()
            .map(EffortLevel)
            .collect(),
    };
    let model_options = match iwork_value(meta, MODELS_KEY)? {
        None => Vec::new(),
        Some(v) => model_options(v).ok_or_else(|| bad(MODELS_KEY, " [{id, label}] 数组"))?,
    };
    Ok(InitMeta {
        permission_wait_limit,
        sandbox_platform,
        effort_levels,
        model_options,
    })
}

fn non_empty_strings(v: &Value) -> Option<Vec<String>> {
    v.as_array()?
        .iter()
        .map(|x| x.as_str().filter(|s| !s.is_empty()).map(str::to_owned))
        .collect()
}

fn model_options(v: &Value) -> Option<Vec<ModelOption>> {
    v.as_array()?
        .iter()
        .map(|m| {
            let id = m.get("id")?.as_str().filter(|s| !s.is_empty())?;
            let label = m.get("label")?.as_str()?;
            Some(ModelOption {
                id: ModelId(id.into()),
                label: label.into(),
            })
        })
        .collect()
}

/// 开会话（`session/new`、`session/load`）响应里 iWork 约定的几项。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionMeta {
    pub sandbox: SandboxStatus,
    /// 续接后运行时换了会话 id 时才有
    pub cli_session_id: Option<String>,
}

/// 解析开会话响应的 `_meta["iwork"]`。没报沙箱按「没起来」：静态的平台能力不冒充会话的实际状态。
pub fn session_meta(meta: Option<&Meta>) -> Result<SessionMeta, String> {
    let sandbox = match iwork_value(meta, SANDBOX_KEY)? {
        None => SandboxStatus::Unavailable,
        Some(v) => match v.as_str() {
            Some("enforced") => SandboxStatus::Enforced,
            Some("unavailable") => SandboxStatus::Unavailable,
            _ => {
                return Err(format!(
                    "_meta.{META_KEY}.{SANDBOX_KEY} 须是 \"enforced\" 或 \"unavailable\""
                ));
            }
        },
    };
    let cli_session_id = match iwork_value(meta, CLI_SESSION_ID_KEY)? {
        None => None,
        Some(v) => match v.as_str() {
            Some(id) if !id.is_empty() => Some(id.to_owned()),
            _ => {
                return Err(format!(
                    "_meta.{META_KEY}.{CLI_SESSION_ID_KEY} 须是非空字符串"
                ));
            }
        },
    };
    Ok(SessionMeta {
        sandbox,
        cli_session_id,
    })
}

/// 一条 `session/update` 通知带的上报。没有上报返回 `None`；有而解不出来返回错误（调用方记诊断、丢弃）。
pub fn report_of(meta: Option<&Meta>) -> Option<Result<AgentUpdate, String>> {
    match iwork_value(meta, REPORT_KEY) {
        Err(e) => Some(Err(e)),
        Ok(None) => None,
        Ok(Some(encoded)) => Some(decode_report(encoded)),
    }
}

fn decode_report(encoded: &Value) -> Result<AgentUpdate, String> {
    let bytes = encoded
        .as_str()
        .and_then(|s| BASE64.decode(s).ok())
        .ok_or_else(|| format!("_meta.{META_KEY}.{REPORT_KEY} 不是 base64 字符串"))?;
    let report = pb::RuntimeReport::decode(bytes.as_slice())
        .map_err(|e| format!("_meta.{META_KEY}.{REPORT_KEY} 解码失败：{e}"))?;
    match report.kind {
        Some(pb::runtime_report::Kind::Model(m)) => {
            if m.model_id.is_empty() {
                return Err("上报的模型 id 为空".into());
            }
            Ok(AgentUpdate::ModelReported(ModelSelection {
                model_id: ModelId(m.model_id),
                effort: m.effort.map(EffortLevel),
            }))
        }
        Some(pb::runtime_report::Kind::Usage(u)) => Ok(AgentUpdate::Usage(Usage {
            input_tokens: u.input_tokens,
            output_tokens: u.output_tokens,
            cache_read_tokens: u.cache_read_tokens,
            cache_write_tokens: u.cache_write_tokens,
            cost_usd: u.cost_usd,
        })),
        Some(pb::runtime_report::Kind::Quota(q)) => q
            .windows
            .into_iter()
            .map(quota_reading)
            .collect::<Result<_, _>>()
            .map(AgentUpdate::Quota),
        None => Err("上报没有内容".into()),
    }
}

fn quota_reading(w: pb::QuotaWindowReport) -> Result<QuotaReading, String> {
    let used_pct = u8::try_from(w.used_pct)
        .ok()
        .filter(|p| *p <= 100)
        .ok_or("额度使用率不在 0–100 之间")?;
    let resets_at = w.resets_at.map(timestamp).transpose()?;
    Ok(QuotaReading {
        window: w.window,
        used_pct,
        resets_at,
    })
}

/// Protobuf 时间 → 领域时间（Unix 纪元起的毫秒）。
fn timestamp(t: prost_types::Timestamp) -> Result<Timestamp, String> {
    let nanos = u32::try_from(t.nanos)
        .ok()
        .filter(|n| *n < 1_000_000_000)
        .ok_or("时间的纳秒部分越界")?;
    t.seconds
        .checked_mul(1000)
        .and_then(|ms| ms.checked_add(i64::from(nanos / 1_000_000)))
        .map(Timestamp)
        .ok_or_else(|| "时间越界".to_owned())
}

/// 把一条上报编码成通知的 `_meta`。运行时那边照同一份约定写；这里给测试与假运行时对拍用。
pub fn report_meta(report: &pb::RuntimeReport) -> Meta {
    let mut meta = Map::new();
    meta.insert(
        META_KEY.into(),
        json!({ REPORT_KEY: BASE64.encode(report.encode_to_vec()) }),
    );
    meta
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
        let spec = SessionSpec {
            cwd: PathBuf::from("/work"),
            model,
            system_prompt: String::new(),
            mcp: Vec::new(),
            constraints,
            tool_set,
            disabled_tools: vec![
                ToolRef("CronCreate".into()),
                ToolRef("RemoteTrigger".into()),
            ],
            plugin_dirs: vec![PathBuf::from("/data/imports/plugins/p1")],
            resume: None,
            kind: iwork_domain::turn::TurnKind::Assistant,
        };
        let meta = session_setup_meta(&spec).unwrap();
        let setup = decode_session_setup(Some(&meta)).unwrap();
        assert_eq!(setup.disabled_tools, vec!["CronCreate", "RemoteTrigger"]);
        assert_eq!(setup.plugin_dirs, vec!["/data/imports/plugins/p1"]);
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

    fn iwork_meta(inner: Value) -> Meta {
        let mut meta = Map::new();
        meta.insert(META_KEY.into(), inner);
        meta
    }

    #[test]
    fn init_meta_reads_every_key_and_defaults_when_absent() {
        let m = iwork_meta(json!({
            PERMISSION_WAIT_LIMIT_KEY: 7080,
            SANDBOX_PLATFORM_KEY: "supported",
            EFFORT_LEVELS_KEY: ["low", "high"],
            MODELS_KEY: [{"id": "opus", "label": "Opus"}],
        }));
        assert_eq!(
            init_meta(Some(&m)).unwrap(),
            InitMeta {
                permission_wait_limit: Some(Duration::from_secs(7080)),
                sandbox_platform: SandboxPlatform::Supported,
                effort_levels: vec![EffortLevel("low".into()), EffortLevel("high".into())],
                model_options: vec![ModelOption {
                    id: ModelId("opus".into()),
                    label: "Opus".into()
                }],
            }
        );
        // 没报：不限时、平台按不支持（不替运行时声称有沙箱）、列表为空
        assert_eq!(
            init_meta(None).unwrap(),
            InitMeta {
                permission_wait_limit: None,
                sandbox_platform: SandboxPlatform::Unsupported,
                effort_levels: Vec::new(),
                model_options: Vec::new(),
            }
        );
    }

    /// 键在而值不对就报错、点名键，不回显值，不退回「没报」：等待上限读丢了编排会等过头。
    #[test]
    fn init_meta_rejects_malformed_values_by_key() {
        for (key, value) in [
            (PERMISSION_WAIT_LIMIT_KEY, json!(0)),
            (PERMISSION_WAIT_LIMIT_KEY, json!(-5)),
            (PERMISSION_WAIT_LIMIT_KEY, json!("7080")),
            (SANDBOX_PLATFORM_KEY, json!("maybe-secret")),
            (EFFORT_LEVELS_KEY, json!(["low", 3])),
            (EFFORT_LEVELS_KEY, json!([""])),
            (MODELS_KEY, json!([{"id": "opus"}])),
            (MODELS_KEY, json!("opus")),
        ] {
            let m = iwork_meta(json!({ key: value }));
            let err = init_meta(Some(&m)).unwrap_err();
            assert!(err.contains(key), "{key}: {err}");
            assert!(!err.contains("maybe-secret"), "{err}");
        }
    }

    #[test]
    fn session_meta_reads_sandbox_and_new_session_id() {
        let m = iwork_meta(json!({ SANDBOX_KEY: "enforced", CLI_SESSION_ID_KEY: "new-id" }));
        assert_eq!(
            session_meta(Some(&m)).unwrap(),
            SessionMeta {
                sandbox: SandboxStatus::Enforced,
                cli_session_id: Some("new-id".into()),
            }
        );
        assert_eq!(
            session_meta(None).unwrap(),
            SessionMeta {
                sandbox: SandboxStatus::Unavailable,
                cli_session_id: None,
            },
            "没报沙箱 = 没起来"
        );
        for bad in [
            json!({ SANDBOX_KEY: "on" }),
            json!({ CLI_SESSION_ID_KEY: "" }),
            json!({ CLI_SESSION_ID_KEY: 1 }),
        ] {
            assert!(
                session_meta(Some(&iwork_meta(bad.clone()))).is_err(),
                "{bad}"
            );
        }
    }

    fn report(kind: pb::runtime_report::Kind) -> Meta {
        report_meta(&pb::RuntimeReport { kind: Some(kind) })
    }

    #[test]
    fn reports_decode_to_updates() {
        let model = report(pb::runtime_report::Kind::Model(pb::ModelSelection {
            model_id: "claude-opus".into(),
            effort: Some("max".into()),
        }));
        assert_eq!(
            report_of(Some(&model)).unwrap().unwrap(),
            AgentUpdate::ModelReported(ModelSelection {
                model_id: ModelId("claude-opus".into()),
                effort: Some(EffortLevel("max".into())),
            })
        );
        let usage = report(pb::runtime_report::Kind::Usage(pb::Usage {
            input_tokens: 10,
            output_tokens: 20,
            cache_read_tokens: 30,
            cache_write_tokens: 40,
            cost_usd: None,
        }));
        assert_eq!(
            report_of(Some(&usage)).unwrap().unwrap(),
            AgentUpdate::Usage(Usage {
                input_tokens: 10,
                output_tokens: 20,
                cache_read_tokens: 30,
                cache_write_tokens: 40,
                cost_usd: None,
            })
        );
        let quota = report(pb::runtime_report::Kind::Quota(pb::QuotaReport {
            windows: vec![
                pb::QuotaWindowReport {
                    window: "five_hour".into(),
                    used_pct: 42,
                    resets_at: Some(prost_types::Timestamp {
                        seconds: 1_731_300_000,
                        nanos: 250_000_000,
                    }),
                },
                pb::QuotaWindowReport {
                    window: "seven_day".into(),
                    used_pct: 100,
                    resets_at: None,
                },
            ],
        }));
        assert_eq!(
            report_of(Some(&quota)).unwrap().unwrap(),
            AgentUpdate::Quota(vec![
                QuotaReading {
                    window: "five_hour".into(),
                    used_pct: 42,
                    resets_at: Some(Timestamp(1_731_300_000_250)),
                },
                QuotaReading {
                    window: "seven_day".into(),
                    used_pct: 100,
                    resets_at: None,
                },
            ])
        );
        assert!(report_of(None).is_none());
        assert!(report_of(Some(&iwork_meta(json!({ "other": 1 })))).is_none());
    }

    #[test]
    fn malformed_reports_are_errors_not_guesses() {
        let bad = [
            iwork_meta(json!({ REPORT_KEY: "!!not base64" })),
            iwork_meta(json!({ REPORT_KEY: 12 })),
            report_meta(&pb::RuntimeReport { kind: None }),
            report(pb::runtime_report::Kind::Model(pb::ModelSelection {
                model_id: String::new(),
                effort: None,
            })),
            report(pb::runtime_report::Kind::Quota(pb::QuotaReport {
                windows: vec![pb::QuotaWindowReport {
                    window: "w".into(),
                    used_pct: 101,
                    resets_at: None,
                }],
            })),
            report(pb::runtime_report::Kind::Quota(pb::QuotaReport {
                windows: vec![pb::QuotaWindowReport {
                    window: "w".into(),
                    used_pct: 1,
                    resets_at: Some(prost_types::Timestamp {
                        seconds: i64::MAX,
                        nanos: 0,
                    }),
                }],
            })),
        ];
        for m in &bad {
            assert!(report_of(Some(m)).unwrap().is_err(), "{m:?}");
        }
    }

    #[test]
    fn permission_request_carries_runtime_cwd() {
        use agent_client_protocol::v1::schema::ToolCallUpdate as Upd;
        let req = |meta: Option<Meta>| {
            let mut r = RequestPermissionRequest::new(
                "s",
                Upd::new("t1", ToolCallUpdateFields::new().name("Bash".to_string())),
                options(),
            );
            r.meta = meta;
            r
        };
        let abs = std::env::temp_dir().join("sub");
        let with_cwd = req(Some(iwork_meta(json!({ CWD_KEY: abs.to_str().unwrap() }))));
        assert_eq!(permission_request(&with_cwd, 3).unwrap().cwd, Some(abs));
        assert_eq!(permission_request(&req(None), 3).unwrap().cwd, None);
        assert_eq!(
            permission_request(&req(Some(iwork_meta(json!({})))), 3)
                .unwrap()
                .cwd,
            None
        );
    }

    /// 1/F-5：`cwd` 不是字符串、是相对路径，`_meta.iwork` 不是对象：拒绝并点名键，不按「没报」放过。
    #[test]
    fn malformed_permission_meta_is_rejected_by_key() {
        use agent_client_protocol::v1::schema::ToolCallUpdate as Upd;
        let req = |meta: Meta| {
            let mut r = RequestPermissionRequest::new(
                "s",
                Upd::new("t1", ToolCallUpdateFields::new().name("Bash".to_string())),
                options(),
            );
            r.meta = Some(meta);
            r
        };
        for bad in [json!({ CWD_KEY: 5 }), json!({ CWD_KEY: "sub/dir" })] {
            let err = permission_request(&req(iwork_meta(bad.clone())), 0).unwrap_err();
            assert!(err.contains("_meta.iwork.cwd"), "{bad}: {err}");
        }
        let err = permission_request(&req(iwork_meta(json!("cwd=/x"))), 0).unwrap_err();
        assert!(err.contains("_meta.iwork 须是对象"), "{err}");
    }

    /// 1/F-5：`_meta.iwork` 不是对象时，initialize、开会话、上报都报错，不当成「没报」。
    #[test]
    fn non_object_iwork_meta_is_an_error_everywhere() {
        let m = iwork_meta(json!(["sandbox", "enforced"]));
        assert!(
            init_meta(Some(&m))
                .unwrap_err()
                .contains("_meta.iwork 须是对象")
        );
        assert!(
            session_meta(Some(&m))
                .unwrap_err()
                .contains("_meta.iwork 须是对象")
        );
        assert!(report_of(Some(&m)).unwrap().is_err());
    }
}
