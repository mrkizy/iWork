//! 会话级事实与模型上报（T06 方案 6.2）。
//!
//! `system/init` 第一条输入之后才出、每一轮开头都有一个（S0 实测，2.1.322）。下面的规则不依赖「每轮都有」：
//! 有就用这一轮的，没有就用缓存的。**每个 `session/prompt` 报一次模型**：
//! - 回合开始时缓存里已经有模型（第二个回合起）→ 先报；
//! - 还没有（第一个回合）→ 这个回合里 `init` 一到就报；
//! - 这一轮已经报过之后再来的 `init` → 只更新缓存、过核对；模型和已报的不一样记一条诊断，不重复报；
//! - 回合结束时还没报过 → 记一条诊断，不编造。
//!
//! 每个 `init` 都过一遍核对，任何一项不过 → `isolation_broken`（停会话的接线在 S6）：
//! - 权限模式（方案 3.5 第 2 处）：不是选定配置的 `dontAsk`；
//! - 会话内容（3.5，2026-11-12 加；和 4.7 的对账同一类，是出事后的发现、不是执行前的保证）：`plugins` 的每一项要么
//!   `source` 以 `@builtin` 结尾（CLI 自带的），要么 `path` 落在这次给的某个插件目录之下（按路径分量比）；`plugins`
//!   没有、或某一项缺 `source`，同样不过。`memory_paths.auto` 没有或等于这次给的改道目录才过（3.1a）。
//!   只核 `plugins`，不核 `skills`、`agents`、`mcp_servers`（3.5 写了为什么）。

use std::path::{Component, Path, PathBuf};

use crate::launch::settings::SELECTED;

use super::events::{Init, InitPlugin, MemoryAuto, RateLimitInfo};

/// 隔离失效：设置文件被忽略或被改，或者有不该进会话的东西进来了，会话不能继续（3.1b 的「种类」）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum IsolationBroken {
    /// 权限模式不是选定配置的；缺失为 `None`
    PermissionMode { reported: Option<String> },
    /// 多出来一个插件：既不是 CLI 自带的，也不在这次给的插件目录下。只记名字（路径里有用户名）
    UnexpectedPlugin { name: String },
    /// `plugins` 这个键没有、不是数组，或某一项缺 `source`：核对不了，不当成通过
    PluginsUnreadable,
    /// `memory_paths.auto` 是别的路径或不是字符串
    MemoryPath,
}

/// 这次会话给出去的东西：会话内容核对拿它比。
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SessionExpect {
    /// `SessionSetup.plugin_dirs`
    pub plugin_dirs: Vec<PathBuf>,
    /// `<会话目录>/m`
    pub memory_dir: PathBuf,
}

/// CLI 自带的插件，或者 `path` 落在给的某个插件目录之下。
fn plugin_expected(p: &InitPlugin, expect: &SessionExpect) -> Result<bool, IsolationBroken> {
    let Some(source) = &p.source else {
        return Err(IsolationBroken::PluginsUnreadable);
    };
    if source.ends_with("@builtin") {
        return Ok(true);
    }
    // `Path` 按分量比（中间的 `.`、多余与末尾的分隔符不算数），不解析链接、不碰文件系统
    Ok(p.path.as_deref().map(Path::new).is_some_and(|path| {
        // 带 `..` 的不算：`<给的目录>/../别处` 按分量比也以给的目录开头
        !path.components().any(|c| matches!(c, Component::ParentDir))
            && expect.plugin_dirs.iter().any(|dir| path.starts_with(dir))
    }))
}

/// 会话内容核对（3.5）。
fn check_content(init: &Init, expect: &SessionExpect) -> Result<(), IsolationBroken> {
    let plugins = init
        .plugins
        .as_ref()
        .ok_or(IsolationBroken::PluginsUnreadable)?;
    for p in plugins {
        if !plugin_expected(p, expect)? {
            return Err(IsolationBroken::UnexpectedPlugin {
                // 名字没有就用 source（形如「名字@来源」，不带路径）
                name: p
                    .name
                    .clone()
                    .or_else(|| p.source.clone())
                    .unwrap_or_default(),
            });
        }
    }
    match &init.memory_auto {
        MemoryAuto::Absent => Ok(()),
        MemoryAuto::Path(p) if Path::new(p) == expect.memory_dir => Ok(()),
        MemoryAuto::Path(_) | MemoryAuto::Malformed => Err(IsolationBroken::MemoryPath),
    }
}

/// 诊断（只进桥接的诊断记录，不进事件）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Diagnostic {
    /// 这一轮已报过模型之后，`init` 报了另一个模型（进程没换、模型是启动时定的，不该发生）
    ModelChangedMidTurn,
    /// 回合结束了还没拿到模型
    ModelNeverReported,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SessionFacts {
    pub model: Option<String>,
    pub session_id: Option<String>,
    pub permission_mode: Option<String>,
    pub tools: Vec<String>,
}

