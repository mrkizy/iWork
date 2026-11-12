//! 从 ACP `_meta` 解出会话配置并校验（T06 方案 2.2、2.5）。
//!
//! 外部输入的边界：缺字段、值不认识、路径不是绝对路径，一律拒绝开会话（`setup_invalid`），不退回 CLI 的
//! 默认值。校验过的 [`Setup`] 之后就被信任：`launch` 只翻译，不再查。

use std::path::PathBuf;

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD as BASE64;
use iwork_proto::pb;
use prost::Message as _;
use serde_json::Value;

/// `_meta` 下 iWork 的键（与 `iwork-agent-acp::convert` 同一份约定，两边各写常量，端到端测试对拍）。
pub const META_KEY: &str = "iwork";
/// `_meta.iwork` 下会话配置的键：值是 `SessionSetup` 的 Protobuf 二进制再做标准 base64。
pub const SESSION_SETUP_KEY: &str = "sessionSetup";

/// 思考强度的可选值（Claude Code 2.1.322 `--help`；G6 逐个读回过）。`initialize` 报的 `effortLevels` 也读它。
pub const EFFORT_LEVELS: [&str; 5] = ["low", "medium", "high", "xhigh", "max"];

/// 校验过的会话配置。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Setup {
    pub model: Model,
    pub constraints: Constraints,
    pub tools: Tools,
    /// 启动时就不提供的工具名（核心算好，桥接原样转交，不认识任何工具名）
    pub disabled_tools: Vec<String>,
    pub plugin_dirs: Vec<PathBuf>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Model {
    pub id: String,
    /// [`EFFORT_LEVELS`] 之一
    pub effort: &'static str,
}

/// 沙箱的抽象约束（`agent.proto` 的 `SessionConstraints`），路径都是绝对路径。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Constraints {
    pub deny_read_write: Vec<PathBuf>,
    pub read_only: Vec<PathBuf>,
    pub allow_write: AllowWrite,
    pub excluded_commands: Vec<CommandPattern>,
    pub allow_unsandboxed_retry: bool,
    pub network: Network,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AllowWrite {
    Whitelist(Vec<PathBuf>),
    /// 全开档
    Anywhere,
}

/// 程序名 + 可选子命令，都不含空白（要拼进 `excludedCommands` 的模式文字）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CommandPattern {
    pub program: String,
    pub subcommand: Option<String>,
}

/// 联网不受控（需求 v1.38），目前只有一种。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Network {
    Open,
}

/// 会话给不给工具。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Tools {
    /// 全部内置工具减去 `disabled_tools`（走 hook 审批）
    Default,
    /// 一个工具都不给（AI 审查回合：没有分组、也不给自身无副作用的工具），也就不触发 hook（技术设计 7.2b）
    None,
}

/// 会话配置不完整或不认识。`field` 是点分的字段路径，原样进错误码 `setup_invalid` 的 `reason`
/// （只用小写字母、数字、下划线、点，`iwork-agent-acp::bridge_error` 只认这种形状）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SetupInvalid {
    pub field: &'static str,
}

impl std::fmt::Display for SetupInvalid {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "会话配置不完整或不认识：{}", self.field)
    }
}

impl std::error::Error for SetupInvalid {}

fn invalid(field: &'static str) -> SetupInvalid {
    SetupInvalid { field }
}

/// 从 `session/new`、`session/load` 请求的 `_meta` 解出会话配置。
pub fn from_meta(meta: Option<&Value>) -> Result<Setup, SetupInvalid> {
    let encoded = meta
        .and_then(|m| m.get(META_KEY))
        .and_then(|v| v.get(SESSION_SETUP_KEY))
        .and_then(Value::as_str)
        .ok_or(invalid("session_setup"))?;
    let bytes = BASE64
        .decode(encoded)
        .map_err(|_| invalid("session_setup"))?;
    let setup = pb::SessionSetup::decode(bytes.as_slice()).map_err(|_| invalid("session_setup"))?;
    validate(setup)
}

