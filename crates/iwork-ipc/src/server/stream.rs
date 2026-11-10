//! 每条连接一条按 seq 排好的事件流（方案 2.8）。
//!
//! 一条连接只有**一个游标**：订阅范围只是过滤条件（全部范围的键取并集）。三条保证——对整条流说：
//! 1. 不漏：匹配过滤的、`seq > 起点` 的持久事件都送出过一次（起点 = 快照 seq 或续传的 `from_seq`）；
//! 2. 不重；3. 严格递增。
//!
//! 送出持久事件只有一个入口 [`Stream::deliver`]，它只比游标：`seq ≤ cursor` 不发。补发、排空缓冲、
//! 实时三处都走它。纯逻辑，不碰 IO；预算放不下时返回 [`FailCause`]，由连接层关连接。

use std::collections::{BTreeMap, VecDeque};
use std::sync::Arc;

use iwork_proto::pb;
use iwork_proto::routing::StreamKey;

use super::budget::{ConnectionBudget, Outbound, Push};
use super::handler::RoutedEvent;

/// 连接被关的原因。`fail_connection` 是唯一的关闭入口，它把原因写进审计（方案 2.9 第 3 条）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FailCause {
    /// 实时事件合并后仍放不下。
    QueueFull,
    /// 补发或交接期间，实时事件进缓冲放不下。
    ReplayBufferFull,
    /// 补发等预算超过 `replay_reserve_timeout`。
    ReplayReserveTimeout,
    /// 交接整体超时（含取数不返回、补发一页不返回）。
    HandoverTimeout,
    /// 读事务看到的 seq 一直没有被发布。
    PublishStalled,
    /// 事件源（存储）报错。
    SourceFailed,
    /// 协议违规：方向不对、未知类型、超长、解不出、`request_id` 0、`Chunk` 超限、范围过多。
    ProtocolViolation,
    /// handler 拒绝分片。
    ChunkRefused,
    /// 写 socket 失败：对端已关闭。
    PeerClosed,
}

impl FailCause {
    pub fn as_str(self) -> &'static str {
        match self {
            FailCause::QueueFull => "QueueFull",
            FailCause::ReplayBufferFull => "ReplayBufferFull",
            FailCause::ReplayReserveTimeout => "ReplayReserveTimeout",
            FailCause::HandoverTimeout => "HandoverTimeout",
            FailCause::PublishStalled => "PublishStalled",
            FailCause::SourceFailed => "SourceFailed",
            FailCause::ProtocolViolation => "ProtocolViolation",
            FailCause::ChunkRefused => "ChunkRefused",
            FailCause::PeerClosed => "PeerClosed",
        }
    }

    /// 后台自身有问题（不是客户端慢或违规）：审计另记 `error`，给 T04 的监督器。
    pub fn is_server_fault(self) -> bool {
        matches!(
            self,
            FailCause::HandoverTimeout | FailCause::PublishStalled | FailCause::SourceFailed
        )
    }
}

#[derive(Debug)]
enum Phase {
    /// 没有任何范围：实时事件都忽略。
    Empty,
    /// 交接或补发中：匹配过滤的实时事件进缓冲（字节算预算）。
    Paused {
        buffer: VecDeque<Buffered>,
    },
    Live,
}

/// 缓冲里的一条：已经做成完整的待发帧（带订阅 id）。预留预算与转入发送队列用的是同一个
/// 完整编码长度 `len`，转移时字节数不变。订阅 id 在暂停期间不会变：范围只由 FIFO 里的操作改，
/// 而暂停本身就是一个 FIFO 操作在执行。
#[derive(Debug)]
struct Buffered {
    seq: u64,
    frame: Outbound,
    len: usize,
}

#[derive(Debug)]
struct Scope {
    scope: pb::SubscriptionScope,
    keys: Vec<StreamKey>,
}

/// 一次范围登记（`Subscribe` 合并后的一项）。
#[derive(Debug, Clone)]
pub(crate) struct ScopeAdd {
    pub id: u32,
    pub scope: pb::SubscriptionScope,
    pub keys: Vec<StreamKey>,
}

#[derive(Debug)]
pub(crate) struct Stream {
    scopes: BTreeMap<u32, Scope>,
    filter: Vec<StreamKey>,
    cursor: u64,
    phase: Phase,
}

impl Default for Stream {
    fn default() -> Self {
        Self::new()
    }
}

impl Stream {
    pub(crate) fn new() -> Self {
        Self {
            scopes: BTreeMap::new(),
            filter: Vec::new(),
            cursor: 0,
            phase: Phase::Empty,
        }
    }

    pub(crate) fn cursor(&self) -> u64 {
        self.cursor
    }

    pub(crate) fn filter(&self) -> &[StreamKey] {
        &self.filter
    }

    pub(crate) fn scope_count(&self) -> usize {
        self.scopes.len()
    }

