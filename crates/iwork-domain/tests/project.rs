//! 项目的最近活跃、置顶与侧栏排序（T08 范围第 7 条，需求 FR19）。

use std::cmp::Ordering;

use iwork_domain::common::Timestamp;
use iwork_domain::id::{ProjectId, RawUlid};
use iwork_domain::project::{PathStatus, Project, ProjectSettings, VcsKind, sidebar_order};
use proptest::prelude::*;

fn project(n: u128, name: &str, pinned: bool, last: Option<i64>) -> Project {
    Project {
        id: ProjectId::from_raw(RawUlid::from_u128(n)),
        root_path: format!("/p/{name}").into(),
        vcs_kind: VcsKind("v".into()),
        name: name.into(),
        pinned,
        last_active_at: last.map(Timestamp),
        path_status: PathStatus::Ok,
        settings: ProjectSettings::default(),
    }
}

#[test]
fn touch_only_moves_forward() {
    let mut p = project(1, "a", false, None);
    assert!(p.touch(Timestamp(10)));
    assert!(!p.touch(Timestamp(5)), "旧时间不回拨");
    assert!(!p.touch(Timestamp(10)), "同一时间不算变化");
    assert_eq!(p.last_active_at, Some(Timestamp(10)));
    assert!(p.touch(Timestamp(11)));
    assert_eq!(p.last_active_at, Some(Timestamp(11)));
}

#[test]
fn set_pinned_reports_changes() {
    let mut p = project(1, "a", false, None);
    assert!(!p.set_pinned(false));
    assert!(p.set_pinned(true));
    assert!(p.pinned);
    assert!(!p.set_pinned(true));
}

#[test]
fn sidebar_puts_pinned_first_then_most_recent_then_never_active() {
    let mut list = [
        project(1, "old", false, Some(1)),
        project(2, "never", false, None),
        project(3, "pinned-old", true, Some(2)),
        project(4, "recent", false, Some(9)),
        project(5, "pinned-new", true, Some(8)),
        project(6, "pinned-never", true, None),
    ];
    list.sort_by(sidebar_order);
    let names: Vec<&str> = list.iter().map(|p| p.name.as_str()).collect();
    assert_eq!(
        names,
        [
            "pinned-new",
            "pinned-old",
            "pinned-never",
            "recent",
            "old",
            "never"
        ]
    );
}

#[test]
fn sidebar_ties_break_by_name_then_id() {
    let a = project(2, "same", false, Some(1));
    let b = project(1, "same", false, Some(1));
    assert_eq!(sidebar_order(&a, &b), Ordering::Greater, "名字相同按 ID");
    let c = project(9, "alpha", false, Some(1));
    assert_eq!(sidebar_order(&c, &a), Ordering::Less, "时间相同按名字");
}

fn arb_project() -> impl Strategy<Value = Project> {
    (
        any::<u128>(),
        "[a-c]{1,2}",
        any::<bool>(),
        prop::option::of(0i64..20),
    )
        .prop_map(|(n, name, pinned, last)| project(n, &name, pinned, last))
}

proptest! {
    /// 任何一组项目排出来：置顶的全在前，组内最近活跃时间不增，且从没活跃过的在组末尾。
    #[test]
    fn sidebar_order_invariants(mut list in prop::collection::vec(arb_project(), 0..20)) {
        list.sort_by(sidebar_order);
        let first_unpinned = list.iter().position(|p| !p.pinned).unwrap_or(list.len());
        prop_assert!(list[first_unpinned..].iter().all(|p| !p.pinned));
        for group in [&list[..first_unpinned], &list[first_unpinned..]] {
            for w in group.windows(2) {
                match (w[0].last_active_at, w[1].last_active_at) {
                    (Some(x), Some(y)) => prop_assert!(x >= y),
                    (None, Some(_)) => prop_assert!(false, "没活跃过的排在活跃过的前面"),
                    _ => {}
                }
            }
        }
    }

    /// 多次 touch 后最近活跃时间等于其中的最大值。
    #[test]
    fn touch_keeps_the_maximum(times in prop::collection::vec(0i64..1000, 1..20)) {
        let mut p = project(1, "a", false, None);
        for &t in &times {
            p.touch(Timestamp(t));
        }
        prop_assert_eq!(p.last_active_at, times.iter().copied().max().map(Timestamp));
    }
}
