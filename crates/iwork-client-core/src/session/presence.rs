//! 报在看哪（T09 方案 2.8，`ReportPresence`）：只供后台决定要不要发系统通知，Dock 跳动不看它。
//!
//! 由「窗口在前台 + 窗口开着 + 当前路由 + 对话视图是否已加载完」算出期望值，变了就发：
//! **离开立刻报，到来延后报**（R11）——失焦、切走、关窗马上发；反过来等稳定 [`ARRIVE_DELAY`] 再发。
//! 一次变化里既有离开又有到来（从对话 A 切到 B）时，先立刻发两者的交集（A 已经不看了），再延后发 B。
//! 它和订阅范围一样是期望状态：不进在飞命令表，断线不重发旧值；每次重新连上后发一次当前值。

use std::time::Duration;

use iwork_proto::pb;

use crate::store::{ScopeKind, Store, ViewState};

/// 到来要稳定这么久才报。
pub const ARRIVE_DELAY: Duration = Duration::from_millis(500);

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Presence {
    pub window_focused: bool,
    pub viewing_conversation_id: Option<String>,
    pub viewing_pending_list: bool,
}

impl Presence {
    pub fn to_proto(&self) -> pb::ReportPresence {
        pb::ReportPresence {
            window_focused: self.window_focused,
            viewing_conversation_id: self.viewing_conversation_id.clone(),
            viewing_pending_list: self.viewing_pending_list,
        }
    }
}

/// 用户现在看得到什么。窗口关着时什么都看不到；对话要视图已加载完（Ready）才算在看。
pub fn desired(store: &Store, window_open: bool) -> Presence {
    if !window_open {
        return Presence::default();
    }
    let conversation = store.visible.conversation_id.as_ref().filter(|cid| {
        store
            .subscription_of(&ScopeKind::Conversation((*cid).clone()))
            .and_then(|id| store.view_state(id))
            == Some(ViewState::Ready)
    });
    Presence {
        window_focused: store.focused,
        viewing_conversation_id: conversation.cloned(),
        viewing_pending_list: store.visible.pending,
    }
}

/// 从已经报过的 `sent` 变到 `want`：要立刻报的那一份（只有「离开」的部分），以及之后还要不要延后报 `want`。
pub fn split(sent: &Presence, want: &Presence) -> (Option<Presence>, bool) {
    let leave = Presence {
        window_focused: sent.window_focused && want.window_focused,
        viewing_conversation_id: (sent.viewing_conversation_id == want.viewing_conversation_id)
            .then(|| sent.viewing_conversation_id.clone())
            .flatten(),
        viewing_pending_list: sent.viewing_pending_list && want.viewing_pending_list,
    };
    let later = leave != *want;
    ((leave != *sent).then_some(leave), later)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn p(f: bool, c: Option<&str>, l: bool) -> Presence {
        Presence {
            window_focused: f,
            viewing_conversation_id: c.map(Into::into),
            viewing_pending_list: l,
        }
    }

    #[test]
    fn leaving_is_immediate_arriving_is_deferred() {
        // 失焦：只有离开，立刻报，不用再等
        assert_eq!(
            split(&p(true, Some("a"), false), &p(false, Some("a"), false)),
            (Some(p(false, Some("a"), false)), false)
        );
        // 回到前台：只有到来，不立刻报，延后报
        assert_eq!(
            split(&p(false, Some("a"), false), &p(true, Some("a"), false)),
            (None, true)
        );
        // A 切到 B：先立刻报「不在看 A」，再延后报 B
        assert_eq!(
            split(&p(true, Some("a"), false), &p(true, Some("b"), false)),
            (Some(p(true, None, false)), true)
        );
        // 没变
        assert_eq!(
            split(&p(true, None, true), &p(true, None, true)),
            (None, false)
        );
    }
}
