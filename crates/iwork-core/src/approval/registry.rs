//! 工具归类登记表（T07 方案 3.2，Claude Code 2.1.319；技术设计 7.1「内置工具归类」、AC39）。
//!
//! 「再加一种工具」= 表里加一行；主流程没有按工具名的分支。工具名与 hook 输入字段名以 T06 从
//! 固定版本导出的清单为准（方案第 12 节需实测 ①），这里先按已知名字登记；表外名字一律按未归类求值
//! （手动、自动询问，安全侧）。

use iwork_domain::common::ToolGroup;

/// 工具怎么归一成动作。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ToolClass {
    /// 读：目标路径在 `field`；`field` 缺省时取 cwd（Grep、Glob 不给 path 时搜当前目录）
    Read {
        field: &'static str,
        default_cwd: bool,
    },
    /// 写：目标路径在 `field`
    Write { field: &'static str },
    /// 逐子命令归类（唯一会产生多个动作的工具）
    Bash,
    /// 联网：恒放行，只审计。`url_field` 为 `None` 时目标记为 `search`
    Network { url_field: Option<&'static str> },
    /// 自身无副作用（子代理、技能、待办、计划模式、后台命令的读取与停止）：默认层内置放行
    Harmless,
    /// 能把工作带出 iWork 管控的工具（云端 / 定时）：启动时不提供，三档拒绝
    Disabled,
}

/// 一个登记过的工具。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ToolSpec {
    pub name: &'static str,
    pub class: ToolClass,
    /// 属于助手工具配置的哪个分组；`None` 是不属于四组的「始终开启」工具
    pub group: Option<ToolGroup>,
}

/// Bash 工具输入的字段名。
pub const BASH_COMMAND_FIELD: &str = "command";
pub const BASH_DESCRIPTION_FIELD: &str = "description";
pub const BASH_DISABLE_SANDBOX_FIELD: &str = "dangerouslyDisableSandbox";
/// Edit 工具的新旧文字（亲手执行卡片用，方案 5.4）。
pub const EDIT_OLD_FIELD: &str = "old_string";
pub const EDIT_NEW_FIELD: &str = "new_string";
/// 生成亲手执行补丁卡片的工具（只有它带旧内容，Q6）。
pub const PATCH_TOOL: &str = "Edit";
/// MCP 工具名前缀。
pub const MCP_PREFIX: &str = "mcp__";

use ToolClass as C;
use ToolGroup as G;

/// Claude Code 2.1.319 的内置工具。
pub const TOOLS: &[ToolSpec] = &[
    ToolSpec {
        name: "Read",
        class: C::Read {
            field: "file_path",
            default_cwd: false,
        },
        group: Some(G::ReadAndSearch),
    },
    ToolSpec {
        name: "Grep",
        class: C::Read {
            field: "path",
            default_cwd: true,
        },
        group: Some(G::ReadAndSearch),
    },
    ToolSpec {
        name: "Glob",
        class: C::Read {
            field: "path",
            default_cwd: true,
        },
        group: Some(G::ReadAndSearch),
    },
    ToolSpec {
        name: "Edit",
        class: C::Write { field: "file_path" },
        group: Some(G::EditFiles),
    },
    ToolSpec {
        name: "Write",
        class: C::Write { field: "file_path" },
        group: Some(G::EditFiles),
    },
    ToolSpec {
        name: "NotebookEdit",
        class: C::Write {
            field: "notebook_path",
        },
        group: Some(G::EditFiles),
    },
    ToolSpec {
        name: "Bash",
        class: C::Bash,
        group: Some(G::RunCommands),
    },
    // 后台命令的读取与停止：名字待 T06 核对（方案第 12 节 ①）
    ToolSpec {
        name: "BashOutput",
        class: C::Harmless,
        group: Some(G::RunCommands),
    },
    ToolSpec {
        name: "KillShell",
        class: C::Harmless,
        group: Some(G::RunCommands),
    },
    ToolSpec {
        name: "WebFetch",
        class: C::Network {
            url_field: Some("url"),
        },
        group: Some(G::Web),
    },
    ToolSpec {
        name: "WebSearch",
        class: C::Network { url_field: None },
        group: Some(G::Web),
    },
    ToolSpec {
        name: "Agent",
        class: C::Harmless,
        group: None,
    },
    ToolSpec {
        name: "Skill",
        class: C::Harmless,
        group: None,
    },
    ToolSpec {
        name: "TodoWrite",
        class: C::Harmless,
        group: None,
    },
    ToolSpec {
        name: "EnterPlanMode",
        class: C::Harmless,
        group: None,
    },
    ToolSpec {
        name: "ExitPlanMode",
        class: C::Harmless,
        group: None,
    },
    ToolSpec {
        name: "RemoteTrigger",
        class: C::Disabled,
        group: None,
    },
    ToolSpec {
        name: "CronCreate",
        class: C::Disabled,
        group: None,
    },
    ToolSpec {
        name: "CronDelete",
        class: C::Disabled,
        group: None,
    },
    ToolSpec {
        name: "CronList",
        class: C::Disabled,
        group: None,
    },
    ToolSpec {
        name: "ScheduleWakeup",
        class: C::Disabled,
        group: None,
    },
    // 产品负责人 2026-11-12 定（T06 方案 P13，技术设计 7.1）：前三个把东西送出 iWork，Workflow 起的子代理不受
    // iWork 回合控制，两个 worktree 工具会在会话中途换工作目录
    ToolSpec {
        name: "DesignSync",
        class: C::Disabled,
        group: None,
    },
    ToolSpec {
        name: "PushNotification",
        class: C::Disabled,
        group: None,
    },
    ToolSpec {
        name: "SendMessage",
        class: C::Disabled,
        group: None,
    },
    ToolSpec {
        name: "Workflow",
        class: C::Disabled,
        group: None,
    },
    ToolSpec {
        name: "EnterWorktree",
        class: C::Disabled,
        group: None,
    },
    ToolSpec {
        name: "ExitWorktree",
        class: C::Disabled,
        group: None,
    },
];

/// 查一个工具的登记。表外（含 MCP）返回 `None`。
pub fn tool(name: &str) -> Option<&'static ToolSpec> {
    TOOLS.iter().find(|t| t.name == name)
}

/// 工具属于哪个分组；不属于四组（始终开启、MCP、未归类）返回 `None`。
pub fn group_of(name: &str) -> Option<ToolGroup> {
    tool(name).and_then(|t| t.group)
}

/// 某一类的全部工具名，按登记顺序。
pub fn tools_of(pred: impl Fn(&ToolSpec) -> bool) -> impl Iterator<Item = &'static str> {
    TOOLS.iter().filter(move |t| pred(t)).map(|t| t.name)
}
