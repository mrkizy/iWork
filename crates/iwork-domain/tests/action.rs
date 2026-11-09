//! 动作指纹：按「动作类型 + 目标」识别同一个动作，与工具和目标的写法无关（需求 AC05）。

use iwork_domain::action::{Action, ActionFlags, ActionKind, ActionTarget, CanonicalPath, ToolRef};

fn p(s: &str) -> CanonicalPath {
    CanonicalPath(s.into())
}

fn action(kind: ActionKind, target: ActionTarget, tool: &str) -> Action {
    Action {
        kind,
        target,
        tool: ToolRef(tool.into()),
        raw_summary: format!("{tool} 的摘要"),
        flags: ActionFlags {
            out_of_bounds: tool.len().is_multiple_of(2),
            ..ActionFlags::default()
        },
    }
}

#[test]
fn fingerprint_ignores_tool_and_target_spelling() {
    let write = |target, tool| action(ActionKind::Write, target, tool).fingerprint();
    let single = write(ActionTarget::Path(p("/a")), "Edit");
    // 换工具、换摘要与标记：同一个动作
    assert_eq!(single, write(ActionTarget::Path(p("/a")), "MultiEdit"));
    // 单路径与单元素集合、重复元素的集合：同一个动作
    assert_eq!(single, write(ActionTarget::Paths(vec![p("/a")]), "Write"));
    assert_eq!(
        single,
        write(ActionTarget::Paths(vec![p("/a"), p("/a")]), "Write")
    );
    // 多路径：顺序与重复不影响
    assert_eq!(
        write(ActionTarget::Paths(vec![p("/b"), p("/a"), p("/b")]), "x"),
        write(ActionTarget::Paths(vec![p("/a"), p("/b")]), "y"),
    );
    // 空集合就是没有目标
    assert_eq!(
        write(ActionTarget::Paths(vec![]), "x"),
        write(ActionTarget::None, "y")
    );
}

#[test]
fn fingerprint_distinguishes_different_actions() {
    let fp = |kind, target| action(kind, target, "t").fingerprint();
    let a = fp(ActionKind::Write, ActionTarget::Path(p("/a")));
    assert_ne!(a, fp(ActionKind::Write, ActionTarget::Path(p("/b"))));
    assert_ne!(a, fp(ActionKind::Delete, ActionTarget::Path(p("/a"))));
    assert_ne!(
        a,
        fp(
            ActionKind::Write,
            ActionTarget::Paths(vec![p("/a"), p("/b")])
        )
    );
    assert_ne!(a, fp(ActionKind::Write, ActionTarget::Remote("/a".into())));
}
