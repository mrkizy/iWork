//! 系统通知相关的两条协议边界（T10a 方案 2.10）：`ReportPresence` 进来时校验并转成领域值；`GetOpenTarget` 的答案。

use iwork_domain::id::ConversationId;
use iwork_ports::notify::{OpenRecord, Presence, Viewing};
use iwork_proto::pb;

use crate::convert::timestamp_to_proto;
use crate::views::invalid;

/// `ReportPresence` → 在看哪。「看着某个对话」与「看着待你处理页」不能同时成立；对话编号要合法。
pub fn presence_from_proto(p: &pb::ReportPresence) -> Result<Presence, pb::Error> {
    let viewing = match (&p.viewing_conversation_id, p.viewing_pending_list) {
        (Some(_), true) => {
            return Err(invalid(
                "report_presence",
                "viewing_conversation_id 与 viewing_pending_list 至多设一个",
            ));
        }
        (Some(id), false) => Viewing::Conversation(id.parse::<ConversationId>().map_err(|_| {
            invalid(
                "viewing_conversation_id",
                format!("不是合法的对话编号：{id}"),
            )
        })?),
        (None, true) => Viewing::PendingList,
        (None, false) => Viewing::Elsewhere,
    };
    Ok(Presence {
        window_focused: p.window_focused,
        viewing,
    })
}

/// `GetOpenTarget` 的答案。没点过通知时是 `token = 0` 的空结果（协议的约定）；`boot_id` 总是带上。
pub fn open_target_to_proto(boot_id: &str, r: Option<&OpenRecord>) -> pb::GetOpenTargetResult {
    let Some(r) = r else {
        return pb::GetOpenTargetResult {
            boot_id: boot_id.to_owned(),
            ..Default::default()
        };
    };
    pb::GetOpenTargetResult {
        token: r.token,
        pending_item_id: r.target.item.id_string(),
        conversation_id: r.target.conversation_id.map(|c| c.to_string()),
        at: Some(timestamp_to_proto(r.at)),
        boot_id: boot_id.to_owned(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use iwork_domain::common::Timestamp;
    use iwork_domain::id::{JobId, RawUlid};
    use iwork_ports::notify::{OpenTarget, PendingItemRef};

    fn report(conversation: Option<&str>, pending: bool) -> pb::ReportPresence {
        pb::ReportPresence {
            window_focused: true,
            viewing_conversation_id: conversation.map(str::to_owned),
            viewing_pending_list: pending,
        }
    }

    #[test]
    fn presence_from_proto_checks_shape() {
        let c = ConversationId::from_raw(RawUlid::from_u128(10));
        let ok = |r| presence_from_proto(&r).unwrap().viewing;
        assert_eq!(ok(report(None, false)), Viewing::Elsewhere);
        assert_eq!(ok(report(None, true)), Viewing::PendingList);
        assert_eq!(
            ok(report(Some(&c.to_string()), false)),
            Viewing::Conversation(c)
        );
        let both = presence_from_proto(&report(Some(&c.to_string()), true)).unwrap_err();
        assert_eq!(both.code, i32::from(pb::ErrorCode::InvalidArgument));
        let bad = presence_from_proto(&report(Some("x"), false)).unwrap_err();
        assert!(bad.message.contains("x"), "{bad:?}");
    }

    #[test]
    fn open_target_to_proto_fills_fields() {
        assert_eq!(
            open_target_to_proto("b1", None),
            pb::GetOpenTargetResult {
                boot_id: "b1".into(),
                ..Default::default()
            }
        );
        let draft = JobId::from_raw(RawUlid::from_u128(40));
        let r = OpenRecord {
            token: 3,
            target: OpenTarget {
                item: PendingItemRef::AgentsMdDraft(draft),
                conversation_id: None,
            },
            at: Timestamp(1_000),
        };
        let p = open_target_to_proto("b1", Some(&r));
        assert_eq!(
            (
                p.token,
                p.pending_item_id.as_str(),
                p.conversation_id,
                p.boot_id.as_str()
            ),
            (3, draft.to_string().as_str(), None, "b1")
        );
        assert!(p.at.is_some());
    }
}
