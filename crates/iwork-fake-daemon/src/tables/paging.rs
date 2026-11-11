//! GetConversation 的取页范围（T09 方案 G11）：最新一页、往前翻（before_message_id）、往后翻（after_message_id）、
//! 以某个位置为中心（around_seq）。只算消息下标的半开区间 [start, end)，其余实体按这一段的 seq 范围随页带上。

use iwork_proto::pb;

use super::invalid;

/// 消息按创建顺序排好时，这一页覆盖的下标区间。
pub(super) fn page_range(
    messages: &[pb::Message],
    g: &pb::GetConversation,
    limit: usize,
) -> Result<(usize, usize), pb::Error> {
    let set = [
        g.before_message_id.is_some(),
        g.after_message_id.is_some(),
        g.around_seq.is_some(),
    ]
    .into_iter()
    .filter(|b| *b)
    .count();
    if set > 1 {
        return Err(invalid(
            "get_conversation",
            "before_message_id、after_message_id、around_seq 至多设一个",
        ));
    }
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
    fn at_most_one_way() {
        let m = msgs(3);
        let g = get(|g| {
            g.before_message_id = Some("m2".into());
            g.around_seq = Some(10);
        });
        assert_eq!(
            page_range(&m, &g, 2).unwrap_err().code,
            i32::from(pb::ErrorCode::InvalidArgument)
        );
        assert!(page_range(&m, &get(|g| g.after_message_id = Some("没有".into())), 2).is_err());
    }
}
