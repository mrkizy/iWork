//! 设置与规则命令的受理计划（T10a 方案 2.6）：纯函数。

use std::path::PathBuf;

use iwork_domain::approval_mode::ProjectDefaultMode;
use iwork_domain::common::ModelSelection;
use iwork_domain::event::{
    DomainEvent, GlobalSettingsChanged, ProjectSettingsUpdated, RuleRevoked,
};
use iwork_domain::project::{AiReviewSettings, Project};
use iwork_domain::rule::RuleEntry;

use crate::refusal::Refusal;

/// `SetPlayfulUi`。
pub fn plan_set_playful_ui(enabled: bool) -> Vec<DomainEvent> {
    vec![
        GlobalSettingsChanged {
            playful_ui: Some(enabled),
            autostart: None,
            projects_root: None,
        }
        .into(),
    ]
}

/// `SetProjectsRoot`：只收绝对路径（相对路径相对谁都说不清）。
pub fn plan_set_projects_root(path: PathBuf) -> Result<Vec<DomainEvent>, Refusal> {
    if !path.is_absolute() {
        return Err(Refusal::InvalidArgument {
            field: "SetProjectsRoot.path",
            reason: format!("{} 不是绝对路径", path.display()),
        });
    }
    Ok(vec![
        GlobalSettingsChanged {
            playful_ui: None,
            autostart: None,
            projects_root: Some(path),
        }
        .into(),
    ])
}

/// `UpdateProjectSettings` 只带要改的部分（proto 注释）；没带的保持原值。
#[derive(Debug, Clone, Default)]
pub struct ProjectSettingsPatch {
    /// `Err(())` = 客户端要设全开（项目默认不能全开）
    pub default_approval_mode: Option<Result<ProjectDefaultMode, ()>>,
    pub default_model: Option<ModelSelection>,
    pub ai_review: Option<AiReviewSettings>,
}

/// `UpdateProjectSettings`：把改动合到现有设置上，事件带完整设置（`ProjectSettingsUpdated` 的约定）。
pub fn plan_update_project_settings(
    project: &Project,
    patch: ProjectSettingsPatch,
) -> Result<Vec<DomainEvent>, Refusal> {
    let mut settings = project.settings.clone();
    if let Some(mode) = patch.default_approval_mode {
        settings.default_approval_mode = mode.map_err(|()| Refusal::ApprovalModeNotAllowed)?;
    }
    if let Some(model) = patch.default_model {
        settings.default_model = Some(model);
    }
    if let Some(ai) = patch.ai_review {
        if ai.timeout_secs == 0 {
            return Err(Refusal::InvalidArgument {
                field: "UpdateProjectSettings.ai_review.timeout_secs",
                reason: "审查超时不能是 0".into(),
            });
        }
        settings.ai_review = ai;
    }
    Ok(vec![
        ProjectSettingsUpdated {
            project_id: project.id,
            settings,
        }
        .into(),
    ])
}

/// `RevokeRule`：只撤用户留下的规则（记住的、用户加的）。内置条目与安全清单条目不能撤（清单有自己的命令）。
pub fn plan_revoke_rule(
    rule: Option<&RuleEntry>,
    rule_id: &str,
) -> Result<Vec<DomainEvent>, Refusal> {
    let rule = rule.ok_or_else(|| Refusal::NotFound {
        entity: "rule",
        id: rule_id.to_owned(),
    })?;
    if rule.is_builtin() || rule.security_list().is_some() {
        return Err(Refusal::RuleNotRevocable {
            rule: rule_id.to_owned(),
        });
    }
    Ok(vec![RuleRevoked { rule_id: rule.id }.into()])
}
