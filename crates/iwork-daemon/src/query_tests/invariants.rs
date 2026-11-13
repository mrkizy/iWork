//! 查询结果自己该满足的性质（审查 2026-11-13-m1-t10a-d7 1/F-1、1/F-2）。
//!
//! 和假后台对拍挡不住「两边照同一个错的理解写」：假后台是 T09 先写的参照，真后台照着它写，同一处理解错了，
//! 两边一起错、对拍照样相等。这里的期望值只从整个对话的行集合和场景里回合的状态算，不读假后台，也不读被测
//! 查询的实现。

use std::collections::BTreeSet;

use super::*;

/// 页里出现的回合编号。
fn turn_ids(g: &pb::GetConversationResult) -> BTreeSet<String> {
    g.turns.iter().map(|t| t.id.clone()).collect()
}

/// 每一页：工具、审批、卡片所属的回合都在 `turns` 里；还没到终态的回合（排着、在跑、等审批、取消中、中断）
/// 也都在——之后它们的状态变化、工具、结束事件到了，客户端要在 `turns` 里找得到它（client-core
/// `store/timeline.rs` 找不到就丢）。
fn assert_page_turns(w: &World, conv: u128, kind: &Q, g: &pb::GetConversationResult) {
    let have = turn_ids(g);
    let owners = g
        .tool_calls
        .iter()
        .map(|c| &c.turn_id)
        .chain(g.approvals.iter().map(|a| &a.turn_id))
        .chain(g.user_actions.iter().map(|c| &c.turn_id));
    for owner in owners {
        assert!(
            have.contains(owner),
            "{kind:?}：行所属的回合 {owner} 不在页里"
        );
    }
    for t in w.turns.iter().filter(|t| t.conv == conv) {
        if !t.state.is_terminal() {
            let id = tid(t.id).to_string();
            assert!(
                have.contains(&id),
                "{kind:?}：没到终态的回合 {id}（{:?}）不在页里",
                t.state
            );
        }
    }
}

/// 整个对话的「行」（T09 方案 2.9）：消息、工具、审批、卡片按 `created_seq`，结束了且到了终态的回合按
/// `ended_seq`。键是行的身份，值是排序字段。
fn all_rows(full: &pb::GetConversationResult) -> Vec<(u64, String)> {
    let mut rows: Vec<(u64, String)> = Vec::new();
    rows.extend(
        full.messages
            .iter()
            .map(|m| (m.created_seq, format!("m:{}", m.id))),
    );
    rows.extend(
        full.tool_calls
            .iter()
            .map(|c| (c.created_seq, format!("t:{}/{}", c.turn_id, c.tool_call_id))),
    );
    rows.extend(
        full.approvals
            .iter()
            .map(|a| (a.created_seq, format!("a:{}", a.id))),
    );
    rows.extend(
        full.user_actions
            .iter()
            .map(|c| (c.created_seq, format!("c:{}", c.id))),
    );
    let ending = [
        pb::TurnState::Completed as i32,
        pb::TurnState::Cancelled as i32,
        pb::TurnState::Failed as i32,
    ];
    rows.extend(
        full.turns
            .iter()
            .filter(|t| t.ended_seq != 0 && ending.contains(&t.state))
            .map(|t| (t.ended_seq, format!("end:{}", t.id))),
    );
    rows.sort();
    rows
}

/// 一页里的实体行（不含结束行：结束行只能从 `turns` 看出「可能被选中」，单独判）。
fn entity_rows(g: &pb::GetConversationResult) -> BTreeSet<String> {
    let mut out = BTreeSet::new();
    out.extend(g.messages.iter().map(|m| format!("m:{}", m.id)));
    out.extend(
        g.tool_calls
            .iter()
            .map(|c| format!("t:{}/{}", c.turn_id, c.tool_call_id)),
    );
    out.extend(g.approvals.iter().map(|a| format!("a:{}", a.id)));
    out.extend(g.user_actions.iter().map(|c| format!("c:{}", c.id)));
    out
}

fn page(q: &Queries<ProtoEventCodec>, kind: Q) -> pb::GetConversationResult {
    match real_answer(q, kind.clone()).unwrap().kind {
        Some(RK::GetConversation(g)) => g,
        other => panic!("{kind:?}：{other:?}"),
    }
}

/// 行级游标一页的期望（T09 方案 2.9）：`before_seq = s` 取小于 s 的最大 `limit` 行；`has_more` = 选中的之外
/// 还有更旧的；`has_newer` = 有比选中的最新一行更新的行，结果为空时是「有比 s 更新的行」（严格大于）。
/// `after_seq` 对称。
fn expected_row_page(
    rows: &[(u64, String)],
    before: bool,
    s: u64,
    limit: usize,
) -> (BTreeSet<String>, bool, bool) {
    let side: Vec<&(u64, String)> = if before {
        rows.iter().rev().filter(|(seq, _)| *seq < s).collect()
    } else {
        rows.iter().filter(|(seq, _)| *seq > s).collect()
    };
    let chosen: Vec<&(u64, String)> = side.iter().take(limit).copied().collect();
    let beyond = side.len() > limit;
    let other = match chosen.last().map(|_| ()) {
        // 结果不空：另一侧从游标起算（游标那一行本身在另一侧）
        Some(()) if before => rows.iter().any(|(seq, _)| *seq >= s),
        Some(()) => rows.iter().any(|(seq, _)| *seq <= s),
        // 结果为空：严格比较
        None if before => rows.iter().any(|(seq, _)| *seq > s),
        None => rows.iter().any(|(seq, _)| *seq < s),
    };
    let ids = chosen.iter().map(|(_, id)| id.clone()).collect();
    if before {
        (ids, beyond, other)
    } else {
        (ids, other, beyond)
    }
}