/// 事实缓存 + 这一轮的上报状态。一个桥接一生只起一个 CLI，所以只有一份。
#[derive(Debug, Default)]
pub struct Facts {
    pub session: SessionFacts,
    /// 会话内容核对拿它比
    expect: SessionExpect,
    /// 当前回合已报的模型；不在回合里时为 `None`
    reported: Option<Option<String>>,
    /// 回合之外到的额度读数，下一回合开头补报（方案 6.4）
    pending_quota: Option<RateLimitInfo>,
}

/// 回合开始时要先报的东西。
#[derive(Debug, Default, PartialEq)]
pub struct TurnStart {
    pub model: Option<String>,
    pub quota: Option<RateLimitInfo>,
}

impl Facts {
    pub fn new(expect: SessionExpect) -> Self {
        Self {
            expect,
            ..Self::default()
        }
    }

    /// `session/prompt` 到了、用户消息写给 CLI 之前。
    pub fn begin_turn(&mut self) -> TurnStart {
        let model = self.session.model.clone();
        self.reported = Some(model.clone());
        TurnStart {
            model,
            quota: self.pending_quota.take(),
        }
    }

    /// 一个 `init`。返回这一轮要报的模型（还没报过才有）与诊断。
    pub fn on_init(
        &mut self,
        init: &Init,
    ) -> Result<(Option<String>, Option<Diagnostic>), IsolationBroken> {
        if init.permission_mode.as_deref() != Some(SELECTED.default_mode) {
            return Err(IsolationBroken::PermissionMode {
                reported: init.permission_mode.clone(),
            });
        }
        check_content(init, &self.expect)?;
        self.session = SessionFacts {
            model: init.model.clone().or(self.session.model.take()),
            session_id: init.session_id.clone().or(self.session.session_id.take()),
            permission_mode: init.permission_mode.clone(),
            tools: init.tools.clone(),
        };
        let model = self.session.model.clone();
        match &mut self.reported {
            // 回合之外：只更新缓存
            None => Ok((None, None)),
            Some(already @ None) => {
                *already = model.clone();
                Ok((model, None))
            }
            Some(Some(prev)) if Some(prev.as_str()) != model.as_deref() => {
                Ok((None, Some(Diagnostic::ModelChangedMidTurn)))
            }
            Some(Some(_)) => Ok((None, None)),
        }
    }

    /// 额度读数：回合里的当场报（返回它），回合外的攒到下一回合开头。
    pub fn on_rate_limit(&mut self, info: RateLimitInfo) -> Option<RateLimitInfo> {
        if self.reported.is_some() {
            Some(info)
        } else {
            self.pending_quota = Some(info);
            None
        }
    }

