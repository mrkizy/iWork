//! 帧编解码（设计 3.3；方案 2.5）。
//!
//! 帧 = `u32 长度（小端，= 类型字节 + 载荷字节数）| u8 类型 | Protobuf 载荷`。
//!
//! 不用 `LengthDelimitedCodec`：要在读到长度字段的那一刻就拒绝超长帧（不等载荷到齐、不为它分配），
//! 并把类型字节与方向单独处理，自己写比包一层清楚。[`decode_frame`] 是纯函数，也是 fuzz 的入口。

use bytes::{Buf, BufMut, BytesMut};
use iwork_proto::limits::MAX_FRAME_LEN;
use iwork_proto::pb::{self, FrameType};
use prost::Message;

/// 帧头：4 字节长度 + 1 字节类型。
pub const HEADER_LEN: usize = 5;

/// 一帧。
#[derive(Debug, Clone, PartialEq)]
pub enum Frame {
    Hello(pb::Hello),
    Welcome(pb::Welcome),
    Request(pb::Request),
    Response(pb::Response),
    Event(pb::Event),
    Chunk(pb::Chunk),
}

impl Frame {
    pub fn frame_type(&self) -> FrameType {
        match self {
            Frame::Hello(_) => FrameType::Hello,
            Frame::Welcome(_) => FrameType::Welcome,
            Frame::Request(_) => FrameType::Request,
            Frame::Response(_) => FrameType::Response,
            Frame::Event(_) => FrameType::Event,
            Frame::Chunk(_) => FrameType::Chunk,
        }
    }

    fn payload_len(&self) -> usize {
        match self {
            Frame::Hello(m) => m.encoded_len(),
            Frame::Welcome(m) => m.encoded_len(),
            Frame::Request(m) => m.encoded_len(),
            Frame::Response(m) => m.encoded_len(),
            Frame::Event(m) => m.encoded_len(),
            Frame::Chunk(m) => m.encoded_len(),
        }
    }

    /// 编码后的总字节数（含帧头）。预算按它计。
    pub fn encoded_len(&self) -> usize {
        HEADER_LEN + self.payload_len()
    }
}

/// 帧的流向。每个方向只允许三种类型，方向不对就是协议违规。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Direction {
    /// 客户端 → 后台：Hello、Request、Chunk。
    ToServer,
    /// 后台 → 客户端：Welcome、Response、Event。
    ToClient,
}

impl Direction {
    fn allows(self, t: FrameType) -> bool {
        matches!(
            (self, t),
            (
                Direction::ToServer,
                FrameType::Hello | FrameType::Request | FrameType::Chunk
            ) | (
                Direction::ToClient,
                FrameType::Welcome | FrameType::Response | FrameType::Event
            )
        )
    }
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum FrameError {
    #[error("帧长 {len} 超过上限 {max}")]
    TooLarge { len: u32, max: u32 },
    #[error("帧长为 0，没有类型字节")]
    Empty,
    #[error("未知帧类型 {0}")]
    UnknownType(u8),
    #[error("{frame_type:?} 帧解不出：{message}")]
    Malformed {
        frame_type: FrameType,
        message: String,
    },
    #[error("{frame_type:?} 帧不该出现在这个方向")]
    Direction { frame_type: FrameType },
}

impl FrameError {
    /// 审计与 `MALFORMED_FRAME.details.frame_type` 用的类型名；长度类错误没有类型。
    pub fn frame_type_name(&self) -> &'static str {
        match self {
            FrameError::Malformed { frame_type, .. } | FrameError::Direction { frame_type } => {
                frame_type.as_str_name()
            }
            FrameError::TooLarge { .. } | FrameError::Empty | FrameError::UnknownType(_) => {
                "UNKNOWN"
            }
        }
    }
}

