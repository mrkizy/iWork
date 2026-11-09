//! 动作：工具调用归一之后的形状（技术设计 7.1）。审批按动作求值，不按工具名。

use std::path::PathBuf;

/// 已解析符号链接、规范化之后的绝对路径。规范化由调用方（适配层、host）做，domain 只承载结果。
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct CanonicalPath(pub PathBuf);

/// 动作类型（技术设计 7.1 表）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum ActionKind {
    Read,
    Write,
    Execute,
    OutsideSandbox,
    /// 写会被自动执行的配置（如钩子、启动脚本）
    WriteAutorunConfig,
    Delete,
    Push,
    Publish,
    Network,
    McpCall,
    Unclassified,
}

/// 动作的目标。
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum ActionTarget {
    Path(CanonicalPath),
    Paths(Vec<CanonicalPath>),
    Command {
        program: String,
        subcommand: Option<String>,
        argv: Vec<String>,
    },
    ToolName(String),
    Remote(String),
    None,
}

/// 原始工具名，只进审计，不参与判定（判定与工具无关，需求 AC05）。
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct ToolRef(pub String);

/// 归一时顺带算出的标记。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub struct ActionFlags {
    /// 目标在能力边界之外
    pub out_of_bounds: bool,
    /// 运行时请求绕过沙箱
    pub bypass_sandbox_requested: bool,
    /// 「执行脚本」类：看不出脚本会做什么
    pub opaque_script: bool,
}

/// 一次工具调用归一后的动作。
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct Action {
    pub kind: ActionKind,
    pub target: ActionTarget,
    pub tool: ToolRef,
    /// 给人看的一行摘要（命令文字或补丁摘要）
    pub raw_summary: String,
    pub flags: ActionFlags,
}

/// 「动作类型 + 目标」：识别「同一个动作」的键（需求 AC05）。
///
/// 用户拒绝过的动作在本对话内、下一条用户消息之前不得以任何方式绕过；换工具、换成员、
/// 换运行时都算同一个动作，所以指纹里不含工具名、摘要和标记。
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct ActionFingerprint {
    pub kind: ActionKind,
    pub target: ActionTarget,
}

impl Action {
    /// 算动作指纹。多路径目标按路径排序去重，路径列出的顺序不同也是同一个动作。
    pub fn fingerprint(&self) -> ActionFingerprint {
        let target = match &self.target {
            ActionTarget::Paths(paths) => {
                let mut paths = paths.clone();
                paths.sort();
                paths.dedup();
                ActionTarget::Paths(paths)
            }
            other => other.clone(),
        };
        ActionFingerprint {
            kind: self.kind,
            target,
        }
    }
}