    pub(crate) fn active_scopes(&self) -> Vec<pb::SubscriptionScope> {
        self.scopes.values().map(|s| s.scope.clone()).collect()
    }

    /// 匹配这条事件的全部活动范围 id（`Event.subscription_ids`）。
    fn matching_ids(&self, routes: &[StreamKey]) -> Vec<u32> {
        self.scopes
            .iter()
            .filter(|(_, s)| s.keys.iter().any(|k| routes.iter().any(|r| k.covers(r))))
            .map(|(id, _)| *id)
            .collect()
    }

    fn rebuild_filter(&mut self) {
        let mut keys: Vec<StreamKey> = self.scopes.values().flat_map(|s| s.keys.clone()).collect();
        keys.sort();
        keys.dedup();
        self.filter = keys;
    }

    /// 登记范围（同 id 替换），重算过滤，并把流暂停为缓冲。返回续传判定要比对的游标。
    ///
    /// 空流（没有范围、游标为 0）上第一次登记是重连：这时可续传的起点就是客户端声明的 `from_seq`。
    /// 这里**只返回**这个起点，不改游标：要等确定走续传才用 [`Stream::adopt_cursor`] 采纳它；
    /// 走快照时游标由快照的 seq 决定，否则一个超前的 `from_seq` 会让快照之后的事件被当成「已送出」吞掉。
    pub(crate) fn add_scopes(&mut self, adds: Vec<ScopeAdd>, from_seq: u64) -> u64 {
        let resume_base = if self.scopes.is_empty() && self.cursor == 0 {
            from_seq
        } else {
            self.cursor
        };
        for a in adds {
            self.scopes.insert(
                a.id,
                Scope {
                    scope: a.scope,
                    keys: a.keys,
                },
            );
        }
        self.rebuild_filter();
        self.pause();
        resume_base
    }

    /// 确定续传后采纳客户端的起点。只会把游标往前推（空流上从 0 推到 `from_seq`），不回退。
    pub(crate) fn adopt_cursor(&mut self, from_seq: u64) {
        self.cursor = self.cursor.max(from_seq);
    }

    /// 移除范围，只缩过滤，游标与状态不动；空了转 Empty（游标保留，之后可续传）。
    /// 返回范围是否存在。只在没有交接进行时调用（FIFO 保证）。
    pub(crate) fn remove_scope(&mut self, id: u32) -> bool {
        let existed = self.scopes.remove(&id).is_some();
        self.rebuild_filter();
        if self.scopes.is_empty() {
            self.phase = Phase::Empty;
        }
        existed
    }

    /// 暂停：此后匹配过滤的实时事件进缓冲。FIFO 保证进来时不在暂停中。
    pub(crate) fn pause(&mut self) {
        debug_assert!(
            !matches!(self.phase, Phase::Paused { .. }),
            "交接一次只做一个"
        );
        self.phase = Phase::Paused {
            buffer: VecDeque::new(),
        };
    }

    /// 持久事件的待发帧：带上匹配的全部活动范围 id。没有范围匹配（补发页里不属于当前范围的事件）返回 `None`。
    fn persisted_frame(&self, e: &Arc<RoutedEvent>) -> Option<Outbound> {
        let ids = self.matching_ids(&e.routes);
        (!ids.is_empty()).then(|| Outbound::Persisted {
            ids,
            event: Arc::clone(e),
        })
    }

    /// 唯一的持久事件发送入口。`seq ≤ cursor` 不发；否则交给 `push` 入队，成功就推进游标。
    fn deliver(&mut self, seq: u64, frame: Outbound, push: impl FnOnce(Outbound) -> Push) -> Push {
        if seq <= self.cursor {
            return Push::Queued;
        }
        let r = push(frame);
        if r == Push::Queued {
            self.cursor = seq;
        }
        r
    }

    /// 实时持久事件。返回是否有东西入队（需要唤醒写任务）。
    pub(crate) fn on_persisted(
        &mut self,
        e: &Arc<RoutedEvent>,
        budget: &mut ConnectionBudget,
    ) -> Result<bool, FailCause> {
        let Some(frame) = self.persisted_frame(e) else {
            return Ok(false);
        };
        match &mut self.phase {
            Phase::Empty => Ok(false),
            Phase::Paused { buffer } => {
                let len = frame.encoded_len();
                match budget.reserve_buffer(len) {
                    Push::Queued => {
                        buffer.push_back(Buffered {
                            seq: e.seq(),
                            frame,
                            len,
                        });
                        Ok(false)
                    }
                    Push::Overflow => Err(FailCause::ReplayBufferFull),
                }
            }
            Phase::Live => match self.deliver(e.seq(), frame, |o| budget.push_live(o)) {
                Push::Queued => Ok(true),
                Push::Overflow => Err(FailCause::QueueFull),
            },
        }
    }

