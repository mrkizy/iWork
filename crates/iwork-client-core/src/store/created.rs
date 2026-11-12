//! 新建对话以后跳过去：用户在这个客户端里点了「新对话」，对话建好就打开它。
//!
//! 命令受理只带回 seq 范围（`CommandAccepted.first_seq..=last_seq`），对话 id 在 `ConversationCreated` 事件里。
//! 两者走的是不同的路（回包与事件流），谁先到都可能；所以两边各留一点记录，后到的那一方配上对就跳。
//! 别的客户端建的对话只进「最近建的」，没有配上的受理范围，不跳。
//! 每个受理范围记着建它是为了什么（[`Purpose`]）：侧栏「新对话」配上就跳；首页发首条消息配上后还要先发消息。
//!
//! 断线重连时事件可能不再逐条重放、而是给一份快照（快照里的对话不带建出它的 seq）：快照盖过了的受理范围
//! 再也等不到事件。两种先后都要收尾（审查 2a 2/F-6）：
//! - 受理先到、快照后到：范围在等着，[`CreatedWatch::covered_by_snapshot`] 把它交出来；
//! - 快照先到、受理后到（重连时先订阅、后重发在飞命令）：受理来时范围已经落在游标之内又配不上，
//!   [`CreatedWatch::accepted`] 当场回 [`Accepted::Missed`]，不再进等待表。
//!
//! 调用方按用途收尾：侧栏那次丢掉，首页那次报原因。

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

/// 一次受理的结果。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Accepted {
    /// 事件已经到了，配上了。
    Matched(String, Purpose),
    /// 事件还没到，等着。
    Waiting,
    /// 范围已在游标之内却配不上：事件是随快照来的（或早已挤出「最近建的」），再也配不上了。
    Missed(Purpose),
}

#[derive(Debug, Default)]
pub struct CreatedWatch {
    /// 受理了、还没见到对应事件的 seq 范围。
    pending: Vec<(u64, u64, Purpose)>,
    /// 见到了、还没配上受理回包的 (seq, 对话 id)。
    recent: VecDeque<(u64, String)>,
}

impl CreatedWatch {
    /// 本客户端的 CreateConversation 受理了。`cursor` 是已经应用到的 seq（含快照）。
    pub fn accepted(&mut self, first: u64, last: u64, purpose: Purpose, cursor: u64) -> Accepted {
        if let Some(i) = self
            .recent
            .iter()
            .position(|(seq, _)| (first..=last).contains(seq))
            && let Some((_, id)) = self.recent.remove(i)
        {
            return Accepted::Matched(id, purpose);
        }
        if last <= cursor {
            return Accepted::Missed(purpose);
        }
        self.pending.push((first, last, purpose));
        Accepted::Waiting
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
        assert_eq!(w.accepted(10, 11, Purpose::Open, 9), Accepted::Waiting);
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
            w.accepted(20, 20, Purpose::Start, 20),
            Accepted::Matched("c-new".into(), Purpose::Start)
        );
        assert_eq!(
            w.accepted(20, 20, Purpose::Start, 20),
            Accepted::Missed(Purpose::Start),
            "已经配过的不再配"
        );
    }

    #[test]
    fn other_clients_creations_do_not_navigate_and_stay_bounded() {
        let mut w = CreatedWatch::default();
        for seq in 0..100 {
            assert_eq!(w.created(seq, "x"), None);
        }
        assert_eq!(w.recent.len(), RECENT_CAP);
        assert_eq!(
            w.accepted(1, 50, Purpose::Open, 99),
            Accepted::Missed(Purpose::Open),
            "早已挤出去的不配，也不留在等待表里"
        );
        assert!(w.pending.is_empty());
    }

    #[test]
    fn snapshot_hands_back_ranges_it_covers() {
        let mut w = CreatedWatch::default();
        assert_eq!(w.accepted(5, 6, Purpose::Start, 4), Accepted::Waiting);
        assert_eq!(w.accepted(9, 9, Purpose::Open, 4), Accepted::Waiting);
        assert_eq!(w.covered_by_snapshot(7), vec![Purpose::Start]);
        assert_eq!(w.covered_by_snapshot(7), vec![], "交过的不再交");
        assert_eq!(
            w.created(9, "c-9"),
            Some(("c-9".into(), Purpose::Open)),
            "快照之后的照常配"
        );
    }

    /// 快照先到、受理后到（重连时先订阅、后重发在飞命令）：受理来时就判配不上，不留在等待表里。
    #[test]
    fn accept_after_covering_snapshot_is_missed_at_once() {
        let mut w = CreatedWatch::default();
        assert_eq!(w.covered_by_snapshot(10), vec![]);
        assert_eq!(
            w.accepted(5, 6, Purpose::Start, 10),
            Accepted::Missed(Purpose::Start)
        );
        assert!(w.pending.is_empty());
        assert_eq!(
            w.accepted(11, 12, Purpose::Start, 10),
            Accepted::Waiting,
            "对照：范围在游标之后的照常等"
        );
    }
}
