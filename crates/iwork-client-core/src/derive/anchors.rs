//! 消息锚点的视图模型（T09 方案 2.9）：类型文字、时间文字、预览文字都在这里出，网页只画。

use std::time::SystemTime;

use iwork_proto::pb;

use super::{Ctx, pending, text, to_system_time};
use crate::store::Store;
use crate::store::anchors::{Edge, HIGHLIGHT_MS, key_of};
use crate::store::timeline::Timeline;

/// 锚点列表；还没取到时为 `None`（组件不显示）。
pub fn anchors(store: &Store, tl: &Timeline, ctx: Ctx) -> Option<pb::AnchorsView> {
    let w = tl.anchors.window.as_ref()?;
    let tz = jiff::tz::TimeZone::system();
    Some(pb::AnchorsView {
        items: w
            .anchors
            .iter()
            .map(|a| item(store, a, ctx.now, &tz))
            .collect(),
        has_before: w.has_before,
        has_after: w.has_after,
        has_waiting: w.waiting_count > 0,
        has_failed: w.failed_count > 0,
        // Q18：跳到没加载的地方时原地不动；被点的刻度流光、顶端进度线，读屏读一句
        jump_loading: tl
            .anchors
            .pending_jump
            .as_ref()
            .map(|_| text::ANCHOR_JUMP_LOADING.to_owned()),
        jump_loading_key: tl
            .anchors
            .pending_jump
            .as_ref()
            .map(|j| key_of(&j.anchor))
            .unwrap_or_default(),
        edge_landing: match tl.anchors.edge {
            Some(Edge::Landed { latest, token }) => Some(pb::EdgeLanding { token, latest }),
            _ => None,
        },
        edge_pending: matches!(tl.anchors.edge, Some(Edge::Waiting { .. })),
    })
}

pub fn cursor(tl: &Timeline) -> pb::AnchorCursor {
    pb::AnchorCursor {
        current_key: tl.anchors.current.clone().unwrap_or_default(),
    }
}

pub fn scroll_to(tl: &Timeline) -> Option<pb::ScrollTo> {
    tl.anchors.scroll.as_ref().map(|s| pb::ScrollTo {
        row_key: s.row_key.clone(),
        highlight_ms: HIGHLIGHT_MS,
        token: s.token,
    })
}

fn item(store: &Store, a: &pb::Anchor, now: SystemTime, tz: &jiff::tz::TimeZone) -> pb::AnchorItem {
    use pb::anchor::Kind as K;
    let (kind, preview) = match a.kind.as_ref() {
        Some(K::UserMessage(m)) => (pb::AnchorKind::User, m.excerpt.clone()),
        Some(K::Waiting(w)) => (
            pb::AnchorKind::Waiting,
            // 和「待你处理」页同一份文案；待处理表里已经没有这一项（刚处理掉、锚点还没重取）时留空
            store
                .summary
                .pending
                .iter()
                .find(|p| pending::item_id(p) == Some(w.pending_item_id.as_str()))
                .map(|p| pending::item_title(store, p))
                .unwrap_or_default(),
        ),
        Some(K::FailedTurn(f)) => (
            pb::AnchorKind::Failed,
            text::anchor_failure(f.had_output, &f.reason),
        ),
        None => (pb::AnchorKind::Unspecified, String::new()),
    };
    pb::AnchorItem {
        key: key_of(a),
        kind: kind.into(),
        kind_label: text::anchor_kind(kind).to_owned(),
        time_label: a
            .at
            .as_ref()
            .map(|t| text::clock_or_date(now, to_system_time(t), tz))
            .unwrap_or_default(),
        preview,
    }
}
