//! 每连接预算（方案 2.9）：发送队列 + 补发缓冲共用一个上限（默认 8MiB），其中一小段只给补发用。
//!
//! 纯逻辑，不碰 IO 也不关连接：判定只回答「放得下 / 放不下」，放不下时由连接层走唯一的出口
//! `fail_connection`。持久事件在这里从不丢弃；只有瞬时增量会被合并。

use std::collections::VecDeque;
use std::sync::Arc;

use bytes::BytesMut;
use iwork_proto::limits::MAX_CHUNK_DATA;
use iwork_proto::pb::{self, FrameType, transient_event::Event as Transient};
use prost::Message;
use prost::encoding::{encoded_len_varint, key_len};

use super::handler::RoutedEvent;
use crate::frame::{HEADER_LEN, put_header};

/// 待发的一帧。持久事件持有 `Arc`，在写任务里直接编码，不为每条连接复制一份事件。
#[derive(Debug, Clone)]
pub(crate) enum Outbound {
    Response(pb::Response),
    Persisted {
        ids: Vec<u32>,
        event: Arc<RoutedEvent>,
    },
    Transient {
        ids: Vec<u32>,
        event: pb::TransientEvent,
    },
}

/// `Event` 消息里两个字段的编号（`frame.proto`）。手写编码只为避免克隆 `Arc` 里的事件；
/// 对拍测试 `persisted_encoding_matches_prost` 绑定它与 prost 生成的编码一致。
const EVENT_SUBSCRIPTION_IDS: u32 = 1;
const EVENT_PERSISTED: u32 = 10;

fn event_payload_len(ids: &[u32], persisted_len: usize) -> usize {
    prost::encoding::uint32::encoded_len_packed(EVENT_SUBSCRIPTION_IDS, ids)
        + key_len(EVENT_PERSISTED)
        + encoded_len_varint(persisted_len as u64)
        + persisted_len
}

impl Outbound {
    fn payload_len(&self) -> usize {
        match self {
            Outbound::Response(r) => r.encoded_len(),
            Outbound::Persisted { ids, event } => event_payload_len(ids, event.encoded_len()),
            Outbound::Transient { ids, event } => transient_frame(ids, event).encoded_len(),
        }
    }

    /// 编码后的总字节数（含帧头）。
    pub(crate) fn encoded_len(&self) -> usize {
        HEADER_LEN + self.payload_len()
    }

    pub(crate) fn encode(&self, dst: &mut BytesMut) {
        match self {
            Outbound::Response(r) => {
                put_header(dst, FrameType::Response, r.encoded_len());
                r.encode_raw(dst);
            }
            Outbound::Persisted { ids, event } => {
                put_header(dst, FrameType::Event, self.payload_len());
                prost::encoding::uint32::encode_packed(EVENT_SUBSCRIPTION_IDS, ids, dst);
                prost::encoding::message::encode(EVENT_PERSISTED, &event.event, dst);
            }
            Outbound::Transient { ids, event } => {
                let ev = transient_frame(ids, event);
                put_header(dst, FrameType::Event, ev.encoded_len());
                ev.encode_raw(dst);
            }
        }
    }
}

fn transient_frame(ids: &[u32], event: &pb::TransientEvent) -> pb::Event {
    pb::Event {
        subscription_ids: ids.to_vec(),
        body: Some(pb::event::Body::Transient(event.clone())),
    }
}

/// 预算判定的结果。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Push {
    Queued,
    /// 放不下。预算不动，调用方决定怎么办（实时 → 关连接；补发 → 等一会再试）。
    Overflow,
}

#[derive(Debug)]
pub(crate) struct ConnectionBudget {
    limit: usize,
    replay_reserve: usize,
    queue: VecDeque<(Outbound, usize)>,
    queue_bytes: usize,
    buffered_bytes: usize,
}

impl ConnectionBudget {
    pub(crate) fn new(limit: usize, replay_reserve: usize) -> Self {
        assert!(replay_reserve < limit, "补发保底必须小于连接预算");
        Self {
            limit,
            replay_reserve,
            queue: VecDeque::new(),
            queue_bytes: 0,
            buffered_bytes: 0,
        }
    }