/// 一个场景里，六种取法的每一页都满足 `assert_page_turns`；行级游标的页选中的行与两个标志等于从整个对话的行
/// 集合独立算出来的。
fn check_world(w: &World) {
    let q = Queries::new(read(&w.store), Default::default());
    let head = block(w.store.head_seq()).unwrap().0;
    let mut checked = 0;
    for c in CONVS {
        let full = page(&q, get(c, |g| g.limit = 1000));
        assert!(!full.has_more && !full.has_newer, "对照：整页取全了");
        let rows = all_rows(&full);
        let mut kinds: Vec<Q> = vec![get(c, |_| {}), get(c, |g| g.limit = 1)];
        for m in &full.messages {
            kinds.push(get(c, |g| g.before_message_id = Some(m.id.clone())));
            kinds.push(get(c, |g| g.after_message_id = Some(m.id.clone())));
        }
        for s in 1..=head + 1 {
            kinds.push(get(c, |g| g.around_seq = Some(s)));
        }
        for kind in kinds {
            let g = page(&q, kind.clone());
            assert_page_turns(w, c, &kind, &g);
            checked += 1;
        }
        for s in 1..=head + 1 {
            for before in [true, false] {
                for limit in [1u32, 3] {
                    let kind = get(c, |g| {
                        g.limit = limit;
                        if before {
                            g.before_seq = Some(s);
                        } else {
                            g.after_seq = Some(s);
                        }
                    });
                    let g = page(&q, kind.clone());
                    assert_page_turns(w, c, &kind, &g);
                    let (want, more, newer) = expected_row_page(&rows, before, s, limit as usize);
                    let want_entities: BTreeSet<String> = want
                        .iter()
                        .filter(|id| !id.starts_with("end:"))
                        .cloned()
                        .collect();
                    assert_eq!(entity_rows(&g), want_entities, "{kind:?}");
                    let have = turn_ids(&g);
                    for end in want.iter().filter_map(|id| id.strip_prefix("end:")) {
                        assert!(
                            have.contains(end),
                            "{kind:?}：选中的结束行 {end} 的回合不在页里"
                        );
                    }
                    assert_eq!((g.has_more, g.has_newer), (more, newer), "{kind:?}");
                    checked += 1;
                }
            }
        }
    }
    assert!(checked > 0, "对照：一页也没查");
}

/// 1/F-1 的反例：回合开跑后用户又发了一条消息，工具才开始。最新一页只有一行时，这一页里有工具，它所属的、
/// 还在跑的回合要在 `turns` 里。
#[test]
fn latest_page_carries_owner_of_tool_started_after_newer_message() {
    let env = Env::new();
    let mut w = World::new(&env);
    // 消息 → 排回合 → 开跑 → 又一条消息 → 工具开始（对话 10；`a = 0` 选对话 10，`b = 0` 选第一个）
    for op in [0, 1, 2, 0, 3] {
        w.step(op, 0, 0);
    }
    let q = Queries::new(read(&w.store), Default::default());
    let latest = page(&q, get(10, |g| g.limit = 1));
    assert!(
        !latest.tool_calls.is_empty() || !latest.messages.is_empty(),
        "对照：页不空"
    );
    let kind = get(10, |g| g.limit = 1);
    assert_page_turns(&w, 10, &kind, &latest);
    let row = page(&q, get(10, |g| g.before_seq = Some(1_000_000)));
    assert_page_turns(&w, 10, &kind, &row);
}

/// 1/F-2 的反例：对话里只有一行（排序字段 s）。`before_seq = s`、`after_seq = s` 都是空页，两个标志都是假。
#[test]
fn empty_row_page_flags_exclude_the_cursor_row() {
    let env = Env::new();
    let mut w = World::new(&env);
    w.step(0, 0, 0);
    let q = Queries::new(read(&w.store), Default::default());
    let full = page(&q, get(10, |g| g.limit = 1000));
    assert_eq!(full.messages.len(), 1, "对照：只有一条消息");
    let s = full.messages[0].created_seq;
    for before in [true, false] {
        let kind = get(10, |g| {
            if before {
                g.before_seq = Some(s);
            } else {
                g.after_seq = Some(s);
            }
        });
        let g = page(&q, kind.clone());
        assert!(entity_rows(&g).is_empty(), "{kind:?}：对照，页是空的");
        assert_eq!((g.has_more, g.has_newer), (false, false), "{kind:?}");
    }
}

/// 固定的 120 步场景（和对拍同一份），每种取法、每个位置都过一遍。
#[test]
fn every_page_satisfies_turn_and_row_invariants() {
    let env = Env::new();
    let mut w = World::new(&env);
    let mut runner = proptest::test_runner::TestRunner::deterministic();
    let ops = proptest::collection::vec((0..OPS, any::<u8>(), any::<u8>()), 120)
        .new_tree(&mut runner)
        .unwrap()
        .current();
    for (op, a, b) in ops {
        w.step(op, a, b);
    }
    // 留着跑的回合：先查一遍（有活动回合），再全部中断查一遍
    check_world(&w);
    w.interrupt_running();
    check_world(&w);
}

proptest! {
    #![proptest_config(ProptestConfig { cases: 24, ..ProptestConfig::default() })]

    /// 随机场景，任意时刻停下来查。
    #[test]
    fn random_worlds_satisfy_turn_and_row_invariants(
        ops in proptest::collection::vec((0..OPS, any::<u8>(), any::<u8>()), 1..40),
    ) {
        let env = Env::new();
        let mut w = World::new(&env);
        for (op, a, b) in ops {
            w.step(op, a, b);
        }
        check_world(&w);
    }
}
