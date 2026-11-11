//! 新建对话以后跳过去：用户在这个客户端里点了「新对话」，对话建好就打开它。
//!
//! 命令受理只带回 seq 范围（`CommandAccepted.first_seq..=last_seq`），对话 id 在 `ConversationCreated` 事件里。
//! 两者走的是不同的路（回包与事件流），谁先到都可能；所以两边各留一点记录，后到的那一方配上对就跳。
//! 别的客户端建的对话只进「最近建的」，没有配上的受理范围，不跳。
//! 每个受理范围记着建它是为了什么（[`Purpose`]）：侧栏「新对话」配上就跳；首页发首条消息配上后还要先发消息。
//!
//! 断线重连时事件可能不再逐条重放、而是给一份快照（快照里的对话不带建出它的 seq）：快照盖过了的受理范围
//! 再也等不到事件，由 [`CreatedWatch::covered_by_snapshot`] 交出来，调用方按用途收尾。

use std::collections::VecDeque;

/// 「最近建的对话」留多少条：只用来等稍后才到的受理回包，不需要多。
const RECENT_CAP: usize = 16;

/// 建这个对话是为了什么。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Purpose {
    /// 侧栏「新对话」：建好就打开。
    Open,
    /// 首页发首条消息：建好先设上选择、发出消息，再打开（store::home）。
    Start,
}

#[derive(Debug, Default)]
pub struct CreatedWatch {
    /// 受理了、还没见到对应事件的 seq 范围。
    pending: Vec<(u64, u64, Purpose)>,
    /// 见到了、还没配上受理回包的 (seq, 对话 id)。
    recent: VecDeque<(u64, String)>,
}

impl CreatedWatch {
    /// 本客户端的 CreateConversation 受理了：事件已经到了就返回要打开的对话。
    pub fn accepted(
        &mut self,
        first: u64,
        last: u64,
        purpose: Purpose,
    ) -> Option<(String, Purpose)> {
        if let Some(i) = self
            .recent
            .iter()
            .position(|(seq, _)| (first..=last).contains(seq))
        {
            return self.recent.remove(i).map(|(_, id)| (id, purpose));
        }
        self.pending.push((first, last, purpose));
        None
    }

    /// 见到一个 ConversationCreated：落在等着的受理范围里就返回要打开的对话。
    pub fn created(&mut self, seq: u64, conversation_id: &str) -> Option<(String, Purpose)> {
        if let Some(i) = self
            .pending
            .iter()
            .position(|(a, b, _)| (*a..=*b).contains(&seq))
        {
            let (_, _, purpose) = self.pending.remove(i);
            return Some((conversation_id.to_owned(), purpose));
        }
        if self.recent.len() == RECENT_CAP {
            self.recent.pop_front();
        }
        self.recent.push_back((seq, conversation_id.to_owned()));
        None
    }

    /// 来了一份截至 `snapshot_seq` 的快照：范围落在它之内、还没配上的，事件不会再来了，交出它们的用途。
    pub fn covered_by_snapshot(&mut self, snapshot_seq: u64) -> Vec<Purpose> {
        let (gone, keep) = std::mem::take(&mut self.pending)
            .into_iter()
            .partition(|(_, last, _)| *last <= snapshot_seq);
        self.pending = keep;
        gone.into_iter().map(|(_, _, p)| p).collect::<Vec<_>>()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn event_after_accept_navigates() {
        let mut w = CreatedWatch::default();
        assert_eq!(w.accepted(10, 11, Purpose::Open), None);
        assert_eq!(w.created(9, "别人的"), None, "范围外的不跳");
        assert_eq!(
            w.created(11, "c-new"),
            Some(("c-new".into(), Purpose::Open))
        );
        assert_eq!(w.created(11, "c-new"), None, "同一个范围只跳一次");
    }

    #[test]
    fn accept_after_event_navigates() {
        let mut w = CreatedWatch::default();
        assert_eq!(w.created(20, "c-new"), None);
        assert_eq!(
            w.accepted(20, 20, Purpose::Start),
            Some(("c-new".into(), Purpose::Start))
        );
        assert_eq!(w.accepted(20, 20, Purpose::Start), None, "已经配过的不再配");
    }

    #[test]
    fn other_clients_creations_do_not_navigate_and_stay_bounded() {
        let mut w = CreatedWatch::default();
        for seq in 0..100 {
            assert_eq!(w.created(seq, "x"), None);
        }
        assert_eq!(w.recent.len(), RECENT_CAP);
        assert_eq!(w.accepted(1, 50, Purpose::Open), None, "早已挤出去的不配");
    }

    #[test]
    fn snapshot_hands_back_ranges_it_covers() {
        let mut w = CreatedWatch::default();
        assert_eq!(w.accepted(5, 6, Purpose::Start), None);
        assert_eq!(w.accepted(9, 9, Purpose::Open), None);
        assert_eq!(w.covered_by_snapshot(7), vec![Purpose::Start]);
        assert_eq!(w.covered_by_snapshot(7), vec![], "交过的不再交");
        assert_eq!(
            w.created(9, "c-9"),
            Some(("c-9".into(), Purpose::Open)),
            "快照之后的照常配"
        );
    }
}
