//! 助手主页与配置页（任务卡 3c）。

use iwork_proto::pb;

use super::avatar::{AvatarState, avatar, avatar_color_options};
use super::model_menu::{MenuInput, model_menu};
use super::text;
use crate::store::Store;

pub fn assistant(store: &Store, id: &str) -> Option<pb::AssistantProfileView> {
    let a = store.summary.assistant(id)?;
    // 摘要里的配置叠上还没落定的改动：摘要要等重查才追上，不叠会闪回旧值（store::assistant_edits）
    let c = store
        .assistant_edits
        .shown(id, &a.config.clone().unwrap_or_default());
    let model = c.model.clone().unwrap_or_default();
    Some(pb::AssistantProfileView {
        assistant_id: a.id.clone(),
        name: c.name.clone(),
        avatar: Some(avatar(store, id, AvatarState::Idle)),
        prompt: c.prompt.clone(),
        runtime_id: c.runtime_id.clone(),
        model_id: model.model_id,
        effort: model.effort.unwrap_or_default(),
        tools: [
            pb::ToolGroup::ReadAndSearch,
            pb::ToolGroup::EditFiles,
            pb::ToolGroup::RunCommands,
            pb::ToolGroup::Web,
        ]
        .into_iter()
        .map(|g| pb::ToolToggle {
            group: g as i32,
            label: text::tool_group(g).into(),
            enabled: !c.disabled_groups.contains(&(g as i32)),
        })
        .collect(),
        builtin: a.builtin,
        can_delete: !a.builtin,
        // 私聊工作目录的路径由后台记（技术设计 9.1）；摘要里还没有这个字段，删除确认框暂不写路径。
        dm_workdir: String::new(),
        version: a.version,
        runtime_options: store
            .runtimes
            .values()
            .enumerate()
            .map(|(i, r)| pb::OptionView {
                value: i32::try_from(i).unwrap_or(i32::MAX),
                label: r.display_name.clone(),
                ..Default::default()
            })
            .collect(),
        runtime_locked: false,
        activity_empty_text: text::ACTIVITY_EMPTY.into(),
        memory_empty_text: text::MEMORY_EMPTY.into(),
        model_menu: Some(model_menu(
            store,
            &MenuInput {
                runtime_id: &c.runtime_id,
                model: c.model.as_ref(),
                runtime_locked: false,
            },
        )),
        save_error: store.assistant_edits.error(id).map(str::to_owned),
        avatar_colors: avatar_color_options(c.avatar.as_ref().map_or("", |a| a.color.as_str())),
    })
}