    /// 队列 + 补发缓冲占用的字节。
    pub(crate) fn used(&self) -> usize {
        self.queue_bytes + self.buffered_bytes
    }

    pub(crate) fn limit(&self) -> usize {
        self.limit
    }

    pub(crate) fn queue_is_empty(&self) -> bool {
        self.queue.is_empty()
    }

    /// 实时事件与缓冲能用到的上限：留出补发保底。
    fn live_limit(&self) -> usize {
        self.limit - self.replay_reserve
    }

    /// 响应永远入队：它对应客户端自己发的请求，数量由读循环的暂停兜底。
    pub(crate) fn push_response(&mut self, r: pb::Response) {
        self.enqueue(Outbound::Response(r));
    }

    /// 补发往队列放事件：能用到整个上限（含保底）。
    pub(crate) fn push_replay(&mut self, o: Outbound) -> Push {
        let len = o.encoded_len();
        if self.used() + len > self.limit {
            return Push::Overflow;
        }
        self.enqueue_with_len(o, len);
        Push::Queued
    }

    /// 实时事件：放得下就入队；放不下且是瞬时增量，先尝试与队尾那段瞬时事件里同一条消息的增量合并。
    pub(crate) fn push_live(&mut self, o: Outbound) -> Push {
        let len = o.encoded_len();
        if self.used() + len <= self.live_limit() {
            self.enqueue_with_len(o, len);
            return Push::Queued;
        }
        if let Outbound::Transient { ids, event } = &o
            && self.try_merge(ids, event)
        {
            return Push::Queued;
        }
        Push::Overflow
    }

    /// 补发期间的实时事件进缓冲：缓冲字节与队列共用实时上限。
    pub(crate) fn reserve_buffer(&mut self, len: usize) -> Push {
        if self.used() + len > self.live_limit() {
            return Push::Overflow;
        }
        self.buffered_bytes += len;
        Push::Queued
    }

    /// 缓冲里的帧转入发送队列。缓冲预留时用的就是这一帧的完整编码长度，转移前后占用不变，所以不再判上限。
    pub(crate) fn move_buffered_to_queue(&mut self, o: Outbound, buffered_len: usize) {
        debug_assert_eq!(
            o.encoded_len(),
            buffered_len,
            "缓冲预留与发送队列必须用同一个完整编码长度"
        );
        self.buffered_bytes -= buffered_len;
        self.enqueue_with_len(o, buffered_len);
    }

    /// 缓冲里跳过不发的事件（已被快照或补发覆盖）。
    pub(crate) fn release_buffer(&mut self, len: usize) {
        self.buffered_bytes -= len;
    }

    /// 取出尽量多的帧编码进 `dst`，直到超过 `max_bytes`（至少一帧）。返回取了几帧。
    pub(crate) fn pop_batch(&mut self, max_bytes: usize, dst: &mut BytesMut) -> usize {
        let mut n = 0;
        while let Some((_, len)) = self.queue.front() {
            if n > 0 && dst.len() + len > max_bytes {
                break;
            }
            let (o, len) = self.queue.pop_front().expect("刚看过非空");
            self.queue_bytes -= len;
            o.encode(dst);
            n += 1;
        }
        n
    }

    /// 连接关闭时丢掉队列与缓冲。队列里的持久事件不会丢：它们在存储里，客户端重连后按游标续传。
    pub(crate) fn clear(&mut self) {
        self.queue.clear();
        self.queue_bytes = 0;
        self.buffered_bytes = 0;
    }

    fn enqueue(&mut self, o: Outbound) {
        let len = o.encoded_len();
        self.enqueue_with_len(o, len);
    }

    fn enqueue_with_len(&mut self, o: Outbound, len: usize) {
        self.queue_bytes += len;
        self.queue.push_back((o, len));
    }

