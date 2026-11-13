//! 「运行时 · 模型」菜单（ui-m1.md 第 2 节第 14 条）：运行时、模型（仅本对话）、思考强度三节。
//! 选中项、可否选、说明文字都在这里算；网页只画、只回传 value。

use iwork_proto::pb;

use super::text;
use crate::store::Store;

/// M1 还没接入的运行时：输入框的运行时菜单（画布 01）和设置 › 运行时（画布 12）都灰着列在末尾、标「M2 接入」，
/// 两处读这一份（ui-m1 2a 第 18 条 e）。接入后从这里删掉。
pub const UPCOMING_RUNTIMES: &[&str] = &["Codex", "Antigravity CLI"];

/// 菜单要的输入：当前运行时、当前模型选择、运行时一节是否已锁（对话开始后）。
pub struct MenuInput<'a> {
    pub runtime_id: &'a str,
    pub model: Option<&'a pb::ModelSelection>,
    pub runtime_locked: bool,
}

pub fn model_menu(store: &Store, input: &MenuInput<'_>) -> pb::ModelMenuView {
    let mut runtimes: Vec<pb::MenuChoice> = store
        .runtimes
        .values()
        .map(|r| {
            let current = r.runtime_id == input.runtime_id;
            pb::MenuChoice {
                value: r.runtime_id.clone(),
                label: r.display_name.clone(),
                selected: current,
                // M1 没有换运行时的命令：当前的就是唯一能选的（对话开始后整节置灰，见 runtime_locked）。
                enabled: current && !input.runtime_locked,
                tip: if input.runtime_locked {
                    text::RUNTIME_LOCKED_TIP.into()
                } else if current {
                    String::new()
                } else {
                    text::RUNTIME_NOT_IN_M1.into()
                },
            }
        })
        .collect();
    runtimes.extend(
        UPCOMING_RUNTIMES
            .iter()
            .filter(|name| !runtimes.iter().any(|r| r.label == **name))
            .map(|name| pb::MenuChoice {
                value: String::new(),
                label: (*name).to_owned(),
                selected: false,
                enabled: false,
                tip: text::RUNTIME_NOT_IN_M1.into(),
            })
            .collect::<Vec<_>>(),
    );
    let catalog = store.models.get(input.runtime_id);
    let current_model = input
        .model
        .map(|m| m.model_id.as_str())
        .filter(|m| !m.is_empty());
    let models: Vec<pb::MenuChoice> = catalog
        .map(|c| {
            c.models
                .iter()
                .map(|m| pb::MenuChoice {
                    value: m.model_id.clone(),
                    label: if m.display_name.is_empty() {
                        m.model_id.clone()
                    } else {
                        m.display_name.clone()
                    },
                    selected: current_model == Some(m.model_id.as_str()),
                    enabled: true,
                    tip: String::new(),
                })
                .collect()
        })
        .unwrap_or_default();
    let custom_model = match current_model {
        Some(m) if !models.iter().any(|c| c.value == m) => m.to_owned(),
        _ => String::new(),
    };
    let current_effort = input.model.and_then(|m| m.effort.as_deref());
    let efforts = catalog
        .map(|c| {
            c.effort_levels
                .iter()
                .map(|e| pb::MenuChoice {
                    value: e.clone(),
                    label: text::effort_label(e),
                    selected: current_effort == Some(e.as_str()),
                    // 思考强度跟着模型走（SetConversationModel 一起带）：没有当前模型时不能单独选。
                    enabled: current_model.is_some(),
                    tip: String::new(),
                })
                .collect()
        })
        .unwrap_or_default();
    pb::ModelMenuView {
        runtimes,
        models_loading: catalog.is_none(),
        models,
        custom_model,
        efforts,
    }
}
