//! 设置与规则命令的外壳（T10a 方案 2.6）。

use iwork_app::convert::{
    approval_mode_from_proto, model_selection_from_proto, project_default_mode_from_proto,
};
use iwork_app::refusal::Refusal;
use iwork_app::settings_command::{
    ProjectSettingsPatch, plan_revoke_rule, plan_set_playful_ui, plan_set_projects_root,
    plan_update_project_settings,
};
use iwork_domain::approval_mode::ApprovalMode;
use iwork_domain::id::{CommandId, ProjectId};
use iwork_domain::project::AiReviewSettings;
use iwork_ports::read_model::ReadModelPort;
use iwork_ports::store::{CommitReceipt, StoragePort};
use iwork_proto::pb;

use super::dispatch::{CommandError, Dispatcher, with_replan};
use super::engine::EngineCommands;
use super::turn::parse;

impl<St, Rd, E> Dispatcher<St, Rd, E>
where
    St: StoragePort + 'static,
    Rd: ReadModelPort + 'static,
    E: EngineCommands,
{
    pub(super) async fn set_playful_ui(
        &self,
        id: CommandId,
        c: pb::SetPlayfulUi,
    ) -> Result<CommitReceipt, CommandError> {
        self.commit(id, plan_set_playful_ui(c.enabled)).await
    }

    pub(super) async fn set_projects_root(
        &self,
        id: CommandId,
        c: pb::SetProjectsRoot,
    ) -> Result<CommitReceipt, CommandError> {
        let events = plan_set_projects_root(c.path.into())?;
        self.commit(id, events).await
    }

    pub(super) async fn update_project_settings(
        &self,
        id: CommandId,
        c: pb::UpdateProjectSettings,
    ) -> Result<CommitReceipt, CommandError> {
        const K: &str = "UpdateProjectSettings";
        let project: ProjectId = parse(&c.project_id)?;
        let default_approval_mode = match c.default_approval_mode {
            None => None,
            Some(m)
                if approval_mode_from_proto(K, "default_approval_mode", m)?
                    == ApprovalMode::FullAccess =>
            {
                Some(Err(()))
            }
            Some(m) => Some(Ok(project_default_mode_from_proto(
                K,
                "default_approval_mode",
                m,
            )?)),
        };
        let ai_review = c
            .ai_review
            .as_ref()
            .map(|a| -> Result<_, CommandError> {
                Ok(AiReviewSettings {
                    enabled: a.enabled,
                    runtime_id: a.runtime_id.as_deref().map(parse).transpose()?,
                    model: a.model.as_ref().map(model_selection_from_proto),
                    timeout_secs: a.timeout_secs,
                })
            })
            .transpose()?;
        let patch = ProjectSettingsPatch {
            default_approval_mode,
            default_model: c.default_model.as_ref().map(model_selection_from_proto),
            ai_review,
        };
        // 合到读到的设置上；读和提交之间别人改了设置，后到的这一笔整份覆盖（设置没有提交时校验）
        with_replan(|| async {
            let projects = self.ports.read.projects().await?;
            let current = projects
                .iter()
                .map(|p| &p.project)
                .find(|p| p.id == project)
                .ok_or_else(|| Refusal::NotFound {
                    entity: "project",
                    id: project.to_string(),
                })?;
            let events = plan_update_project_settings(current, patch.clone())?;
            self.commit(id, events).await
        })
        .await
    }

    pub(super) async fn revoke_rule(
        &self,
        id: CommandId,
        c: pb::RevokeRule,
    ) -> Result<CommitReceipt, CommandError> {
        let rules = self.ports.read.rules(None).await?;
        let rule = rules.iter().find(|r| r.id.to_string() == c.rule_id);
        let events = plan_revoke_rule(rule, &c.rule_id)?;
        self.commit(id, events).await
    }
}