    /// 从队尾往前，只在连续的瞬时事件里找同一条消息（或工具调用）的上一片增量，文字接得上就拼接。
    /// 碰到持久事件或响应就停：越过它们合并会改变增量与持久事件的先后。
    fn try_merge(&mut self, ids: &[u32], new: &pb::TransientEvent) -> bool {
        let Some(new_ev) = &new.event else {
            return false;
        };
        let room = self.live_limit().saturating_sub(self.used());
        for (o, len) in self.queue.iter_mut().rev() {
            let Outbound::Transient {
                ids: prev_ids,
                event: prev,
            } = o
            else {
                return false;
            };
            let same_stream = match (&prev.event, new_ev) {
                (Some(Transient::MessageDelta(p)), Transient::MessageDelta(n)) => {
                    p.message_id == n.message_id
                }
                (Some(Transient::ToolOutputDelta(p)), Transient::ToolOutputDelta(n)) => {
                    p.tool_call_id == n.tool_call_id
                }
                _ => false,
            };
            if !same_stream {
                continue;
            }
            // 找到同一条消息的上一片：订阅集合不同、文字接不上、拼完超长、合并后的净增长超出实时上限，
            // 都不合并，也不再往前找（交给调用方关连接）。合并在副本上做，确定放得下才替换。
            if prev_ids.as_slice() != ids {
                return false;
            }
            let mut merged = prev.clone();
            let appended = match (&mut merged.event, new_ev) {
                (Some(Transient::MessageDelta(p)), Transient::MessageDelta(n)) => {
                    append(&mut p.text, p.offset, &n.text, n.offset)
                }
                (Some(Transient::ToolOutputDelta(p)), Transient::ToolOutputDelta(n)) => {
                    append(&mut p.text, p.offset, &n.text, n.offset)
                }
                _ => unreachable!("same_stream 已确认两片同类"),
            };
            if !appended {
                return false;
            }
            let merged_frame = Outbound::Transient {
                ids: prev_ids.clone(),
                event: merged,
            };
            let new_len = merged_frame.encoded_len();
            if new_len - *len > room {
                return false;
            }
            self.queue_bytes = self.queue_bytes - *len + new_len;
            *o = merged_frame;
            *len = new_len;
            return true;
        }
        false
    }
}

/// 把 `next` 拼到 `text` 后面：要求 `next_offset == offset + text.len()`、拼完不超过 `MAX_CHUNK_DATA`。
fn append(text: &mut String, offset: u64, next: &str, next_offset: u64) -> bool {
    if next_offset != offset + text.len() as u64 || text.len() + next.len() > MAX_CHUNK_DATA {
        return false;
    }
    text.push_str(next);
    true
}

#[cfg(test)]
mod tests {
    use super::*;
    use iwork_proto::routing::StreamKey;

    fn persisted(seq: u64, pad: usize) -> Outbound {
        Outbound::Persisted {
            ids: vec![1],
            event: Arc::new(RoutedEvent::new(
                pb::PersistedEvent {
                    seq,
                    actor: "x".repeat(pad),
                    ..Default::default()
                },
                vec![StreamKey::all("project")],
            )),
        }
    }

    fn delta(msg: &str, offset: u64, text: &str, ids: &[u32]) -> Outbound {
        Outbound::Transient {
            ids: ids.to_vec(),
            event: pb::TransientEvent {
                event: Some(Transient::MessageDelta(pb::MessageDelta {
                    message_id: msg.into(),
                    offset,
                    text: text.into(),
                })),
            },
        }
    }

    fn texts(b: &ConnectionBudget) -> Vec<String> {
        b.queue
            .iter()
            .map(|(o, _)| match o {
                Outbound::Transient { event, .. } => match &event.event {
                    Some(Transient::MessageDelta(d)) => d.text.clone(),
                    _ => "?".into(),
                },
                Outbound::Persisted { event, .. } => format!("P{}", event.seq()),
                Outbound::Response(_) => "R".into(),
            })
            .collect()
    }