/// 从缓冲里切出一帧；不够一帧返回 `Ok(None)`，缓冲不动。
///
/// 读到长度字段就判上限，不等载荷到齐。出错后缓冲内容不再有意义，调用方应断开连接。
pub fn decode_frame(
    buf: &mut BytesMut,
    max_len: u32,
    direction: Direction,
) -> Result<Option<Frame>, FrameError> {
    if buf.len() < 4 {
        return Ok(None);
    }
    let len = u32::from_le_bytes([buf[0], buf[1], buf[2], buf[3]]);
    if len > max_len {
        return Err(FrameError::TooLarge { len, max: max_len });
    }
    if len == 0 {
        return Err(FrameError::Empty);
    }
    let total = 4 + len as usize;
    if buf.len() < total {
        buf.reserve(total - buf.len());
        return Ok(None);
    }
    let type_byte = buf[4];
    let frame_type = FrameType::try_from(i32::from(type_byte))
        .map_err(|_| FrameError::UnknownType(type_byte))?;
    if frame_type == FrameType::Unspecified {
        return Err(FrameError::UnknownType(type_byte));
    }
    if !direction.allows(frame_type) {
        return Err(FrameError::Direction { frame_type });
    }
    buf.advance(HEADER_LEN);
    let payload = buf.split_to(total - HEADER_LEN).freeze();
    let malformed = |e: prost::DecodeError| FrameError::Malformed {
        frame_type,
        message: e.to_string(),
    };
    Ok(Some(match frame_type {
        FrameType::Hello => Frame::Hello(pb::Hello::decode(payload).map_err(malformed)?),
        FrameType::Welcome => Frame::Welcome(pb::Welcome::decode(payload).map_err(malformed)?),
        FrameType::Request => Frame::Request(pb::Request::decode(payload).map_err(malformed)?),
        FrameType::Response => Frame::Response(pb::Response::decode(payload).map_err(malformed)?),
        FrameType::Event => Frame::Event(pb::Event::decode(payload).map_err(malformed)?),
        FrameType::Chunk => Frame::Chunk(pb::Chunk::decode(payload).map_err(malformed)?),
        FrameType::Unspecified => unreachable!("上面已拒绝"),
    }))
}

/// 把一帧编码追加到 `dst`。
pub fn encode_frame(frame: &Frame, dst: &mut BytesMut) {
    let payload_len = frame.payload_len();
    put_header(dst, frame.frame_type(), payload_len);
    // encode 只在缓冲空间不足时失败；上面已 reserve 足够空间。
    let res = match frame {
        Frame::Hello(m) => m.encode(dst),
        Frame::Welcome(m) => m.encode(dst),
        Frame::Request(m) => m.encode(dst),
        Frame::Response(m) => m.encode(dst),
        Frame::Event(m) => m.encode(dst),
        Frame::Chunk(m) => m.encode(dst),
    };
    res.expect("BytesMut 会自动扩容，编码不会因空间不足失败");
}

/// 写帧头并为载荷预留空间。服务端推送持久事件时直接在 `Arc` 上编码、不复制事件，也走这里。
pub(crate) fn put_header(dst: &mut BytesMut, frame_type: FrameType, payload_len: usize) {
    let len = u32::try_from(payload_len + 1).expect("载荷长度超过 u32");
    dst.reserve(HEADER_LEN + payload_len);
    dst.put_u32_le(len);
    dst.put_u8(frame_type as u8);
}

/// `tokio_util` 的编解码器：解码方向与长度上限可在握手前后切换。
#[derive(Debug, Clone)]
pub struct FrameCodec {
    direction: Direction,
    max_len: u32,
}

impl FrameCodec {
    /// `direction` 是本端**收到**的帧的方向。
    pub fn new(direction: Direction, max_len: u32) -> Self {
        Self { direction, max_len }
    }

    /// 握手完成后放宽到 [`MAX_FRAME_LEN`]。
    pub fn set_max_len(&mut self, max_len: u32) {
        self.max_len = max_len;
    }
}

impl Default for FrameCodec {
    fn default() -> Self {
        Self::new(Direction::ToServer, MAX_FRAME_LEN)
    }
}

