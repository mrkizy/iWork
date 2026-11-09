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

/// 规则按标记匹配时用的标记名（[`crate::rule::RuleMatcher::Flag`]），与 [`ActionFlags`] 的字段一一对应。
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum ActionFlag {
    OutOfBounds,
    BypassSandboxRequested,
    OpaqueScript,
}

impl ActionFlags {
    /// 这个标记是否为真。
    pub fn has(&self, flag: ActionFlag) -> bool {
        match flag {
            ActionFlag::OutOfBounds => self.out_of_bounds,
            ActionFlag::BypassSandboxRequested => self.bypass_sandbox_requested,
            ActionFlag::OpaqueScript => self.opaque_script,
        }
    }
}

/// 一次调用的形状（T07 方案 3.1）。决定能否生成亲手执行卡片、能否放行规则与记住。
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum CallShapeKind {
    /// 非 Bash 工具：恰好一个动作
    Tool,
    /// 恰好一个简单命令，全部词字面
    SingleSimpleCommand,
    /// 多个简单命令，且每个都归类成功
    Compound,
    /// 解析不了：三档询问、不记住、不审查
    Unparsable,
}

/// 命令会在沙箱里还是沙箱外运行（T07 方案 3.7）。`Ambiguous` 按沙箱外求值。
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum SandboxPlacement {
    Inside,
    Outside,
    Ambiguous,
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
/// 换运行时、换一种写法表达同一个目标都算同一个动作，所以指纹里不含工具名、摘要和标记，
/// 目标先归一再比较。字段私有：只能经 [`ActionFingerprint::new`] 构造，保证同一个动作只有一种表示。
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct ActionFingerprint {
    kind: ActionKind,
    target: ActionTarget,
}

impl ActionFingerprint {
    /// 归一目标后构造指纹。归一规则（只对路径集合）：
    /// - 排序、去重：列出的顺序与重复不影响；
    /// - 去重后只剩一条 → 当单路径 `Path`：一个文件不管用单路径还是单元素集合表达都是同一个目标；
    /// - 空集合 → `None`：没有目标。
    ///
    /// 不另造一个 `NormalizedTarget` 类型：归一后的值仍是 `ActionTarget`，proto 的
    /// `ActionFingerprint.target` 原样能装，不用改协议。
    pub fn new(kind: ActionKind, target: ActionTarget) -> Self {
        let target = match target {
            ActionTarget::Paths(mut paths) => {
                paths.sort();
                paths.dedup();
                match paths.len() {
                    0 => ActionTarget::None,
                    1 => ActionTarget::Path(paths.remove(0)),
                    _ => ActionTarget::Paths(paths),
                }
            }
            other => other,
        };
        Self { kind, target }
    }

    pub fn kind(&self) -> ActionKind {
        self.kind
    }

    /// 归一后的目标。
    pub fn target(&self) -> &ActionTarget {
        &self.target
    }
}

impl Action {
    /// 算动作指纹，见 [`ActionFingerprint::new`]。
    pub fn fingerprint(&self) -> ActionFingerprint {
        ActionFingerprint::new(self.kind, self.target.clone())
    }
}