    #[test]
    fn persisted_encoding_matches_prost() {
        let o = persisted(7, 10);
        let mut mine = BytesMut::new();
        o.encode(&mut mine);
        let Outbound::Persisted { ids, event } = &o else {
            unreachable!()
        };
        let mut theirs = BytesMut::new();
        crate::frame::encode_frame(
            &crate::frame::Frame::Event(Box::new(pb::Event {
                subscription_ids: ids.clone(),
                body: Some(pb::event::Body::Persisted(event.event.clone())),
            })),
            &mut theirs,
        );
        assert_eq!(mine, theirs);
        assert_eq!(o.encoded_len(), theirs.len());
    }

    #[test]
    fn live_events_cannot_use_replay_reserve_but_replay_can() {
        let one = persisted(1, 100).encoded_len();
        // 上限 10 条、保底 3 条：实时最多 7 条，补发能用满 10 条。
        let mut b = ConnectionBudget::new(one * 10, one * 3);
        for i in 1..=7 {
            assert_eq!(b.push_live(persisted(i, 100)), Push::Queued);
        }
        assert_eq!(b.push_live(persisted(8, 100)), Push::Overflow);
        assert_eq!(b.used(), one * 7, "溢出时预算不动");
        for i in 0..3 {
            assert_eq!(b.push_replay(persisted(10 + i, 100)), Push::Queued);
        }
        assert_eq!(b.push_replay(persisted(20, 100)), Push::Overflow);
        assert_eq!(b.used(), b.limit());
    }

    #[test]
    fn responses_are_never_refused() {
        let mut b = ConnectionBudget::new(100, 10);
        for _ in 0..50 {
            b.push_response(pb::Response {
                request_id: 1,
                body: None,
            });
        }
        assert!(b.used() > b.limit());
        assert_eq!(b.push_live(persisted(1, 0)), Push::Overflow);
    }

    #[test]
    fn buffer_counts_against_live_limit_and_moves_without_recheck() {
        let one = persisted(1, 100).encoded_len();
        let mut b = ConnectionBudget::new(one * 4, one);
        assert_eq!(b.reserve_buffer(one), Push::Queued);
        assert_eq!(b.reserve_buffer(one), Push::Queued);
        assert_eq!(b.reserve_buffer(one), Push::Queued);
        assert_eq!(
            b.reserve_buffer(one),
            Push::Overflow,
            "缓冲也只能用到实时上限"
        );
        assert_eq!(b.used(), one * 3);
        b.move_buffered_to_queue(persisted(1, 100), one);
        b.release_buffer(one);
        assert_eq!(b.used(), one * 2);
        b.clear();
        assert_eq!(b.used(), 0);
    }

    #[test]
    fn adjacent_deltas_of_same_message_merge_when_full() {
        let first = delta("m", 0, "ab", &[1]);
        // 实时上限比第一片多 3 字节：第二片单独成帧放不下，合并只多 2 字节，放得下。
        let mut b = ConnectionBudget::new(first.encoded_len() + 4, 1);
        assert_eq!(b.push_live(first), Push::Queued);
        assert_eq!(
            b.push_live(delta("m", 2, "cd", &[1])),
            Push::Queued,
            "单独放不下就合并"
        );
        assert_eq!(texts(&b), ["abcd"]);
        assert_eq!(b.used(), b.queue[0].0.encoded_len(), "合并后字节重算");
        // offset 不连续、订阅集合不同、别的消息：都不合并 → 溢出。
        assert_eq!(b.push_live(delta("m", 9, "x", &[1])), Push::Overflow);
        assert_eq!(b.push_live(delta("m", 4, "x", &[1, 2])), Push::Overflow);
        assert_eq!(b.push_live(delta("other", 0, "x", &[1])), Push::Overflow);
        assert_eq!(texts(&b), ["abcd"]);
    }

