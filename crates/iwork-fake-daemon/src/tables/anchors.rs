//! ListAnchors（T09 方案 2.9、G10）：用户消息、等你处理的事、失败的回合按 created_seq 合在一起，
//! 以 around_seq 为中心前后各取若干条。真后台的同一个查询归 T10a；这里按同一份规则造数据。

use iwork_proto::pb;

use super::{Tables, pending_identity};
use crate::envelope_time;

/// 用户消息的摘录长度（按字符；真后台按字素簇，假后台的文本都是常见字，结果一样）。
const EXCERPT_CHARS: usize = 200;

impl Tables {
    pub(super) fn list_anchors(
        &self,
        q: &pb::ListAnchors,
    ) -> Result<pb::ListAnchorsResult, pb::Error> {
        let row = self.conversation_row(&q.conversation_id)?;
        let mut all: Vec<pb::Anchor> = Vec::new();
        for m in &row.messages {
            let user = matches!(
                m.author.as_ref().and_then(|a| a.kind.as_ref()),
                Some(pb::author::Kind::User(_))
            );
            if !user {
                continue;
            }
            let text = match m.body.as_ref().and_then(|b| b.content.as_ref()) {
                Some(pb::message_body::Content::InlineText(t)) => t.as_str(),
                _ => "",
            };
            all.push(anchor(
                m.created_seq,
                pb::anchor::Kind::UserMessage(pb::UserMessageAnchor {
                    message_id: m.id.clone(),
                    excerpt: text.chars().take(EXCERPT_CHARS).collect(),
                }),
            ));
        }
        for (seq, item) in self.pending_items(Some(&row.conversation.id)) {
            all.push(anchor(
                seq,
                pb::anchor::Kind::Waiting(pb::WaitingAnchor {
                    pending_item_id: pending_identity(&item).1,
                }),
            ));
        }
        for t in self.turns.values() {
            if t.conversation_id != row.conversation.id
                || t.state != i32::from(pb::TurnState::Failed)
            {
                continue;
            }
            let had_output = row.messages.iter().any(|m| {
                m.turn_id.as_deref() == Some(t.id.as_str())
                    && matches!(
                        m.body.as_ref().and_then(|b| b.content.as_ref()),
                        Some(pb::message_body::Content::InlineText(s)) if !s.is_empty()
                    )
            });
            let seq = if t.ended_seq != 0 {
                t.ended_seq
            } else {
                t.created_seq
            };
            all.push(anchor(
                seq,
                pb::anchor::Kind::FailedTurn(pb::FailedTurnAnchor {
                    turn_id: t.id.clone(),
                    reason: t.state_reason.clone().unwrap_or_default(),
                    had_output,
                }),
            ));
        }
        all.sort_by_key(|a| a.created_seq);
        let waiting_count = count(&all, |k| matches!(k, pb::anchor::Kind::Waiting(_)));
        let failed_count = count(&all, |k| matches!(k, pb::anchor::Kind::FailedTurn(_)));
        let pivot = if q.around_seq == 0 {
            all.len()
        } else {
            all.partition_point(|a| a.created_seq <= q.around_seq)
        };
        let start = pivot.saturating_sub(q.before as usize);
        let end = (pivot + q.after as usize).min(all.len());
        Ok(pb::ListAnchorsResult {
            has_before: start > 0,
            has_after: end < all.len(),
            anchors: all[start..end].to_vec(),
            waiting_count,
            failed_count,
        })
    }
}

fn anchor(seq: u64, kind: pb::anchor::Kind) -> pb::Anchor {
    pb::Anchor {
        created_seq: seq,
        at: Some(envelope_time(seq)),
        kind: Some(kind),
    }
}

fn count(all: &[pb::Anchor], f: impl Fn(&pb::anchor::Kind) -> bool) -> u32 {
    u32::try_from(
        all.iter()
            .filter(|a| a.kind.as_ref().is_some_and(&f))
            .count(),
    )
    .unwrap_or(u32::MAX)
}
