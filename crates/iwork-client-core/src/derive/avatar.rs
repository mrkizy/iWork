//! 助手头像（ui-m1.md 3.3、2a 第 13 条）：画哪个形象、什么表情、配什么动效，全在这里定，网页只按 id 查素材。
//!
//! 形象与表情给字符串 id，对应 `design-system/characters/<形象>-<表情>.svg`：素材加了新表情，
//! 只要在 [`MOODS`] 里让某个状态用上它，协议与网页都不用改。
//! 内置 iWork 画应用标志；其余助手一律画 [`DEFAULT_CHARACTER`]，颜色取配置里的（M1 没有选形象的入口）。

use std::collections::BTreeMap;

use iwork_proto::pb;

use crate::store::Store;

/// 新建助手的形象：圆团子，颜色在配置里换（design-system README「助手形象」）。
pub const DEFAULT_CHARACTER: &str = "blob";

/// 助手头像能选的配色（ui-m1 2a 第 18 条 a），按配置页的显示顺序：(设计令牌名, 颜色名)。存进配置的是令牌名，
/// 不是色值：同一个令牌在浅色、深色下取不同的色。不放 ch3（深色下和天蓝几乎一样）、ch11（和芥末黄太近）、
/// ch10（发色）、ch12/ch13（T 恤色）。
pub const AVATAR_COLORS: [(&str, &str); 8] = [
    ("ch1", "赭红"),
    ("ch2", "芥末黄"),
    ("ch4", "天蓝"),
    ("ch5", "薄荷绿"),
    ("ch6", "淡紫"),
    ("ch7", "青绿"),
    ("ch8", "粉"),
    ("ch9", "驼棕"),
];

/// 没选过、或配置里的值不在 [`AVATAR_COLORS`] 里时用的配色：天蓝。
pub const DEFAULT_AVATAR_COLOR: &str = "ch4";

/// 配置里存的配色 → 要画的令牌名。不认识的值（含空、旧数据里的色值）一律当默认天蓝。
pub fn avatar_color(stored: &str) -> &'static str {
    AVATAR_COLORS
        .iter()
        .find(|(token, _)| *token == stored)
        .map_or(DEFAULT_AVATAR_COLOR, |(token, _)| token)
}

/// 配置页的 8 个色块：配置里存的值对应的那块选中（不认识的值选中默认那块）。
pub fn avatar_color_options(stored: &str) -> Vec<pb::AvatarColorOption> {
    let current = avatar_color(stored);
    AVATAR_COLORS
        .iter()
        .map(|(token, name)| pb::AvatarColorOption {
            token: (*token).to_owned(),
            label: format!("头像配色 {name}"),
            selected: *token == current,
        })
        .collect()
}

/// 头像此刻的状态。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AvatarState {
    Idle,
    Working,
    Waiting,
    /// 最近一个回合失败、且没有等你处理的事。
    Failed,
}

/// 状态 → 表情 id 与配合的动效（design-system README「状态与成员状态的对应」）。加一种状态 = 加一行。
pub const MOODS: [(AvatarState, &str, &str); 4] = [
    (AvatarState::Idle, "idle", ""),
    (AvatarState::Working, "work", "float"),
    (AvatarState::Waiting, "wait", ""),
    // 出错：不浮动、不弹跳（2a 第 13 条）。
    (AvatarState::Failed, "sorry", ""),
];

/// 回合状态 → 头像状态（表情随状态变化，与轻松元素开关无关）。
pub fn state_of_turn(state: Option<i32>) -> AvatarState {
    use pb::TurnState as S;
    match state.and_then(|s| S::try_from(s).ok()) {
        Some(S::Queued | S::Running | S::Cancelling) => AvatarState::Working,
        Some(S::WaitingApproval | S::Interrupted) => AvatarState::Waiting,
        Some(S::Failed) => AvatarState::Failed,
        _ => AvatarState::Idle,
    }
}

/// 只给录像与预览：按助手 id 换形象（阿森、林岚、老周）。M1 没有选形象的入口，正式运行时为空。
/// 助手配置有了形象字段就删掉（见 `Store::character_overrides`）。
pub type CharacterOverrides = BTreeMap<String, String>;

pub fn avatar(store: &Store, assistant_id: &str, state: AvatarState) -> pb::AvatarView {
    let assistant = store.summary.assistant(assistant_id);
    let (_, mood, motion) = MOODS
        .iter()
        .find(|(s, _, _)| *s == state)
        .expect("MOODS 覆盖每一种 AvatarState");
    let base = pb::AvatarView {
        mood: (*mood).to_owned(),
        motion: (*motion).to_owned(),
        ..Default::default()
    };
    if assistant.is_some_and(|a| a.builtin) {
        return pb::AvatarView {
            app_logo: true,
            ..base
        };
    }
    let character = store
        .character_overrides
        .get(assistant_id)
        .map_or(DEFAULT_CHARACTER, String::as_str);
    let stored = assistant
        .and_then(|a| a.config.as_ref())
        .and_then(|c| c.avatar.as_ref())
        .map_or("", |a| a.color.as_str());
    pb::AvatarView {
        character: character.to_owned(),
        // 换过形象的（预览用）用素材自带的颜色：配置里的颜色只给圆团子
        color: if character == DEFAULT_CHARACTER {
            avatar_color(stored).to_owned()
        } else {
            String::new()
        },
        ..base
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 存的是 8 个令牌名之一就照用；空、旧数据里的色值、不在名单里的令牌（ch3 等）都当天蓝 ch4。
    #[test]
    fn unknown_avatar_colors_fall_back_to_sky_blue() {
        assert_eq!(avatar_color("ch1"), "ch1");
        assert_eq!(avatar_color("ch9"), "ch9");
        for stored in ["", "#4F7DF3", "ch3", "ch10", "CH1", "var(--ch1)"] {
            assert_eq!(avatar_color(stored), "ch4", "{stored:?}");
        }
    }

    /// 配置页 8 块按显示顺序，名字进读屏名称，只选中一块；不认识的值选中天蓝那块。
    #[test]
    fn color_options_select_exactly_one() {
        let opts = avatar_color_options("ch6");
        assert_eq!(
            opts.iter().map(|o| o.token.as_str()).collect::<Vec<_>>(),
            ["ch1", "ch2", "ch4", "ch5", "ch6", "ch7", "ch8", "ch9"]
        );
        assert_eq!(opts[4].label, "头像配色 淡紫");
        let selected = |o: &[pb::AvatarColorOption]| {
            o.iter()
                .filter(|x| x.selected)
                .map(|x| x.token.clone())
                .collect::<Vec<_>>()
        };
        assert_eq!(selected(&opts), ["ch6"]);
        assert_eq!(selected(&avatar_color_options("#4F7DF3")), ["ch4"]);
    }

    #[test]
    fn every_state_has_exactly_one_mood() {
        for s in [
            AvatarState::Idle,
            AvatarState::Working,
            AvatarState::Waiting,
            AvatarState::Failed,
        ] {
            assert_eq!(MOODS.iter().filter(|(x, _, _)| *x == s).count(), 1, "{s:?}");
        }
    }

    #[test]
    fn failed_turn_is_sorry_without_motion() {
        let (_, mood, motion) = MOODS
            .iter()
            .find(|(s, _, _)| *s == state_of_turn(Some(pb::TurnState::Failed as i32)))
            .unwrap();
        assert_eq!((*mood, *motion), ("sorry", ""));
    }
}
