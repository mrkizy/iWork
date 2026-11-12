//! GetConversation 的取页范围（T09 方案 G11、2.9）。六种取法：最新一页、往前翻（before_message_id）、往后翻
//! （after_message_id）、以某个位置为中心（around_seq）这四种按消息数取，只算消息下标的半开区间 [start, end)，
//! 其余实体按这一段的 seq 范围随页带上；行级游标（before_seq、after_seq）按行数取，见 [`RowCursor`]。

use iwork_proto::pb;

use super::invalid;

/// 行级游标一次最多取多少行（方案 2.9：limit 大于它按它取）。
pub const ROW_LIMIT_MAX: u32 = 1000;

/// 取法：按消息数取，还是按行数取（行级游标）。
pub(super) enum Mode {
    Messages,
    Rows(RowCursor),
}

/// 行级游标（方案 2.9「行级游标的语义」）：排序字段小于（`Before`）或大于（`After`）`seq` 的各类行里，
/// 紧挨着它的 `limit` 行。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum RowCursor {
    Before { seq: u64, limit: usize },
    After { seq: u64, limit: usize },
}

impl RowCursor {
    /// 已按排序字段从旧到新排好的全部行 → 选中的那一段（下标半开区间），以及两侧还有没有：(has_more, has_newer)。
    pub(super) fn select(self, sorted: &[u64]) -> ((usize, usize), bool, bool) {
        match self {
            RowCursor::Before { seq, limit } => {
                let end = sorted.partition_point(|s| *s < seq);
                let start = end.saturating_sub(limit);
                ((start, end), start > 0, end < sorted.len())
            }
            RowCursor::After { seq, limit } => {
                let start = sorted.partition_point(|s| *s <= seq);
                let end = (start + limit).min(sorted.len());
                ((start, end), start > 0, end < sorted.len())
            }
        }
    }
}

/// 校验取法字段（至多设一个），分出取法。
pub(super) fn mode(g: &pb::GetConversation) -> Result<Mode, pb::Error> {
    let set = [
        g.before_message_id.is_some(),
        g.after_message_id.is_some(),
        g.around_seq.is_some(),
        g.before_seq.is_some(),
        g.after_seq.is_some(),
    ]
    .into_iter()
    .filter(|b| *b)
    .count();
    if set > 1 {
        return Err(invalid(
            "get_conversation",
            "before_message_id、after_message_id、around_seq、before_seq、after_seq 至多设一个",
        ));
    }
    let row = |field: &str, seq: u64| {
        if seq == 0 {
            return Err(invalid(field, "行级游标不能为 0"));
        }
        if g.limit == 0 {
            return Err(invalid("limit", "行级游标的 limit 不能为 0"));
        }
        Ok(g.limit.min(ROW_LIMIT_MAX) as usize)
    };
    Ok(match (g.before_seq, g.after_seq) {
        (Some(seq), _) => Mode::Rows(RowCursor::Before {
            seq,
            limit: row("before_seq", seq)?,
        }),
        (_, Some(seq)) => Mode::Rows(RowCursor::After {
            seq,
            limit: row("after_seq", seq)?,
        }),
        _ => Mode::Messages,
    })
}

/// 消息按创建顺序排好时，这一页覆盖的下标区间（按消息数取的四种取法；调用前已由 [`mode`] 校验过互斥）。
pub(super) fn page_range(
    messages: &[pb::Message],
    g: &pb::GetConversation,
    limit: usize,
) -> Result<(usize, usize), pb::Error> {
    let index_of = |field: &str, id: &str| {
        messages
            .iter()
            .position(|m| m.id == id)
            .ok_or_else(|| invalid(field, &format!("消息不在对话里：{id}")))
    };
    let len = messages.len();
    Ok(if let Some(before) = &g.before_message_id {
        let end = index_of("before_message_id", before)?;
        (end.saturating_sub(limit), end)
    } else if let Some(after) = &g.after_message_id {
        let start = index_of("after_message_id", after)? + 1;
        (start, (start + limit).min(len))
    } else if let Some(around) = g.around_seq {
        // 以 around 为中心：它之前（含）取一半，之后补满；靠近两端时往另一边挪，页长尽量不变。
        let pivot = messages.partition_point(|m| m.created_seq <= around);
        let start = pivot
            .saturating_sub(limit / 2)
            .min(len.saturating_sub(limit));
        (start, (start + limit).min(len))
    } else {
        (len.saturating_sub(limit), len)
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn msgs(n: u64) -> Vec<pb::Message> {
        (1..=n)
            .map(|i| pb::Message {
                id: format!("m{i}"),
                created_seq: i * 10,
                ..Default::default()
            })
            .collect()
    }

    fn get(f: impl FnOnce(&mut pb::GetConversation)) -> pb::GetConversation {
        let mut g = pb::GetConversation::default();
        f(&mut g);
        g
    }

    #[test]
    fn four_ways_to_page() {
        let m = msgs(10);
        assert_eq!(
            page_range(&m, &get(|_| {}), 3).unwrap(),
            (7, 10),
            "最新一页"
        );
        assert_eq!(
            page_range(&m, &get(|g| g.before_message_id = Some("m5".into())), 3).unwrap(),
            (1, 4)
        );
        assert_eq!(
            page_range(&m, &get(|g| g.after_message_id = Some("m5".into())), 3).unwrap(),
            (5, 8)
        );
        assert_eq!(
            page_range(&m, &get(|g| g.after_message_id = Some("m9".into())), 3).unwrap(),
            (9, 10),
            "往后翻到头"
        );
        // m5 的 seq 是 50：以它为中心，前面（含）一半
        assert_eq!(
            page_range(&m, &get(|g| g.around_seq = Some(50)), 4).unwrap(),
            (3, 7)
        );
        assert_eq!(
            page_range(&m, &get(|g| g.around_seq = Some(5)), 4).unwrap(),
            (0, 4),
            "靠最旧一端：往后补满"
        );
        assert_eq!(
            page_range(&m, &get(|g| g.around_seq = Some(1000)), 4).unwrap(),
            (6, 10),
            "靠最新一端：往前补满"
        );
    }

    #[test]
    fn row_cursor_selects_next_to_cursor() {
        let sorted = [2, 4, 6, 8, 10];
        let before = RowCursor::Before { seq: 8, limit: 2 };
        assert_eq!(before.select(&sorted), ((1, 3), true, true), "4、6");
        let after = RowCursor::After { seq: 4, limit: 10 };
        assert_eq!(
            after.select(&sorted),
            ((2, 5), true, false),
            "6、8、10，到头"
        );
        let empty = RowCursor::Before { seq: 1, limit: 3 };
        assert_eq!(
            empty.select(&sorted),
            ((0, 0), false, true),
            "游标之前没有行"
        );
    }

    #[test]
    fn at_most_one_way() {
        let m = msgs(3);
        let g = get(|g| {
            g.before_message_id = Some("m2".into());
            g.around_seq = Some(10);
        });
        assert!(matches!(
            mode(&g),
            Err(e) if e.code == i32::from(pb::ErrorCode::InvalidArgument)
        ));
        assert!(page_range(&m, &get(|g| g.after_message_id = Some("没有".into())), 2).is_err());
    }
}
