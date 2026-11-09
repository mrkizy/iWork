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

    /// 缓冲里的事件转入发送队列：字节早已算过，转移不会改变占用，所以不再判上限。
    pub(crate) fn move_buffered_to_queue(&mut self, o: Outbound, buffered_len: usize) {
        self.buffered_bytes -= buffered_len;
        self.enqueue(o);
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
            // 找到同一条消息的上一片：订阅集合不同、文字接不上、拼完超长都不合并，也不再往前找。
            if prev_ids.as_slice() != ids {
                return false;
            }
            let appended = match (&mut prev.event, new_ev) {
                (Some(Transient::MessageDelta(p)), Transient::MessageDelta(n)) => {
                    append(&mut p.text, p.offset, &n.text, n.offset)
                }
                (Some(Transient::ToolOutputDelta(p)), Transient::ToolOutputDelta(n)) => {
                    append(&mut p.text, p.offset, &n.text, n.offset)
                }
                _ => unreachable!("same_stream 已确认两片同类"),
            };
            if appended {
                let new_len = o.encoded_len();
                self.queue_bytes = self.queue_bytes - *len + new_len;
                *len = new_len;
            }
            return appended;
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
