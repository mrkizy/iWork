//! 会话级事实与模型上报（T06 方案 6.2）。
//!
//! `system/init` 第一条输入之后才出、每一轮开头都有一个（S0 实测，2.1.322）。下面的规则不依赖「每轮都有」：
//! 有就用这一轮的，没有就用缓存的。**每个 `session/prompt` 报一次模型**：
//! - 回合开始时缓存里已经有模型（第二个回合起）→ 先报；
//! - 还没有（第一个回合）→ 这个回合里 `init` 一到就报；
//! - 这一轮已经报过之后再来的 `init` → 只更新缓存、过核对；模型和已报的不一样记一条诊断，不重复报；
//! - 回合结束时还没报过 → 记一条诊断，不编造。
//!
//! 每个 `init` 都过一遍权限模式核对（方案 3.5 第 2 处）：不是选定配置的 `dontAsk` → `isolation_broken`。

use crate::launch::settings::SELECTED;

use super::events::{Init, RateLimitInfo};

/// `init` 的权限模式不是选定配置的（含缺失）：设置文件被忽略或被改，会话不能继续。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IsolationBroken {
    /// 报来的权限模式；缺失为 `None`。只用于诊断
    pub reported: Option<String>,
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
            return Err(IsolationBroken {
                reported: init.permission_mode.clone(),
            });
        }
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
            Err(IsolationBroken {
                reported: Some("default".into())
            })
        );
        i.permission_mode = None;
        assert!(f.on_init(&i).is_err());
        assert!(f.on_init(&init("haiku")).is_ok(), "对照");
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
