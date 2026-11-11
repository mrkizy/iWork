//! 设置页：通用开关、运行时（验证状态只在这里标，R10）、记住的规则、本机导入；另含「全部对话」与添加项目两页。

use iwork_proto::pb;

use super::sidebar::{conversation_item, waiting_conversations};
use super::{Ctx, text};
use crate::store::Store;

pub fn runtime_view(r: &pb::RuntimeStatus) -> pb::RuntimeView {
    let state = pb::RuntimeState::try_from(r.state).unwrap_or_default();
    let login = r.login.as_ref();
    pb::RuntimeView {
        runtime_id: r.runtime_id.clone(),
        display_name: r.display_name.clone(),
        state_label: text::runtime_state(state).into(),
        verification_label: text::verification(state).map(str::to_owned),
        version_label: r.cli_version.clone().unwrap_or_default(),
        login_label: match login {
            Some(l) if l.logged_in => format!("已登录 · {}", l.subscription_type),
            _ => "未登录".into(),
        },
        guidance: text::runtime_guidance(state, &r.display_name),
        can_redetect: matches!(
            state,
            pb::RuntimeState::Missing
                | pb::RuntimeState::NotLoggedIn
                | pb::RuntimeState::Incompatible
        ),
        ready: matches!(state, pb::RuntimeState::ReadyVerified),
    }
}

/// 记住的规则按「永久 / 本项目 / 本对话」分组（ui-m1.md 第 2 节第 15 条）。只列记住的规则。
fn rule_groups(rules: &[pb::Rule]) -> Vec<pb::RuleGroup> {
    use pb::scope::Kind as S;
    type Belongs = fn(&S) -> bool;
    let groups: [(&str, Belongs); 3] = [
        ("永久", |k| matches!(k, S::Global(_) | S::AssistantId(_))),
        ("本项目", |k| matches!(k, S::ProjectId(_))),
        ("本对话", |k| matches!(k, S::ConversationId(_))),
    ];
    groups
        .into_iter()
        .filter_map(|(title, belongs)| {
            let items: Vec<pb::RuleItem> = rules
                .iter()
                .filter(|r| r.layer == pb::RuleLayer::Remembered as i32)
                .filter(|r| {
                    r.scope
                        .as_ref()
                        .and_then(|s| s.kind.as_ref())
                        .is_some_and(belongs)
                })
                .map(|r| pb::RuleItem {
                    rule_id: r.id.clone(),
                    label: r.label.clone(),
                    can_revoke: true,
                })
                .collect();
            (!items.is_empty()).then(|| pb::RuleGroup {
                title: title.into(),
                rules: items,
            })
        })
        .collect()
}

fn import_groups(store: &Store) -> Vec<pb::ImportGroup> {
    use pb::LocalConfigKind as K;
    [(K::Mcp, "MCP"), (K::Skill, "技能"), (K::Plugin, "插件")]
        .into_iter()
        .filter_map(|(kind, title)| {
            let items: Vec<pb::ImportItem> = store
                .local_config
                .values()
                .flatten()
                .filter(|i| i.kind == kind as i32)
                .map(|i| pb::ImportItem {
                    item_id: i.id.clone(),
                    name: i.name.clone(),
                    description: i.description.clone(),
                    source: i.source.clone(),
                    imported: i.imported,
                })
                .collect();
            (!items.is_empty()).then(|| pb::ImportGroup {
                title: title.into(),
                items,
            })
        })
        .collect()
}

pub fn settings(store: &Store) -> pb::SettingsView {
    let s = store.settings.clone().unwrap_or_default();
    let rule_groups = store.rules.as_deref().map(rule_groups).unwrap_or_default();
    let import_groups = import_groups(store);
    pb::SettingsView {
        playful_ui: s.playful_ui,
        autostart: s.autostart,
        projects_root: s.projects_root,
        runtimes: store.runtimes.values().map(runtime_view).collect(),
        rules_loading: store.rules.is_none(),
        rules_empty_text: if store.rules.is_some() && rule_groups.is_empty() {
            text::RULES_EMPTY.into()
        } else {
            String::new()
        },
        rule_groups,
        import_empty_text: if import_groups.is_empty() {
            text::IMPORT_EMPTY.into()
        } else {
            String::new()
        },
        import_groups,
    }
}

pub fn conversation_list(store: &Store, ctx: Ctx) -> Option<pb::ConversationListView> {
    let list = store.conversation_list.as_ref()?;
    let waiting = waiting_conversations(store);
    let items: Vec<pb::ConversationItem> = list
        .items
        .iter()
        .map(|c| conversation_item(store, &waiting, ctx, c))
        .collect();
    let empty_text = match (
        items.is_empty() && !list.loading,
        list.query.filter.is_empty(),
    ) {
        (false, _) => String::new(),
        (true, true) => text::LIST_EMPTY.into(),
        (true, false) => text::list_filter_empty(&list.query.filter),
    };
    Some(pb::ConversationListView {
        project_id: list.query.project_id.clone(),
        filter: list.query.filter.clone(),
        items,
        has_more: list.has_more,
        loading: list.loading,
        empty_text,
    })
}

pub fn add_project(store: &Store) -> pb::AddProjectView {
    pb::AddProjectView {
        clone: store.clone_job.as_ref().map(|j| pb::CloneProgressView {
            job_id: j.progress.job_id.clone(),
            phase: j.progress.phase.clone(),
            percent: j.progress.percent,
            text: j.progress.text.clone(),
            can_retry: j.error.is_some(),
            error: j.error.clone(),
        }),
        drafts: store
            .summary
            .pending
            .iter()
            .filter_map(|p| match &p.item {
                Some(pb::pending_item::Item::AgentsMdDraft(d)) => Some(pb::DraftCardView {
                    card_id: d.card_id.clone(),
                    project_id: d.project_id.clone(),
                    project_name: store
                        .summary
                        .project(&d.project_id)
                        .map_or_else(String::new, |p| p.name.clone()),
                }),
                _ => None,
            })
            .collect(),
    }
}
