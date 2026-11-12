//! 侧栏与待处理的摘要表（T09 方案 2.2「摘要：失效后重查，不推演」）。
//!
//! 未读数、自动标题、最近对话的顺序、对话在不在跑、助手最近动态都是后台按自己的规则算的；客户端再算一遍就是
//! 第二份规则。所以这里只有两条规则：查询结果整份替换对应的表；持久事件命中侧栏或待处理订阅就把摘要标成失效，
//! 由 [`Refresh`] 节流重查，一轮结果全部应用完、而且这一轮在飞期间没有新事件命中，才清掉失效标记。

use std::time::{Duration, Instant};

use iwork_proto::pb;

/// 摘要的四张表。
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum SummaryTable {
    Projects,
    Conversations,
    Assistants,
    Pending,
    /// 「全部对话」列表页开着时这一轮多带的那一条（方案 2.2 第 2 条）；查询由会话层按列表页的条件拼。
    List,
    /// 打开的对话已经有锚点窗口时这一轮多带的那一条（方案 2.9：锚点失效后进摘要重查的同一轮）；
    /// 查询由会话层按当前窗口的中心拼。
    Anchors,
}

impl SummaryTable {
    pub const ALL: [SummaryTable; 4] = [
        SummaryTable::Projects,
        SummaryTable::Conversations,
        SummaryTable::Assistants,
        SummaryTable::Pending,
    ];

    /// 重查这张表的查询；`List` 的条件在会话层，这里返回 `None`。
    pub fn query(self) -> Option<pb::Query> {
        use pb::query::Kind as Q;
        let kind = match self {
            SummaryTable::Projects => Q::ListProjects(pb::ListProjects {}),
            SummaryTable::Conversations => Q::ListConversations(pb::ListConversations {
                project_id: None,
                title_contains: None,
                before: None,
                limit: 0,
            }),
            SummaryTable::Assistants => Q::ListAssistants(pb::ListAssistants {}),
            SummaryTable::Pending => Q::ListPendingItems(pb::ListPendingItems {}),
            SummaryTable::List | SummaryTable::Anchors => return None,
        };
        Some(pb::Query { kind: Some(kind) })
    }
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct Summary {
    pub projects: Vec<pb::Project>,
    pub conversations: Vec<pb::Conversation>,
    pub assistants: Vec<pb::Assistant>,
    pub activities: Vec<pb::AssistantActivity>,
    pub pending: Vec<pb::PendingItem>,
}

impl Summary {
    pub fn conversation(&self, id: &str) -> Option<&pb::Conversation> {
        self.conversations.iter().find(|c| c.id == id)
    }

    pub fn project(&self, id: &str) -> Option<&pb::Project> {
        self.projects.iter().find(|p| p.id == id)
    }

    pub fn assistant(&self, id: &str) -> Option<&pb::Assistant> {
        self.assistants.iter().find(|a| a.id == id)
    }

    /// 某个助手的私聊。
    pub fn dm_of(&self, assistant_id: &str) -> Option<&pb::Conversation> {
        self.conversations.iter().find(|c| {
            matches!(&c.kind, Some(pb::conversation::Kind::AssistantDm(dm)) if dm.assistant_id == assistant_id)
        })
    }

    pub fn resume_cards(&self) -> impl Iterator<Item = &pb::ResumeCard> + Clone {
        self.pending.iter().filter_map(|p| match &p.item {
            Some(pb::pending_item::Item::Resume(r)) => Some(r),
            _ => None,
        })
    }
}

/// 一轮重查：四条查询都应用完才算走完。
#[derive(Debug, Clone, PartialEq, Eq)]
struct Round {
    id: u64,
    outstanding: Vec<SummaryTable>,
    /// 这一轮在飞期间有新事件命中摘要：走完也不清标记。
    again: bool,
}

/// 摘要重查的节流与「一轮走完才算数」（方案 2.2 第 2–4 条）。纯状态机，时间由调用方传入。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Refresh {
    stale: bool,
    round: Option<Round>,
    last_sent: Option<Instant>,
    next_round: u64,
}

/// 两轮重查之间的最小间隔。
pub const REFRESH_INTERVAL: Duration = Duration::from_millis(250);

impl Default for Refresh {
    fn default() -> Self {
        Self::new()
    }
}

impl Refresh {
    pub fn new() -> Self {
        Self {
            stale: false,
            round: None,
            last_sent: None,
            next_round: 1,
        }
    }

    pub fn is_stale(&self) -> bool {
        self.stale
    }

    pub fn in_flight(&self) -> bool {
        self.round.is_some()
    }

    /// 持久事件命中侧栏或待处理订阅。
    pub fn invalidate(&mut self) {
        self.stale = true;
        if let Some(r) = self.round.as_mut() {
            r.again = true;
        }
    }

