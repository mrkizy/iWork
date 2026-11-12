//! 启动参数表（T06 方案 3.1）：一行一个参数，依据写在方案那张表里。
//!
//! **不传的参数**（`tests/launch_snapshot.rs` 反向断言）：
//! - `--dangerously-skip-permissions`；
//! - `--permission-mode`：权限模式只经设置文件给，一处真值；
//! - `--bare`、`--safe-mode`：两者都会关掉 hook；
//! - `--fallback-model`：不自动换模型（技术设计 9.6 第 7 条）；
//! - `--add-dir`：可写目录只经沙箱设置给；
//! - `--allowedTools`：CLI 的放行规则不参与判定；
//! - `--continue`、`--fork-session`。

use super::session_dir::Files;
use super::{Opening, Start};
use crate::setup::{Setup, Tools};

/// 反向断言用：这些参数永远不出现。
pub const NEVER_PASSED: [&str; 9] = [
    "--dangerously-skip-permissions",
    "--permission-mode",
    "--bare",
    "--safe-mode",
    "--fallback-model",
    "--add-dir",
    "--allowedTools",
    "--continue",
    "--fork-session",
];

/// 起 CLI 的参数，次序就是方案 3.1 那张表的次序。
pub fn cli_args(setup: &Setup, opening: &Opening, files: &Files) -> Vec<String> {
    let mut a: Vec<String> = Vec::new();
    let mut push = |items: &[&str]| a.extend(items.iter().map(|s| (*s).to_owned()));
    // 非交互、流式输入多轮、流式输出
    push(&[
        "-p",
        "--input-format",
        "stream-json",
        "--output-format",
        "stream-json",
        "--verbose",
    ]);
    // 正文的增量片段（G7a′）
    push(&["--include-partial-messages"]);
    // 用户、项目、本地设置三类都不加载（需求 8.4 v1.63；空值写法待 G5）
    push(&["--setting-sources", ""]);
    // 只加载 iWork 给的 MCP（M1 恒为空）
    push(&[
        "--strict-mcp-config",
        "--mcp-config",
        &files.mcp.display().to_string(),
    ]);
    // hook、权限模式、沙箱
    push(&["--settings", &files.settings.display().to_string()]);
    for dir in &setup.plugin_dirs {
        push(&["--plugin-dir", &dir.display().to_string()]);
    }
    // 显式传模型与思考强度（技术设计 9.6 第 6 条）
    push(&["--model", &setup.model.id]);
    push(&["--effort", setup.model.effort]);
    if !opening.system_prompt.is_empty() {
        push(&["--append-system-prompt", &opening.system_prompt]);
    }
    if !setup.disabled_tools.is_empty() {
        push(&["--disallowedTools", &setup.disabled_tools.join(",")]);
    }
    // AI 审查回合：一个工具都不给，也就不触发 hook（技术设计 7.2b；G8a）。白名单管不到它，两者都有时以它为准。
    // 否则表外的 CLI 版本带内置工具白名单（第 9 节，P11 的 d）；和 --disallowedTools 同用时禁用优先（G8d）。
    // 这条路依赖两个待 R2 核实的前提：白名单不影响 MCP 工具（G8e）、写了这个版本没有的名字不报错（G8g）。
    // 任何一个不成立就退到第 9 节的 d1（不用 --tools，把新工具追加进 disabled_tools），由 architect 改方案后再改这里
    match (setup.tools, &setup.builtin_tool_allowlist) {
        (Tools::None, _) => push(&["--tools", ""]),
        (Tools::Default, Some(allow)) => push(&["--tools", &allow.join(",")]),
        (Tools::Default, None) => {}
    }
    match &opening.start {
        Start::New { session_id } => push(&["--session-id", session_id]),
        Start::Resume { session_id } => push(&["--resume", session_id]),
    }
    a
}
