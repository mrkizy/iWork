//! 设置页：通用开关、运行时（验证状态只在这里标，R10）、记住的规则、本机导入；另含「全部对话」一页。

use iwork_proto::pb;

use super::sidebar::{conversation_item, waiting_conversations};
use super::{Ctx, text, to_system_time};
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
            Some(l) if l.logged_in => text::login_label(&l.subscription_type),
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
        upcoming: false,
    }
}

/// 设置 › 运行时里列出、但 M1 还没接入的运行时（画布 12；ui-m1 第 1 节：Codex、agy 显示「M2 接入」）。
/// 输入框的运行时菜单照画布 01 只列 Codex（model_menu::UPCOMING_RUNTIMES）。接入后从这里删掉。
pub const UPCOMING_IN_SETTINGS: &[&str] = &["Codex", "Antigravity CLI"];

fn upcoming_runtime(name: &str) -> pb::RuntimeView {
    pb::RuntimeView {
        display_name: name.to_owned(),
        state_label: text::RUNTIME_NOT_IN_M1.into(),
        upcoming: true,
        ..Default::default()
    }
}

/// 记住的规则分组（ui-m1.md 第 2 节第 15 条）：「永久」一组（全局与记在助手上的），之后每个项目一组「本项目 · 名字」、
/// 每个对话一组「本对话 · 标题」（UI 审查 g3 2/F-2：标题写明是哪个）。每行写明记在哪、什么时候记的。只列记住的规则。
fn rule_groups(store: &Store, ctx: Ctx, rules: &[pb::Rule]) -> Vec<pb::RuleGroup> {
    use pb::scope::Kind as S;
    let tz = jiff::tz::TimeZone::system();
    // 分组键：(先后, 目标 id)。永久一组在前，其次按项目、对话，组内照后台给的顺序
    let mut groups: Vec<((u8, String), pb::RuleGroup)> = Vec::new();
    for r in rules
        .iter()
        .filter(|r| r.layer == pb::RuleLayer::Remembered as i32)
    {
        let Some(kind) = r.scope.as_ref().and_then(|s| s.kind.as_ref()) else {
            continue;
        };
        let (key, title) = match kind {
            S::Global(_) | S::AssistantId(_) => ((0, String::new()), "永久".to_owned()),
            S::ProjectId(p) => (
                (1, p.clone()),
                format!("本项目 · {}", project_label(store, p)),
            ),
            S::ConversationId(c) => (
                (2, c.clone()),
                format!("本对话 · {}", conversation_label(store, c)),
            ),
        };
        let item = pb::RuleItem {
            rule_id: r.id.clone(),
            label: r.label.clone(),
            can_revoke: true,
            label_spans: text::code_spans(&r.label),
            owner_label: owner_label(store, kind),
            created_label: r
                .created_at
                .as_ref()
                .map(|t| text::clock_or_date(ctx.now, to_system_time(t), &tz))
                .unwrap_or_default(),
        };
        match groups.iter_mut().find(|(k, _)| *k == key) {
            Some((_, g)) => g.rules.push(item),
            None => groups.push((
                key,
                pb::RuleGroup {
                    title,
                    rules: vec![item],
                },
            )),
        }
    }
    groups.sort_by_key(|((order, _), _)| *order);
    groups.into_iter().map(|(_, g)| g).collect()
}

fn project_label(store: &Store, id: &str) -> String {
    store
        .summary
        .project(id)
        .map_or_else(|| text::RULE_TARGET_GONE.to_owned(), |p| p.name.clone())
}

fn assistant_label(store: &Store, id: &str) -> String {
    store
        .summary
        .assistant(id)
        .and_then(|a| a.config.as_ref())
        .map_or_else(|| text::RULE_TARGET_GONE.to_owned(), |c| c.name.clone())
}

fn conversation_label(store: &Store, id: &str) -> String {
    store.summary.conversation(id).map_or_else(
        || text::RULE_TARGET_GONE.to_owned(),
        |c| super::conversation_title(store, c),
    )
}

/// 规则记在哪个助手或项目上。对话范围的规则算在对话所属的项目或助手上。
fn owner_label(store: &Store, kind: &pb::scope::Kind) -> String {
    use pb::scope::Kind as S;
    match kind {
        S::Global(_) => text::RULE_OWNER_ALL.to_owned(),
        S::ProjectId(p) => format!("项目 {}", project_label(store, p)),
        S::AssistantId(a) => format!("助手 {}", assistant_label(store, a)),
        S::ConversationId(c) => match store.summary.conversation(c).and_then(|c| c.kind.as_ref()) {
            Some(pb::conversation::Kind::Project(p)) => {
                format!("项目 {}", project_label(store, &p.project_id))
            }
            Some(pb::conversation::Kind::AssistantDm(dm)) => {
                format!("助手 {}", assistant_label(store, &dm.assistant_id))
            }
            None => text::RULE_TARGET_GONE.to_owned(),
        },
    }
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

pub fn settings(store: &Store, ctx: Ctx) -> pb::SettingsView {
    let s = store.settings.clone().unwrap_or_default();
    let rule_groups = store
        .rules
        .as_deref()
        .map(|r| rule_groups(store, ctx, r))
        .unwrap_or_default();
    let import_groups = import_groups(store);
    pb::SettingsView {
        playful_ui: s.playful_ui,
        autostart: s.autostart,
        projects_root: s.projects_root,
        runtimes: store
            .runtimes
            .values()
            .map(runtime_view)
            .chain(
                UPCOMING_IN_SETTINGS
                    .iter()
                    .filter(|n| !store.runtimes.values().any(|r| r.display_name == **n))
                    .map(|n| upcoming_runtime(n)),
            )
            .collect(),
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
