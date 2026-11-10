//! 卡验收 2：重发同一 `command_id` 返回相同结果，事件不重复（proptest）。

mod support;

use std::collections::HashMap;

use iwork_domain::event::{DomainEvent, ProjectAdded};
use iwork_domain::id::Seq;
use iwork_ports::store::{CommitReceipt, EventFilter, StoragePort};
use proptest::prelude::*;
use support::*;

proptest! {
    #![proptest_config(ProptestConfig { cases: 48, ..ProptestConfig::default() })]

    /// 随机命令序列（`command_id` 取自很小的范围，必然有重复；每次带的事件数 0–3）：
    /// 事件总数 = 去重后各命令首次提交的事件数之和；重复的回执与首次逐字段相等，只是 `replayed`。
    #[test]
    fn duplicate_command_ids_never_duplicate_events(
        plan in prop::collection::vec((0u128..6, 0usize..4), 1..24)
    ) {
        let env = Env::new();
        let store = env.open();
        let mut first: HashMap<u128, CommitReceipt> = HashMap::new();
        let mut expected_events = 0u64;
        for (pos, (id, n)) in plan.iter().enumerate() {
            // 每次的事件都不同：重发时带了不同内容也必须被忽略
            let events: Vec<DomainEvent> = (0..*n)
                .map(|j| ProjectAdded { project: project((pos * 10 + j + 1) as u128) }.into())
                .collect();
            let got = block(store.commit(cmd(*id + 1, events))).unwrap();
            match first.get(id) {
                None => {
                    prop_assert!(!got.replayed);
                    expected_events += *n as u64;
                    first.insert(*id, got);
                }
                Some(prev) => {
                    prop_assert_eq!(got, CommitReceipt { replayed: true, ..*prev });
                }
            }
        }
        let head = block(store.head_seq()).unwrap();
        prop_assert_eq!(head, Seq(expected_events));
        let all = block(store.read_events(EventFilter::All, Seq(0), 10_000)).unwrap();
        prop_assert_eq!(all.len() as u64, expected_events);
        prop_assert_eq!(
            scalar(&env.db(), "SELECT COUNT(*) FROM projects") as u64,
            expected_events,
            "投影也没重复"
        );
    }
}
