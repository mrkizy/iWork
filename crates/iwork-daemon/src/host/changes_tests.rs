//! 改动探针（D10）里宿主这一半：比较的纯函数、开关、额度。

use super::*;

fn present(b: u8) -> PathVersion {
    PathVersion::Present {
        kind: iwork_ports::vcs::PathKind::File,
        digest: [b; 32],
    }
}

/// 开始那一份里没有、现在出现的算变了；版本不同、有一头说不清算变了；一样的不算。
#[test]
fn changed_paths_compares_versions() {
    let p = |s: &str| PathBuf::from(s);
    let start = BTreeMap::from([
        (p("same"), present(1)),
        (p("edited"), present(1)),
        (p("unknown"), PathVersion::Unknown),
    ]);
    let now = BTreeMap::from([
        (p("same"), present(1)),
        (p("edited"), present(2)),
        (p("unknown"), present(3)),
        (p("new"), present(4)),
    ]);
    assert_eq!(
        changed_paths(&start, &now),
        vec![p("edited"), p("new"), p("unknown")]
    );
}

/// 额度：同时最多两个，第三个拿不到、不等；还一个之后又拿得到。
#[test]
fn budget_is_bounded_and_returned_on_drop() {
    let b = Arc::new(ReadBudget::default());
    let one = b.try_take().unwrap();
    let _two = b.try_take().unwrap();
    assert!(b.try_take().is_none());
    assert_eq!(b.in_flight(), 2);
    drop(one);
    assert!(b.try_take().is_some());
}