    /// 实时瞬时事件：没有 seq、不进游标、不进缓冲，只在 Live 时投给匹配的范围。
    pub(crate) fn on_transient(
        &mut self,
        event: &pb::TransientEvent,
        routes: &[StreamKey],
        budget: &mut ConnectionBudget,
    ) -> Result<bool, FailCause> {
        if !matches!(self.phase, Phase::Live) {
            return Ok(false);
        }
        let ids = self.matching_ids(routes);
        if ids.is_empty() {
            return Ok(false);
        }
        match budget.push_live(Outbound::Transient {
            ids,
            event: event.clone(),
        }) {
            Push::Queued => Ok(true),
            Push::Overflow => Err(FailCause::QueueFull),
        }
    }

    /// 补发一条历史事件。`Overflow` 时什么都没变，调用方等预算后重试。
    pub(crate) fn deliver_replay(
        &mut self,
        e: &Arc<RoutedEvent>,
        budget: &mut ConnectionBudget,
    ) -> Push {
        match self.persisted_frame(e) {
            Some(frame) => self.deliver(e.seq(), frame, |o| budget.push_replay(o)),
            None => Push::Queued,
        }
    }

    /// 交接的插入：先送缓冲里 `seq ≤ at` 的事件，再把 `responses` 入队，游标推到 `at`，再送其余缓冲，恢复。
    /// 结果因此正好落在流里 seq = `at` 的位置。
    pub(crate) fn insert_at(
        &mut self,
        at: u64,
        responses: Vec<pb::Response>,
        budget: &mut ConnectionBudget,
    ) {
        let mut buffer = self.take_buffer();
        while buffer.front().is_some_and(|b| b.seq <= at) {
            let b = buffer.pop_front().expect("刚看过非空");
            self.move_from_buffer(b, budget);
        }
        for r in responses {
            budget.push_response(r);
        }
        self.cursor = self.cursor.max(at);
        for b in buffer {
            self.move_from_buffer(b, budget);
        }
        self.resume();
    }

    /// 补发读完 `(cursor, to]`：游标至少到 `to`，排空缓冲，恢复。
    pub(crate) fn finish_replay(&mut self, to: u64, budget: &mut ConnectionBudget) {
        self.cursor = self.cursor.max(to);
        self.drain_and_resume(budget);
    }

    /// 交接以业务错误结束：缓冲按序全部送出，游标只随送出的事件前进，恢复。
    pub(crate) fn drain_and_resume(&mut self, budget: &mut ConnectionBudget) {
        for b in self.take_buffer() {
            self.move_from_buffer(b, budget);
        }
        self.resume();
    }

    fn take_buffer(&mut self) -> VecDeque<Buffered> {
        match std::mem::replace(&mut self.phase, Phase::Empty) {
            Phase::Paused { buffer } => buffer,
            other => {
                self.phase = other;
                VecDeque::new()
            }
        }
    }

    fn move_from_buffer(&mut self, b: Buffered, budget: &mut ConnectionBudget) {
        let mut moved = false;
        self.deliver(b.seq, b.frame, |o| {
            budget.move_buffered_to_queue(o, b.len);
            moved = true;
            Push::Queued
        });
        if !moved {
            budget.release_buffer(b.len);
        }
    }

    fn resume(&mut self) {
        self.phase = if self.scopes.is_empty() {
            Phase::Empty
        } else {
            Phase::Live
        };
    }

    /// 连接关闭：丢掉缓冲（字节由预算的 `clear` 一起归零）。
    pub(crate) fn clear(&mut self) {
        self.phase = Phase::Empty;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scope(i: u32) -> ScopeAdd {
        ScopeAdd {
            id: i,
            scope: pb::SubscriptionScope::default(),
            keys: vec![StreamKey::all("project")],
        }
    }

    fn event(seq: u64) -> Arc<RoutedEvent> {
        Arc::new(RoutedEvent::new(
            pb::PersistedEvent {
                seq,
                ..Default::default()
            },
            vec![StreamKey::one("project", "p")],
        ))
    }

    /// 256 个重叠范围让每条待发帧远大于裸事件：缓冲按完整帧预留，排空进队列后占用不变，都不越过上限。
    #[test]
    fn buffered_events_with_many_scope_ids_stay_within_budget() {
        let mut stream = Stream::new();
        stream.add_scopes((0..256).map(scope).collect(), 0);
        let mut budget = ConnectionBudget::new(64 * 1024, 1024);
        let mut buffered = 0;
        for seq in 1.. {
            match stream.on_persisted(&event(seq), &mut budget) {
                Ok(_) => buffered += 1,
                Err(cause) => {
                    assert_eq!(cause, FailCause::ReplayBufferFull);
                    break;
                }
            }
            assert!(
                budget.used() <= 64 * 1024 - 1024,
                "缓冲越过实时上限：{}",
                budget.used()
            );
        }
        assert!(buffered > 0, "对照：放得下时会缓冲");
        let before = budget.used();
        stream.finish_replay(0, &mut budget);
        assert_eq!(budget.used(), before, "转入发送队列前后占用不变");
    }
}
