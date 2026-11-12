//! 起 CLI 的环境变量（T06 方案 3.1a；P14，产品负责人 2026-11-12 定：不筛选）。
//!
//! CLI 的环境 = 桥接自己的环境原样继承，只额外设上两个开关 `CLAUDE_CODE_DISABLE_AUTO_MEMORY=1`、
//! `CLAUDE_CODE_DISABLE_BACKGROUND_TASKS=1`（都覆盖继承来的值）和 hook 通道的四个 `IWORK_HOOK_*`。不筛选是定下来的做法：正式包由登录自启项或 Finder 启动，环境本来就干净；从带着用户
//! 变量的终端、从 Claude Code 里起 iWork 时带进来的东西是已知限制（3.1a），由每一轮 init 的会话内容核对事后发现。
//!
//! 桥接不读、不记录任何环境变量的值（需求第 11 章）：[`LaunchEnv`] 的 `Debug` 只列变量名。

use std::ffi::{OsStr, OsString};
use std::fmt;

/// 关掉后台任务（D5，产品负责人 2026-11-12 定：M1 先关，M2 开放）：子代理在前台跑、Bash 不收 `run_in_background`。
/// 继承来的是别的值也覆盖成 `1`（3.1a）。M2 拿掉时只动这一处、禁用名单里的 Monitor 和 `events.rs` 里认后台信号的那一段
pub const DISABLE_BACKGROUND_TASKS: &str = "CLAUDE_CODE_DISABLE_BACKGROUND_TASKS";

/// 关掉 CLI 的自动记忆：设成 `1` 不建也不读自动记忆文件；继承来的是 `0`（强制打开）也要被它盖掉（3.1a）。
pub const DISABLE_AUTO_MEMORY: &str = "CLAUDE_CODE_DISABLE_AUTO_MEMORY";

/// 一份环境变量：桥接自己继承来的那份（`BridgeEnv.inherited`），或交给 CLI 的完整环境（薄壳起进程时先清空，
/// 再原样设这一份）。
#[derive(Clone, PartialEq, Eq)]
pub struct LaunchEnv(Vec<(OsString, OsString)>);

impl LaunchEnv {
    /// 由薄壳在启动时取（`std::env::vars_os()`）；测试直接给假的。
    pub fn from_vars(vars: Vec<(OsString, OsString)>) -> Self {
        Self(vars)
    }

    pub fn vars(&self) -> &[(OsString, OsString)] {
        &self.0
    }

    pub fn get(&self, name: &str) -> Option<&OsStr> {
        self.0
            .iter()
            .find(|(k, _)| k == name)
            .map(|(_, v)| v.as_os_str())
    }
}

/// 只列变量名：值里可能有密钥（`ANTHROPIC_API_KEY`、令牌），不进任何日志。
impl fmt::Debug for LaunchEnv {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_list()
            .entries(self.0.iter().map(|(k, _)| k))
            .finish()
    }
}

/// 继承的环境原样保留（次序不变），`set` 里的名字先从继承的里去掉，再按给的次序接在后面。
/// 名字按字节比较（M1 不支持 Windows，那边大小写不敏感的变量名不在这里处理）。
pub fn launch_env(inherited: &LaunchEnv, set: &[(&'static str, String)]) -> LaunchEnv {
    let mut vars: Vec<(OsString, OsString)> = inherited
        .vars()
        .iter()
        .filter(|(k, _)| !set.iter().any(|(name, _)| k == name))
        .cloned()
        .collect();
    vars.extend(
        set.iter()
            .map(|(k, v)| (OsString::from(k), OsString::from(v))),
    );
    LaunchEnv(vars)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn os(pairs: &[(&str, &str)]) -> Vec<(OsString, OsString)> {
        pairs
            .iter()
            .map(|(k, v)| (OsString::from(k), OsString::from(v)))
            .collect()
    }

    #[test]
    fn set_overrides_inherited_and_keeps_the_rest_in_order() {
        let env = launch_env(
            &LaunchEnv::from_vars(os(&[
                ("PATH", "/bin"),
                (DISABLE_AUTO_MEMORY, "0"),
                ("HOME", "/h"),
            ])),
            &[
                (DISABLE_AUTO_MEMORY, "1".into()),
                ("IWORK_HOOK_SOCK", "/s".into()),
            ],
        );
        assert_eq!(
            env.vars(),
            os(&[
                ("PATH", "/bin"),
                ("HOME", "/h"),
                (DISABLE_AUTO_MEMORY, "1"),
                ("IWORK_HOOK_SOCK", "/s"),
            ])
        );
    }

    #[test]
    fn debug_prints_names_only() {
        let env = launch_env(
            &LaunchEnv::from_vars(os(&[("ANTHROPIC_API_KEY", "sk-fake-secret")])),
            &[],
        );
        let text = format!("{env:?}");
        assert!(text.contains("ANTHROPIC_API_KEY"), "{text}");
        assert!(!text.contains("sk-fake-secret"), "{text}");
    }
}