/// 解码错误：帧错误或底层 IO 错误。
#[derive(Debug, thiserror::Error)]
pub enum CodecError {
    #[error(transparent)]
    Frame(#[from] FrameError),
    #[error(transparent)]
    Io(#[from] std::io::Error),
}

impl tokio_util::codec::Decoder for FrameCodec {
    type Item = Frame;
    type Error = CodecError;

    fn decode(&mut self, src: &mut BytesMut) -> Result<Option<Frame>, CodecError> {
        Ok(decode_frame(src, self.max_len, self.direction)?)
    }
}

impl tokio_util::codec::Encoder<Frame> for FrameCodec {
    type Error = CodecError;

    fn encode(&mut self, item: Frame, dst: &mut BytesMut) -> Result<(), CodecError> {
        encode_frame(&item, dst);
        Ok(())
    }
}

#[cfg(feature = "dev")]
mod display {
    //! 调试输出：帧按 JSON 一行打印（`ipc-tool`、`RUST_LOG=iwork_ipc=trace`）。线上始终二进制。

    use std::fmt;
    use std::sync::OnceLock;

    use prost::Message;
    use prost_reflect::{DescriptorPool, DynamicMessage};

    use super::Frame;

    fn pool() -> &'static DescriptorPool {
        static POOL: OnceLock<DescriptorPool> = OnceLock::new();
        POOL.get_or_init(|| {
            DescriptorPool::decode(iwork_proto::FILE_DESCRIPTOR_SET)
                .expect("iwork_proto::FILE_DESCRIPTOR_SET 是构建时生成的合法描述符")
        })
    }

    fn to_json(name: &str, bytes: Vec<u8>) -> String {
        let Some(desc) = pool().get_message_by_name(name) else {
            return format!("\"<描述符里没有 {name}>\"");
        };
        match DynamicMessage::decode(desc, bytes.as_slice()) {
            Ok(m) => serde_json::to_string(&m).unwrap_or_else(|e| format!("\"<{e}>\"")),
            Err(e) => format!("\"<{e}>\""),
        }
    }

    impl fmt::Display for Frame {
        fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
            let (name, bytes) = match self {
                Frame::Hello(m) => ("iwork.v1.Hello", m.encode_to_vec()),
                Frame::Welcome(m) => ("iwork.v1.Welcome", m.encode_to_vec()),
                Frame::Request(m) => ("iwork.v1.Request", m.encode_to_vec()),
                Frame::Response(m) => ("iwork.v1.Response", m.encode_to_vec()),
                Frame::Event(m) => ("iwork.v1.Event", m.encode_to_vec()),
                Frame::Chunk(m) => ("iwork.v1.Chunk", m.encode_to_vec()),
            };
            write!(
                f,
                "{{\"type\":\"{}\",\"body\":{}}}",
                self.frame_type().as_str_name(),
                to_json(name, bytes)
            )
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn raw(len: u32, type_byte: u8, payload: &[u8]) -> BytesMut {
        let mut b = BytesMut::new();
        b.put_u32_le(len);
        b.put_u8(type_byte);
        b.put_slice(payload);
        b
    }

    fn samples() -> Vec<Frame> {
        vec![
            Frame::Hello(pb::Hello {
                protocol_major: 1,
                protocol_minor: 2,
                client_version: "0.1.0".into(),
                ..Default::default()
            }),
            Frame::Welcome(pb::Welcome {
                session_id: "s".into(),
                head_seq: 9,
                ..Default::default()
            }),
            Frame::Request(pb::Request {
                request_id: 7,
                body: Some(pb::request::Body::Unsubscribe(pb::Unsubscribe {
                    subscription_id: 3,
                })),
            }),
            Frame::Response(pb::Response {
                request_id: 7,
                body: None,
            }),
            Frame::Event(pb::Event {
                subscription_ids: vec![1, 2],
                body: None,
            }),
            Frame::Chunk(pb::Chunk {
                upload_id: "u".into(),
                data: vec![1, 2, 3].into(),
                ..Default::default()
            }),
        ]
    }

    fn dir_of(f: &Frame) -> Direction {
        match f {
            Frame::Hello(_) | Frame::Request(_) | Frame::Chunk(_) => Direction::ToServer,
            _ => Direction::ToClient,
        }
    }

    #[test]
    fn six_types_round_trip_and_wrong_direction_is_rejected() {
        for frame in samples() {
            let mut buf = BytesMut::new();
            encode_frame(&frame, &mut buf);
            assert_eq!(buf.len(), frame.encoded_len());
            let d = dir_of(&frame);
            let mut copy = buf.clone();
            assert_eq!(
                decode_frame(&mut buf, MAX_FRAME_LEN, d).unwrap(),
                Some(frame.clone())
            );
            assert!(buf.is_empty());
            let other = if d == Direction::ToServer {
                Direction::ToClient
            } else {
                Direction::ToServer
            };
            assert_eq!(
                decode_frame(&mut copy, MAX_FRAME_LEN, other).unwrap_err(),
                FrameError::Direction {
                    frame_type: frame.frame_type()
                }
            );
        }
    }

    #[test]
    fn length_boundary_is_checked_before_payload_arrives() {
        // 只有帧头、没有载荷：4MiB 正好允许（等待更多字节），4MiB+1 立即拒绝。
        let mut at_limit = raw(MAX_FRAME_LEN, FrameType::Request as u8, &[]);
        assert_eq!(
            decode_frame(&mut at_limit, MAX_FRAME_LEN, Direction::ToServer),
            Ok(None)
        );
        let mut below = raw(MAX_FRAME_LEN - 1, FrameType::Request as u8, &[]);
        assert_eq!(
            decode_frame(&mut below, MAX_FRAME_LEN, Direction::ToServer),
            Ok(None)
        );
        let mut over = raw(MAX_FRAME_LEN + 1, FrameType::Request as u8, &[]);
        assert_eq!(
            decode_frame(&mut over, MAX_FRAME_LEN, Direction::ToServer),
            Err(FrameError::TooLarge {
                len: MAX_FRAME_LEN + 1,
                max: MAX_FRAME_LEN
            })
        );
        let mut huge = raw(u32::MAX, 1, &[]);
        assert!(matches!(
            decode_frame(&mut huge, MAX_FRAME_LEN, Direction::ToServer),
            Err(FrameError::TooLarge { .. })
        ));
    }

    #[test]
    fn a_full_4mib_frame_decodes() {
        let chunk = pb::Chunk {
            upload_id: "u".into(),
            data: vec![7; MAX_FRAME_LEN as usize - 64].into(),
            ..Default::default()
        };
        let frame = Frame::Chunk(chunk);
        let mut buf = BytesMut::new();
        encode_frame(&frame, &mut buf);
        assert!(buf.len() - 4 <= MAX_FRAME_LEN as usize);
        assert_eq!(
            decode_frame(&mut buf, MAX_FRAME_LEN, Direction::ToServer).unwrap(),
            Some(frame)
        );
    }

    #[test]
    fn unknown_type_empty_and_garbage_are_errors() {
        let mut unknown = raw(1, 99, &[]);
        assert_eq!(
            decode_frame(&mut unknown, MAX_FRAME_LEN, Direction::ToServer),
            Err(FrameError::UnknownType(99))
        );
        let mut zero_type = raw(1, 0, &[]);
        assert_eq!(
            decode_frame(&mut zero_type, MAX_FRAME_LEN, Direction::ToServer),
            Err(FrameError::UnknownType(0))
        );
        let mut empty = raw(0, 0, &[]);
        assert_eq!(
            decode_frame(&mut empty, MAX_FRAME_LEN, Direction::ToServer),
            Err(FrameError::Empty)
        );
        // 0xFF 开头是非法的 Protobuf 标签（字段号 0 之外的线类型 7）。
        let mut garbage = raw(4, FrameType::Request as u8, &[0xff, 0xff, 0xff]);
        assert!(matches!(
            decode_frame(&mut garbage, MAX_FRAME_LEN, Direction::ToServer),
            Err(FrameError::Malformed {
                frame_type: FrameType::Request,
                ..
            })
        ));
    }

    #[test]
    fn truncated_frame_waits_then_decodes_when_rest_arrives() {
        let frame = samples().remove(2);
        let mut full = BytesMut::new();
        encode_frame(&frame, &mut full);
        // 再追加第二帧，证明一次只切一帧。
        encode_frame(&frame, &mut full);
        let mut buf = BytesMut::new();
        let bytes = full.freeze();
        for (i, b) in bytes.iter().enumerate() {
            buf.put_u8(*b);
            let got = decode_frame(&mut buf, MAX_FRAME_LEN, Direction::ToServer).unwrap();
            let one = frame.encoded_len();
            if i + 1 == one || i + 1 == 2 * one {
                assert_eq!(got, Some(frame.clone()), "第 {i} 字节应恰好凑齐一帧");
            } else {
                assert_eq!(got, None, "第 {i} 字节还不够一帧");
            }
        }
    }

    #[test]
    fn hello_limit_applies_before_handshake() {
        let mut big = raw(
            iwork_proto::limits::MAX_HELLO_LEN + 1,
            FrameType::Hello as u8,
            &[],
        );
        assert!(matches!(
            decode_frame(
                &mut big,
                iwork_proto::limits::MAX_HELLO_LEN,
                Direction::ToServer
            ),
            Err(FrameError::TooLarge { .. })
        ));
    }

    #[cfg(feature = "dev")]
    #[test]
    fn dev_display_is_one_line_json() {
        let s = samples()[1].to_string();
        let v: serde_json::Value = serde_json::from_str(&s).unwrap();
        assert_eq!(v["type"], "FRAME_TYPE_WELCOME");
        assert_eq!(v["body"]["sessionId"], "s");
        assert!(!s.contains('\n'));
    }
}
