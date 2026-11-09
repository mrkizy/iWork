//! ID 的文本形式（方案 4.1）。

use std::str::FromStr;

use iwork_domain::id::{ConversationId, IdParseError, IdParseReason, ProjectId, RawUlid};
use proptest::prelude::*;

proptest! {
    /// 任意 128 位值 → 26 字符文本 → 解析回来，等于原值。
    #[test]
    fn id_roundtrip(value in any::<u128>()) {
        let id = ProjectId::from_raw(RawUlid::from_u128(value));
        let text = id.to_string();
        prop_assert_eq!(text.len(), 26);
        prop_assert_eq!(ProjectId::from_str(&text), Ok(id));
        prop_assert_eq!(id.to_raw().to_u128(), value);
    }

    /// ID 的大小顺序就是文本的字典序：按 ID 排序即按创建时间排序。
    #[test]
    fn id_order_matches_text_order(a in any::<u128>(), b in any::<u128>()) {
        let (ia, ib) = (ProjectId::from_raw(RawUlid::from_u128(a)), ProjectId::from_raw(RawUlid::from_u128(b)));
        prop_assert_eq!(ia.cmp(&ib), ia.to_string().cmp(&ib.to_string()));
    }
}

#[test]
fn id_known_value() {
    // 期望值独立给出：全 1 的 128 位就是 7 后面 25 个 Z
    let id = ConversationId::from_raw(RawUlid::from_u128(u128::MAX));
    assert_eq!(id.to_string(), "7ZZZZZZZZZZZZZZZZZZZZZZZZZ");
    assert_eq!(
        format!("{id:?}"),
        "ConversationId(7ZZZZZZZZZZZZZZZZZZZZZZZZZ)"
    );
    let raw = RawUlid::from_parts(1, 2);
    assert_eq!((raw.timestamp_ms(), raw.random()), (1, 2));
    assert_eq!(raw.increment().map(RawUlid::random), Some(3));
    assert_eq!(RawUlid::from_parts(1, (1u128 << 80) - 1).increment(), None);
}

#[test]
fn id_parse_errors_name_kind_and_input() {
    let cases = [
        ("01ARZ3NDEKTSV4RRFFQ69G5FA", IdParseReason::InvalidLength),
        ("01ARZ3NDEKTSV4RRFFQ69G5FAVX", IdParseReason::InvalidLength),
        ("01ARZ3NDEKTSV4RRFFQ69G5FAU", IdParseReason::InvalidChar),
        ("01arz3ndektsv4rrffq69g5fav", IdParseReason::NotCanonical),
        ("8ZZZZZZZZZZZZZZZZZZZZZZZZZ", IdParseReason::NotCanonical),
    ];
    for (input, reason) in cases {
        assert_eq!(
            ProjectId::from_str(input),
            Err(IdParseError {
                kind: "project",
                input: input.into(),
                reason
            }),
            "{input}"
        );
    }
    // 对照：规范写法能解析
    assert!(ProjectId::from_str("01ARZ3NDEKTSV4RRFFQ69G5FAV").is_ok());
}
