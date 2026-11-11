//! 新建对话以后跳过去：用户在这个客户端里点了「新对话」，对话建好就打开它。
//!
//! 命令受理只带回 seq 范围（`CommandAccepted.first_seq..=last_seq`），对话 id 在 `ConversationCreated` 事件里。
//! 两者走的是不同的路（回包与事件流），谁先到都可能；所以两边各留一点记录，后到的那一方配上对就跳。
//! 别的客户端建的对话只进「最近建的」，没有配上的受理范围，不跳。

use std::collections::VecDeque;

/// 「最近建的对话」留多少条：只用来等稍后才到的受理回包，不需要多。
const RECENT_CAP: usize = 16;

#[derive(Debug, Default)]
pub struct CreatedWatch {
    /// 受理了、还没见到对应事件的 seq 范围。
    pending: Vec<(u64, u64)>,
    /// 见到了、还没配上受理回包的 (seq, 对话 id)。
    recent: VecDeque<(u64, String)>,
}

impl CreatedWatch {
    /// 本客户端的 CreateConversation 受理了：事件已经到了就返回要打开的对话。
    pub fn accepted(&mut self, first: u64, last: u64) -> Option<String> {
        if let Some(i) = self
            .recent
            .iter()
            .position(|(seq, _)| (first..=last).contains(seq))
        {
            return self.recent.remove(i).map(|(_, id)| id);
        }
        self.pending.push((first, last));
        None
    }

    /// 见到一个 ConversationCreated：落在等着的受理范围里就返回要打开的对话。
    pub fn created(&mut self, seq: u64, conversation_id: &str) -> Option<String> {
        if let Some(i) = self
            .pending
            .iter()
            .position(|(a, b)| (*a..=*b).contains(&seq))
        {
            self.pending.remove(i);
            return Some(conversation_id.to_owned());
        }
        if self.recent.len() == RECENT_CAP {
            self.recent.pop_front();
        }
        self.recent.push_back((seq, conversation_id.to_owned()));
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn event_after_accept_navigates() {
        let mut w = CreatedWatch::default();
        assert_eq!(w.accepted(10, 11), None);
        assert_eq!(w.created(9, "别人的"), None, "范围外的不跳");
        assert_eq!(w.created(11, "c-new").as_deref(), Some("c-new"));
        assert_eq!(w.created(11, "c-new"), None, "同一个范围只跳一次");
    }

    #[test]
    fn accept_after_event_navigates() {
        let mut w = CreatedWatch::default();
        assert_eq!(w.created(20, "c-new"), None);
        assert_eq!(w.accepted(20, 20).as_deref(), Some("c-new"));
        assert_eq!(w.accepted(20, 20), None, "已经配过的不再配");
    }

    #[test]
    fn other_clients_creations_do_not_navigate_and_stay_bounded() {
        let mut w = CreatedWatch::default();
        for seq in 0..100 {
            assert_eq!(w.created(seq, "x"), None);
        }
        assert_eq!(w.recent.len(), RECENT_CAP);
        assert_eq!(w.accepted(1, 50), None, "早已挤出去的不配");
    }
}