    /// 现在该不该发一轮：失效、没有在飞、距上次发出不少于节流间隔。该发就记下这一轮，返回轮次号与要发的表。
    /// 轮次号随查询用途一起记下，结果回来时交回 [`Refresh::result_applied`]：作废轮次的迟到结果不算数。
    pub fn due(&mut self, now: Instant, with_list: bool) -> Option<(u64, Vec<SummaryTable>)> {
        self.due_with(now, with_list, false)
    }

    /// 同 [`Refresh::due`]，`with_anchors` 时这一轮多带锚点窗口那一条。
    pub fn due_with(
        &mut self,
        now: Instant,
        with_list: bool,
        with_anchors: bool,
    ) -> Option<(u64, Vec<SummaryTable>)> {
        if !self.stale || self.round.is_some() {
            return None;
        }
        if self
            .last_sent
            .is_some_and(|t| now.saturating_duration_since(t) < REFRESH_INTERVAL)
        {
            return None;
        }
        self.last_sent = Some(now);
        let id = self.next_round;
        self.next_round += 1;
        let mut tables = SummaryTable::ALL.to_vec();
        if with_list {
            tables.push(SummaryTable::List);
        }
        if with_anchors {
            tables.push(SummaryTable::Anchors);
        }
        self.round = Some(Round {
            id,
            outstanding: tables.clone(),
            again: false,
        });
        Some((id, tables))
    }

    /// 下一次可以发的时刻（会话层据此设定时器）；不需要发时为 `None`。
    pub fn next_due_at(&self) -> Option<Instant> {
        if !self.stale || self.round.is_some() {
            return None;
        }
        Some(
            self.last_sent
                .map_or_else(Instant::now, |t| t + REFRESH_INTERVAL),
        )
    }

    /// 这一轮的一条结果应用完了。全部走完且期间没有新事件 → 清标记。
    pub fn result_applied(&mut self, round_id: u64, table: SummaryTable) {
        let Some(round) = self.round.as_mut().filter(|r| r.id == round_id) else {
            return;
        };
        round.outstanding.retain(|t| *t != table);
        if round.outstanding.is_empty() {
            let again = round.again;
            self.round = None;
            if !again {
                self.stale = false;
            }
        }
    }

    /// 连接关闭或这一轮里有查询失败：这一轮作废，标记本来就没清，之后整轮重发。
    pub fn abandon_round(&mut self) {
        self.round = None;
    }

    /// 快照一次替换了四张表：标记清掉，在飞的轮次（快照前发出的）作废。
    pub fn replaced_by_snapshot(&mut self) {
        self.round = None;
        self.stale = false;
    }

    /// 启动或重连时没有快照可依：要重查。
    pub fn force_stale(&mut self) {
        self.stale = true;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn t0() -> Instant {
        Instant::now()
    }

    #[test]
    fn refresh_is_throttled_and_never_starves() {
        let mut r = Refresh::new();
        let start = t0();
        assert_eq!(r.due(start, false), None, "没失效不发");
        r.invalidate();
        let (id1, tables) = r.due(start, false).unwrap();
        assert_eq!(tables.len(), 4);
        // 在飞期间又来事件：不发第二轮，记 again。
        r.invalidate();
        assert_eq!(r.due(start + Duration::from_secs(1), false), None);
        for t in SummaryTable::ALL {
            r.result_applied(id1, t);
        }
        assert!(r.is_stale(), "在飞期间有新事件，走完不清");
        // 节流：距上次发出不足 250ms 不发。
        assert_eq!(r.due(start + Duration::from_millis(100), false), None);
        let (id2, _) = r.due(start + Duration::from_millis(250), false).unwrap();
        // 上一轮的迟到结果不算进这一轮。
        r.result_applied(id1, SummaryTable::Projects);
        for t in SummaryTable::ALL.iter().skip(1) {
            r.result_applied(id2, *t);
        }
        assert!(r.is_stale(), "这一轮还差一张表");
        r.result_applied(id2, SummaryTable::Projects);
        assert!(!r.is_stale(), "完整一轮且期间无事件，清标记");
    }

    #[test]
    fn partial_round_then_close_keeps_stale() {
        for k in 0..4 {
            let mut r = Refresh::new();
            r.invalidate();
            let (id, _) = r.due(t0(), false).unwrap();
            for t in SummaryTable::ALL.iter().take(k) {
                r.result_applied(id, *t);
            }
            r.abandon_round();
            assert!(r.is_stale(), "应用完第 {k} 个就断线：标记还在");
            assert!(!r.in_flight());
        }
    }

    #[test]
    fn snapshot_clears_stale_and_round() {
        let mut r = Refresh::new();
        r.invalidate();
        let (id, _) = r.due(t0(), false).unwrap();
        r.replaced_by_snapshot();
        assert!(!r.is_stale());
        assert!(!r.in_flight());
        // 快照前发出的那一轮结果迟到：不影响。
        r.result_applied(id, SummaryTable::Projects);
        assert!(!r.is_stale());
    }
}
