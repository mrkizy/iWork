//! `EventCodec` 端口的实现：domain 事件 ↔ proto `DomainEvent` 的二进制编码（方案 3.6、5.2）。
//! 存储层经它编解码，自己不依赖 proto；持久格式与推给客户端的事件是同一个 proto 消息。

use iwork_domain::event::DomainEvent;
use iwork_ports::store::{CodecError, EncodedEvent, EventCodec};
use iwork_proto::pb;
use prost::Message;

use crate::convert::{event_from_proto, event_to_proto};

/// 用 Protobuf 编码事件。无状态，可随意复制。
#[derive(Debug, Clone, Copy, Default)]
pub struct ProtoEventCodec;

impl EventCodec for ProtoEventCodec {
    fn protocol_major(&self) -> u32 {
        iwork_proto::PROTOCOL_MAJOR
    }

    fn encode(&self, event: &DomainEvent) -> Result<EncodedEvent, CodecError> {
        let proto = event_to_proto(event).map_err(|e| CodecError::Unencodable {
            kind: event.kind(),
            message: e.to_string(),
        })?;
        let payload = proto.encode_to_vec();
        // 原则：编码成功的值，自己一定能解回（convert 在编码边界拒绝装不下的值）。
        // 调试构建里每次核对一遍，违反即是 convert 的缺陷；发布构建不付这次解码的代价。
        debug_assert!(
            self.decode(event.kind(), &payload).is_ok(),
            "事件 {} 编得进去却解不回来",
            event.kind()
        );
        Ok(EncodedEvent {
            kind: event.kind(),
            payload,
        })
    }

    /// 解出的事件种类必须与 `kind`（`events.type` 列）一致，不一致说明行被写坏了。
    fn decode(&self, kind: &str, payload: &[u8]) -> Result<DomainEvent, CodecError> {
        let malformed = |field: &str, message: String| CodecError::Malformed {
            kind: kind.to_owned(),
            field: field.to_owned(),
            message,
        };
        let proto =
            pb::DomainEvent::decode(payload).map_err(|e| malformed("payload", e.to_string()))?;
        let event = event_from_proto(proto)
            .map_err(|e| malformed(&format!("{}.{}", e.kind(), e.field()), e.to_string()))?;
        if event.kind() != kind {
            return Err(malformed(
                "type",
                format!("载荷是 {}，与行上记的种类不符", event.kind()),
            ));
        }
        Ok(event)
    }
}