/// 校验并转成 [`Setup`]。
pub fn validate(setup: pb::SessionSetup) -> Result<Setup, SetupInvalid> {
    let pb::SessionSetup {
        model,
        constraints,
        tool_set,
        disabled_tools,
        plugin_dirs,
    } = setup;
    Ok(Setup {
        model: model_from(model.ok_or(invalid("model"))?)?,
        constraints: constraints_from(constraints.ok_or(invalid("constraints"))?)?,
        tools: tools_from(tool_set.ok_or(invalid("tool_set"))?)?,
        disabled_tools: disabled_tools
            .into_iter()
            .map(|t| {
                // 逗号分隔进 --disallowedTools：名字里带逗号或空白会拆错
                let ok = !t.is_empty() && !t.contains(',') && !t.chars().any(char::is_whitespace);
                ok.then_some(t).ok_or(invalid("disabled_tools"))
            })
            .collect::<Result<_, _>>()?,
        plugin_dirs: absolute_all(plugin_dirs, "plugin_dirs")?,
    })
}

fn model_from(m: pb::ModelSelection) -> Result<Model, SetupInvalid> {
    if m.model_id.trim().is_empty() {
        return Err(invalid("model.model_id"));
    }
    // 不传 --effort 就是 CLI 的默认值，配置隔离后不允许这种状态（技术设计 9.6 第 6 条）
    let effort = m
        .effort
        .as_deref()
        .and_then(|e| EFFORT_LEVELS.iter().find(|l| **l == e))
        .ok_or(invalid("model.effort"))?;
    Ok(Model {
        id: m.model_id,
        effort,
    })
}

fn absolute_all(paths: Vec<String>, field: &'static str) -> Result<Vec<PathBuf>, SetupInvalid> {
    paths
        .into_iter()
        .map(|p| {
            let path = PathBuf::from(p);
            path.is_absolute().then_some(path).ok_or(invalid(field))
        })
        .collect()
}

fn word(s: &str) -> bool {
    !s.is_empty() && !s.chars().any(char::is_whitespace)
}

fn constraints_from(c: pb::SessionConstraints) -> Result<Constraints, SetupInvalid> {
    let allow_write = match c.allow_write.and_then(|a| a.kind) {
        Some(pb::allow_write::Kind::Whitelist(w)) => {
            AllowWrite::Whitelist(absolute_all(w.paths, "constraints.allow_write")?)
        }
        Some(pb::allow_write::Kind::Anywhere(_)) => AllowWrite::Anywhere,
        None => return Err(invalid("constraints.allow_write")),
    };
    let network = match pb::NetworkPolicy::try_from(c.network) {
        Ok(pb::NetworkPolicy::Open) => Network::Open,
        _ => return Err(invalid("constraints.network")),
    };
    let excluded_commands = c
        .excluded_commands
        .into_iter()
        .map(|cmd| {
            let sub_ok = cmd.subcommand.as_deref().is_none_or(word);
            (word(&cmd.program) && sub_ok)
                .then_some(CommandPattern {
                    program: cmd.program,
                    subcommand: cmd.subcommand,
                })
                .ok_or(invalid("constraints.excluded_commands"))
        })
        .collect::<Result<_, _>>()?;
    Ok(Constraints {
        deny_read_write: absolute_all(c.deny_read_write, "constraints.deny_read_write")?,
        read_only: absolute_all(c.read_only, "constraints.read_only")?,
        allow_write,
        excluded_commands,
        allow_unsandboxed_retry: c.allow_unsandboxed_retry,
        network,
    })
}

fn tools_from(t: pb::SessionToolSet) -> Result<Tools, SetupInvalid> {
    for g in &t.enabled_groups {
        match pb::ToolGroup::try_from(*g) {
            Ok(pb::ToolGroup::Unspecified) | Err(_) => {
                return Err(invalid("tool_set.enabled_groups"));
            }
            Ok(_) => {}
        }
    }
    // 分组本身怎么映射成工具名是核心的事（已算进 disabled_tools）；桥接只认「一个都不给」这一种
    Ok(if t.enabled_groups.is_empty() && !t.include_always_on {
        Tools::None
    } else {
        Tools::Default
    })
}

#[cfg(test)]
#[path = "setup_tests.rs"]
mod tests;
