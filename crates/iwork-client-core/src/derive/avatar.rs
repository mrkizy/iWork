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
    let color = assistant
        .and_then(|a| a.config.as_ref())
        .and_then(|c| c.avatar.as_ref())
        .map(|a| a.color.clone())
        .unwrap_or_default();
    pb::AvatarView {
        character: character.to_owned(),
        // 换过形象的（预览用）用素材自带的颜色：配置里的颜色只给圆团子
        color: if character == DEFAULT_CHARACTER {
            color
        } else {
            String::new()
        },
        ..base
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