    /// 回合结束（`result` 或 CLI 没了）。
    pub fn end_turn(&mut self) -> Option<Diagnostic> {
        match self.reported.take() {
            Some(None) => Some(Diagnostic::ModelNeverReported),
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn init(model: &str) -> Init {
        Init {
            session_id: Some("sid".into()),
            model: Some(model.into()),
            permission_mode: Some("dontAsk".into()),
            tools: vec!["Bash".into()],
            plugins: Some(vec![]),
            memory_auto: MemoryAuto::Absent,
        }
    }

    fn plugin(name: &str, source: &str, path: &str) -> InitPlugin {
        InitPlugin {
            name: Some(name.into()),
            source: Some(source.into()),
            path: Some(path.into()),
        }
    }

    fn expect() -> SessionExpect {
        SessionExpect {
            plugin_dirs: vec!["/data/imports/plugins/p1".into()],
            memory_dir: "/data/s/abc/m".into(),
        }
    }

    /// 第一个回合：init 在第一条输入之后才到，到了就报；同一轮再来的 init 不重复报。
    #[test]
    fn first_turn_reports_on_init() {
        let mut f = Facts::default();
        assert_eq!(f.begin_turn(), TurnStart::default());
        assert_eq!(
            f.on_init(&init("haiku")).unwrap(),
            (Some("haiku".into()), None)
        );
        assert_eq!(f.on_init(&init("haiku")).unwrap(), (None, None));
        assert_eq!(f.end_turn(), None);
    }

    /// 第二个回合起：开头就报缓存的模型；这一轮开头的 init 只更新缓存。
    #[test]
    fn later_turns_report_cached_model_up_front() {
        let mut f = Facts::default();
        f.begin_turn();
        f.on_init(&init("haiku")).unwrap();
        f.end_turn();
        assert_eq!(f.begin_turn().model.as_deref(), Some("haiku"));
        assert_eq!(f.on_init(&init("haiku")).unwrap(), (None, None));
        // 报了别的模型：诊断，不重复报；下一回合报更新后的缓存
        assert_eq!(
            f.on_init(&init("sonnet")).unwrap(),
            (None, Some(Diagnostic::ModelChangedMidTurn))
        );
        f.end_turn();
        assert_eq!(f.begin_turn().model.as_deref(), Some("sonnet"));
    }

    #[test]
    fn turn_without_model_is_diagnosed() {
        let mut f = Facts::default();
        f.begin_turn();
        assert_eq!(f.end_turn(), Some(Diagnostic::ModelNeverReported));
        // 回合之外的 init 只更新缓存
        assert_eq!(f.on_init(&init("haiku")).unwrap(), (None, None));
        assert_eq!(f.session.model.as_deref(), Some("haiku"));
    }

    /// 权限模式不是 dontAsk（设置文件被忽略时 CLI 报 default）或缺失：isolation_broken。
    #[test]
    fn permission_mode_is_checked_on_every_init() {
        let mut f = Facts::default();
        let mut i = init("haiku");
        i.permission_mode = Some("default".into());
        assert_eq!(
            f.on_init(&i),
            Err(IsolationBroken::PermissionMode {
                reported: Some("default".into())
            })
        );
        i.permission_mode = None;
        assert!(f.on_init(&i).is_err());
        assert!(f.on_init(&init("haiku")).is_ok(), "对照");
    }

    /// 3.5：多出一个不是自带、也不在给的插件目录下的插件（用户自己的插件经环境带进来）。
    #[test]
    fn init_with_unexpected_plugin_breaks_isolation() {
        let mut f = Facts::new(expect());
        let mut i = init("haiku");
        i.plugins = Some(vec![
            plugin("code-review", "code-review@builtin", "code-review"),
            plugin("mine", "mine@inline", "/home/u/.claude/plugins/mine"),
        ]);
        assert_eq!(
            f.on_init(&i),
            Err(IsolationBroken::UnexpectedPlugin {
                name: "mine".into()
            })
        );
        // 前缀相同但不是这个目录之下（按路径分量比，不按字符串）
        i.plugins = Some(vec![plugin("x", "x@inline", "/data/imports/plugins/p10/x")]);
        assert!(f.on_init(&i).is_err());
        // `..` 走出给的目录
        i.plugins = Some(vec![plugin(
            "x",
            "x@inline",
            "/data/imports/plugins/p1/../../../../home/u/x",
        )]);
        assert!(f.on_init(&i).is_err());
        // 相对路径不会被当成落在某个绝对目录下
        i.plugins = Some(vec![plugin("x", "x@inline", "data/imports/plugins/p1")]);
        assert!(f.on_init(&i).is_err());
        // 没有 path 的非自带插件
        i.plugins = Some(vec![InitPlugin {
            path: None,
            ..plugin("x", "x@inline", "")
        }]);
        assert!(f.on_init(&i).is_err());
    }

    #[test]
    fn init_with_given_plugin_dir_passes() {
        let mut f = Facts::new(expect());
        let mut i = init("haiku");
        i.plugins = Some(vec![
            plugin("code-review", "code-review@builtin", "code-review"),
            plugin("p1", "p1@inline", "/data/imports/plugins/p1"),
            plugin("p1b", "p1b@inline", "/data/imports/plugins/./p1/sub/"),
        ]);
        assert!(f.on_init(&i).is_ok());
    }

    /// `plugins` 没有、不是数组（事件层记成 `None`），或某一项缺 `source`：核对不了，不算过。
    #[test]
    fn init_without_plugins_key_breaks_isolation() {
        let mut f = Facts::new(expect());
        let mut i = init("haiku");
        i.plugins = None;
        assert_eq!(f.on_init(&i), Err(IsolationBroken::PluginsUnreadable));
        i.plugins = Some(vec![InitPlugin {
            source: None,
            ..plugin("p1", "", "/data/imports/plugins/p1")
        }]);
        assert_eq!(f.on_init(&i), Err(IsolationBroken::PluginsUnreadable));
    }

    #[test]
    fn init_with_foreign_memory_path_breaks_isolation() {
        let mut f = Facts::new(expect());
        let mut i = init("haiku");
        i.memory_auto = MemoryAuto::Path("/home/u/.claude/projects/-x-/memory".into());
        assert_eq!(f.on_init(&i), Err(IsolationBroken::MemoryPath));
        i.memory_auto = MemoryAuto::Malformed;
        assert_eq!(f.on_init(&i), Err(IsolationBroken::MemoryPath));
    }

    #[test]
    fn init_with_redirected_or_absent_memory_path_passes() {
        let mut f = Facts::new(expect());
        let mut i = init("haiku");
        i.memory_auto = MemoryAuto::Path("/data/s/abc/m/".into());
        assert!(f.on_init(&i).is_ok());
        i.memory_auto = MemoryAuto::Absent;
        assert!(f.on_init(&i).is_ok());
    }

    #[test]
    fn quota_outside_turn_waits_for_next_turn() {
        let info = RateLimitInfo {
            windows: vec![],
            incomplete: 0,
        };
        let mut f = Facts::default();
        assert_eq!(f.on_rate_limit(info.clone()), None);
        assert_eq!(f.begin_turn().quota, Some(info.clone()));
        assert_eq!(f.on_rate_limit(info.clone()), Some(info));
    }
}
