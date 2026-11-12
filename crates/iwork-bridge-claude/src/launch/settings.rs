//! `--settings` 的 JSON（T06 方案 3.2、3.4a、第 8 节）：hook、权限模式、沙箱翻译表。
//!
//! 文件写进会话目录后桥接不再改它（CLI 会热加载设置文件）。数据目录对 Agent 禁读禁写，Agent 改不了它。

use std::path::Path;

use iwork_claude_hook::client::ENV_TOKEN;
use serde_json::{Value, json};

use crate::setup::{AllowWrite, CommandPattern, Constraints, Network};

/// PreToolUse hook 的 timeout（秒）。T0 验证过；更大的值要不要用见方案 18 节 P5。三个期限都从它算（`deadlines`）。
pub const HOOK_TIMEOUT_SECS: u64 = 7200;

/// SessionStart 报到 hook 的 timeout（秒）。
pub const SESSION_START_TIMEOUT_SECS: u64 = 30;

/// 选定配置（方案 3.4a，产品负责人 2026-11-12 定）：写死，不按情况放宽；改任何一个都要先改 3.4a 与 AGENTS.md。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SelectedConfig {
    /// `permissions.defaultMode`：设置文件被整个忽略时 CLI 报 `default`，和它对不上，核对得出来（3.4a 第 2 条）
    pub default_mode: &'static str,
    /// 每个 PreToolUse hook 的 `onFailure`：hook 失败、超时、程序不在时拦下调用（3.4a 第 1 条）
    pub on_failure: &'static str,
}

pub const SELECTED: SelectedConfig = SelectedConfig {
    default_mode: "dontAsk",
    on_failure: "block",
};

/// 生成的设置文件。
/// `hook_timeout`：PreToolUse hook 的 timeout，和算期限用的是同一个值（产品里是 [`HOOK_TIMEOUT_SECS`]）。
pub fn settings_json(
    constraints: &Constraints,
    hook_program: &str,
    memory_dir: &Path,
    hook_timeout: std::time::Duration,
) -> Value {
    let hook = shell_quote(hook_program);
    json!({
        // 3.1a：自动记忆的目录改到会话目录里的空目录（开关另由环境变量 CLAUDE_CODE_DISABLE_AUTO_MEMORY 给）。
        // 经 --settings 生效待 R2 的 G5 记忆行核实；不成立时只剩开关（P15）
        "autoMemoryDirectory": memory_dir.display().to_string(),
        // 显式写出：命令行这一层压过项目层，仓库里写 true 也关不掉
        "disableAllHooks": false,
        "permissions": {
            "defaultMode": SELECTED.default_mode,
            "disableBypassPermissionsMode": "disable",
        },
        "hooks": {
            "PreToolUse": [{ "matcher": "*", "hooks": [{
                "type": "command",
                "command": format!("{hook} pre-tool-use"),
                "timeout": hook_timeout.as_secs(),
                "onFailure": SELECTED.on_failure,
            }]}],
            "SessionStart": [{ "hooks": [{
                "type": "command",
                "command": format!("{hook} session-start"),
                "timeout": SESSION_START_TIMEOUT_SECS,
            }]}],
        },
        "sandbox": sandbox_json(constraints),
    })
}

/// 第 8 节的翻译表，一行一条。路径原样用后台给的绝对路径，不展开、不解析链接。
fn sandbox_json(c: &Constraints) -> Value {
    let paths = |list: &[std::path::PathBuf]| -> Vec<String> {
        list.iter().map(|p| p.display().to_string()).collect()
    };
    let allow_write = match &c.allow_write {
        AllowWrite::Whitelist(list) => paths(list),
        // 全开档：底线目录的 denyRead / denyWrite 不变，仍然优先
        AllowWrite::Anywhere => vec!["/".to_owned()],
    };
    let allowed_domains = match c.network {
        Network::Open => vec!["*"],
    };
    json!({
        "enabled": true,
        // P4 选 a（2026-11-12）：本该有沙箱的平台上沙箱起不来，CLI 不启动，会话开不了
        "failIfUnavailable": true,
        // 沙箱内也不自动放行：问不问由 iWork 定
        "autoAllowBashIfSandboxed": false,
        "allowUnsandboxedCommands": c.allow_unsandboxed_retry,
        "excludedCommands": c.excluded_commands.iter().map(excluded_pattern).collect::<Vec<_>>(),
        "filesystem": {
            "denyRead": paths(&c.deny_read_write),
            // 只读区在数据目录下，已被 denyWrite 盖住
            "denyWrite": paths(&c.deny_read_write),
            "allowRead": paths(&c.read_only),
            "allowWrite": allow_write,
        },
        "credentials": {
            "files": c.deny_read_write.iter()
                .map(|p| json!({ "path": p.display().to_string(), "mode": "deny" }))
                .collect::<Vec<_>>(),
            // 令牌不进沙箱内命令的环境（G9b）
            "envVars": [{ "name": ENV_TOKEN, "mode": "deny" }],
        },
        "network": { "allowedDomains": allowed_domains },
        // 本机 socket 不放行：不设 allowUnixSockets
    })
}

/// 需凭据命令清单的一条 → `excludedCommands` 的模式文字（T07 3.7 第 1 条定的写法；全仓唯一生成它的地方）。
pub fn excluded_pattern(c: &CommandPattern) -> String {
    match &c.subcommand {
        Some(sub) => format!("{} {sub} *", c.program),
        None => format!("{} *", c.program),
    }
}

/// `--mcp-config` 的内容：M1 恒为空（导入的 MCP 由 T10 接）。
pub fn mcp_json() -> Value {
    json!({ "mcpServers": {} })
}

/// POSIX shell 的单引号转义：hook 命令由 CLI 交给 shell 执行，路径里有空格（`Application Support`）。
pub fn shell_quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', r"'\''"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn quote_survives_spaces_and_quotes() {
        assert_eq!(
            shell_quote("/Users/a b/Application Support/iWork/hook"),
            "'/Users/a b/Application Support/iWork/hook'"
        );
        assert_eq!(shell_quote("/x/it's"), r"'/x/it'\''s'");
    }

    #[test]
    fn excluded_pattern_text() {
        let cmd = |p: &str, s: Option<&str>| CommandPattern {
            program: p.into(),
            subcommand: s.map(Into::into),
        };
        assert_eq!(excluded_pattern(&cmd("git", Some("push"))), "git push *");
        assert_eq!(excluded_pattern(&cmd("gh", None)), "gh *");
    }
}