    #[test]
    fn merge_skips_other_transients_but_not_persisted_events() {
        let mut b = ConnectionBudget::new(10_000, 1);
        b.push_live(delta("m", 0, "a", &[1]));
        b.push_live(delta("n", 0, "z", &[1]));
        // 塞满，逼合并。
        while b.push_live(persisted(1, 50)) == Push::Queued {}
        // 再用别的消息的增量把零头塞满（offset 不连续，彼此不合并）。
        let mut k = 0;
        while b.push_live(delta("f", k * 10, "q", &[9])) == Push::Queued {
            k += 1;
        }
        assert_eq!(
            b.push_live(delta("m", 1, "b", &[1])),
            Push::Overflow,
            "中间隔着持久事件不合并"
        );
        let mut b = ConnectionBudget::new(200, 1);
        b.push_live(delta("m", 0, "a", &[1]));
        b.push_live(delta("n", 0, "z", &[1]));
        while b.push_live(delta("n", b.queue.len() as u64 + 100, "q", &[2])) == Push::Queued {}
        let before = texts(&b);
        assert_eq!(before[0], "a");
        assert_eq!(
            b.push_live(delta("m", 1, "b", &[1])),
            Push::Queued,
            "隔着别的瞬时事件可以合并"
        );
        assert_eq!(texts(&b)[0], "ab");
    }

    #[test]
    fn merge_never_exceeds_live_limit() {
        // 实时上限刚好放下第一片：之后无论来多少片同一条消息的增量，合并都不能让占用越过上限。
        let first = delta("m", 0, "ab", &[1]);
        let live = first.encoded_len() + 8;
        let mut b = ConnectionBudget::new(live + 1, 1);
        assert_eq!(b.push_live(first), Push::Queued);
        let mut offset = 2;
        let mut merged = 0;
        loop {
            let r = b.push_live(delta("m", offset, "cdef", &[1]));
            assert!(b.used() <= live, "占用 {} 越过实时上限 {live}", b.used());
            if r == Push::Overflow {
                break;
            }
            merged += 1;
            offset += 4;
        }
        assert!(merged >= 1, "对照：放得下时仍会合并");
        assert_eq!(b.queue.len(), 1);
    }

    #[test]
    fn buffered_frame_moves_with_its_full_length() {
        // 256 个订阅 id 的帧远大于裸事件：预留按完整帧计，转入队列后占用不变、不越过上限。
        let ids: Vec<u32> = (0..256).collect();
        let frame = Outbound::Persisted {
            ids,
            event: Arc::new(RoutedEvent::new(
                pb::PersistedEvent {
                    seq: 1,
                    ..Default::default()
                },
                vec![StreamKey::all("project")],
            )),
        };
        let len = frame.encoded_len();
        let mut b = ConnectionBudget::new(len * 2, 1);
        assert_eq!(b.reserve_buffer(len), Push::Queued);
        assert_eq!(
            b.reserve_buffer(len),
            Push::Overflow,
            "第二帧完整长度放不下"
        );
        let before = b.used();
        b.move_buffered_to_queue(frame, len);
        assert_eq!(b.used(), before, "转移前后占用不变");
        assert!(b.used() <= b.limit());
    }

    #[test]
    fn merge_respects_chunk_limit() {
        let big = "x".repeat(MAX_CHUNK_DATA);
        let first = delta("m", 0, &big, &[1]);
        let mut b = ConnectionBudget::new(first.encoded_len() + 1, 1);
        assert_eq!(b.push_live(first), Push::Queued);
        assert_eq!(
            b.push_live(delta("m", MAX_CHUNK_DATA as u64, "y", &[1])),
            Push::Overflow
        );
    }

    #[test]
    fn pop_batch_takes_at_least_one_and_respects_size() {
        let mut b = ConnectionBudget::new(1 << 20, 1);
        for i in 1..=10 {
            b.push_live(persisted(i, 100));
        }
        let one = persisted(1, 100).encoded_len();
        let mut buf = BytesMut::new();
        assert_eq!(b.pop_batch(one * 3, &mut buf), 3);
        assert_eq!(buf.len(), one * 3);
        let mut buf = BytesMut::new();
        assert_eq!(b.pop_batch(1, &mut buf), 1, "上限小于一帧也至少取一帧");
        assert_eq!(b.used(), one * 6);
    }
}
