//! 时间线行数上限严格成立与行级游标（T09 方案 2.2「查询结果怎么进时间线」第 4 条、R14，组 2b 审查 5/F-3）。
//!
//! 「服务器」是假后台的折叠表（同进程直接查）：状态库按自己的游标该发什么，测试就照样去问它，再把结果按同一个用途
//! 应用回去。会话层怎么挑游标、发查询，另有连真假后台的用例（`tests/row_cap_live.rs` 一节在本文件末尾）。

mod support;

use iwork_client_core::session::PAGE_SIZE;
use iwork_client_core::store::{
    Item, PageCursor, QueryPurpose, ROW_PAGE, RowKey, ScopeKind, Store, Timeline, Visible,
};
use iwork_fake_daemon::fixtures as f;
use iwork_fake_daemon::{Tables, envelope_time};
use iwork_proto::pb;
use pb::TurnState as T;
use support::{CID, CONV};

// ---------- 世界与「服务器」 ----------

/// 假后台的表 + 事件头。事件只经 `push` 追加，和真后台一样 seq 递增。
struct World {
    t: Tables,
    head: u64,
}

impl World {
    fn new() -> Self {
        let mut w = World {
            t: Tables::default(),
            head: 0,
        };
        w.push(f::project(support::PID, "demo", "/tmp/demo"));
        w.push(f::project_conversation(CID, support::PID));
        w
    }

    fn push(&mut self, e: pb::DomainEvent) -> u64 {
        self.head += 1;
        self.t.fold(
            self.head,
            &envelope_time(self.head),
            e.event.as_ref().unwrap(),
        );
        self.head
    }

    fn get(&self, g: pb::GetConversation) -> pb::QueryResult {
        self.t
            .query(
                self.head,
                &pb::Query {
                    kind: Some(pb::query::Kind::GetConversation(pb::GetConversation {
                        conversation_id: CID.into(),
                        ..g
                    })),
                },
            )
            .expect("假后台拒了查询")
    }

    /// 一个回合：用户问一句，排队、运行，`tools` 次工具调用（第 `approval_at` 次后面夹一个审批），助手答一句，结束。
    /// 返回审批的 seq。
    fn long_turn(&mut self, n: &str, tools: usize, approval_at: Option<usize>) -> Option<u64> {
        let turn = format!("t-{n}");
        self.push(f::user_message(&format!("q-{n}"), CID, "跑一遍"));
        self.push(f::turn_queued(&turn, CID, f::IWORK));
        self.push(f::turn_state(&turn, T::Queued, T::Running, None));
        let mut approval = None;
        for i in 0..tools {
            self.push(f::tool_started(
                &turn,
                &format!("{n}-tc{i}"),
                "Bash",
                "cargo test",
            ));
            if approval_at == Some(i) {
                approval = Some(self.push(f::approval(
                    &format!("ap-{n}"),
                    CID,
                    &turn,
                    f::command_action("rm -rf build"),
                    pb::ApprovalKind::Ask,
                    &[],
                )));
            }
        }
        self.push(f::assistant_message(
            &format!("a-{n}"),
            CID,
            &turn,
            f::IWORK,
            "好了",
        ));
        self.push(f::turn_finished(&turn, T::Completed, false, None));
        approval
    }
}

fn store(cap: usize) -> Store {
    let mut s = Store::with_timeline_cap(cap);
    s.register_subscription(CONV, ScopeKind::Conversation(CID.into()));
    s.set_visible(Visible {
        conversation_id: Some(CID.into()),
        ..Default::default()
    });
    s
}

fn tl(s: &Store) -> &Timeline {
    s.open.as_ref().expect("时间线开着")
}

/// 打开对话：订阅快照里的最新页（按消息数取）。
fn open(s: &mut Store, w: &World) {
    let page = w.get(pb::GetConversation {
        limit: PAGE_SIZE,
        ..Default::default()
    });
    s.apply(support::snapshot(CONV, w.head, vec![page]));
}

/// 以 `seq` 为中心加载一段（跳到没加载的锚点）。
fn around(s: &mut Store, w: &World, seq: u64) {
    let page = w.get(pb::GetConversation {
        around_seq: Some(seq),
        limit: 50,
        ..Default::default()
    });
    s.apply(Item::ViewResult {
        purpose: QueryPurpose::AroundPage {
            conversation_id: CID.into(),
            seq,
        },
        result: Box::new(page),
    });
}

/// 往前翻一页，照会话层的规矩挑游标（`session::page_query`）。返回是否发了。
fn older(s: &mut Store, w: &World) -> bool {
    let Some(before) = tl(s).older_cursor().filter(|_| tl(s).has_older) else {
        return false;
    };
    let g = match &before {
        PageCursor::Message(id) => pb::GetConversation {
            before_message_id: Some(id.clone()),
            limit: PAGE_SIZE,
            ..Default::default()
        },
        PageCursor::Seq(seq) => pb::GetConversation {
            before_seq: Some(*seq),
            limit: ROW_PAGE,
            ..Default::default()
        },
    };
    let page = w.get(g);
    s.apply(Item::ViewResult {
        purpose: QueryPurpose::OlderPage {
            conversation_id: CID.into(),
            before,
        },
        result: Box::new(page),
    });
    true
}

/// 往后翻一页。
fn newer(s: &mut Store, w: &World) -> bool {
    let Some(after) = tl(s).newer_cursor().filter(|_| !tl(s).at_tail) else {
        return false;
    };
    let g = match &after {
        PageCursor::Message(id) => pb::GetConversation {
            after_message_id: Some(id.clone()),
            limit: PAGE_SIZE,
            ..Default::default()
        },
        PageCursor::Seq(seq) => pb::GetConversation {
            after_seq: Some(*seq),
            limit: ROW_PAGE,
            ..Default::default()
        },
    };
    let page = w.get(g);
    s.apply(Item::ViewResult {
        purpose: QueryPurpose::NewerPage {
            conversation_id: CID.into(),
            after,
        },
        result: Box::new(page),
    });
    true
}

/// 网页声明的可见窗口（ordinal = seq × 2 + slot）：顶着最旧一行 / 底着最新一行。
fn look_at_top(s: &mut Store) {
    let first = tl(s).identities().first().unwrap().0.ordinal();
    s.set_window(CID, first, first + 4, false);
}
fn look_at_bottom(s: &mut Store) {
    let last = tl(s).identities().last().unwrap().0.ordinal();
    s.set_window(CID, last - 4, last, false);
}

fn keys(s: &Store) -> Vec<(RowKey, String)> {
    tl(s).identities()
}

/// 不设上限时一次全取的行（对照）。
fn unlimited(w: &World) -> Vec<(RowKey, String)> {
    let mut s = store(100_000);
    let page = w.get(pb::GetConversation {
        limit: 100_000,
        ..Default::default()
    });
    s.apply(support::snapshot(CONV, w.head, vec![page]));
    keys(&s)
}

fn is_message(id: &str) -> bool {
    id.starts_with("q-") || id.starts_with("a-")
}

// ---------- 用例 ----------

/// 一个回合 200 次工具调用、上限 40：以中间的审批为中心加载后两端都没有消息行；往前、往后各翻到头，
/// 一路见过的行拼起来等于不设上限时一次全取的，而且任何时候都不超上限（R14）。
#[test]
fn trim_both_ends_then_page_back() {
    const CAP: usize = 40;
    let mut w = World::new();
    let approval = w.long_turn("x", 200, Some(100)).unwrap();
    let full = unlimited(&w);
    assert!(full.len() > 200, "对照：整段远超上限（{}）", full.len());

    let mut s = store(CAP);
    open(&mut s, &w);
    around(&mut s, &w, approval);
    let mut seen: std::collections::BTreeMap<RowKey, String> = keys(&s).into_iter().collect();
    {
        let t = tl(&s);
        assert!(t.len() <= CAP);
        assert!(seen.values().any(|id| id == "ap-x"), "目标行留着");
        assert!(
            !seen.values().any(|id| is_message(id)),
            "对照：两端都没有消息行"
        );
        assert!(
            t.edge_older.is_some() && t.edge_newer.is_some(),
            "两端都裁过、记了边界"
        );
    }
    // 往前翻到头（网页一直顶着最旧一行）
    let mut steps = 0;
    loop {
        look_at_top(&mut s);
        if !older(&mut s, &w) {
            break;
        }
        assert!(tl(&s).len() <= CAP, "第 {steps} 步超上限");
        seen.extend(keys(&s));
        steps += 1;
        assert!(steps < 50, "往前翻不到头");
    }
    assert!(!tl(&s).has_older);
    // 往后翻到头（网页一直底着最新一行）
    loop {
        look_at_bottom(&mut s);
        if !newer(&mut s, &w) {
            break;
        }
        assert!(tl(&s).len() <= CAP);
        seen.extend(keys(&s));
        steps += 1;
        assert!(steps < 100, "往后翻不到头");
    }
    assert!(tl(&s).at_tail, "接回了最新");
    assert_eq!(
        seen.into_iter().collect::<Vec<_>>(),
        full,
        "拼出来的等于一次全取的：不重、不漏"
    );
}

/// 边界正好卡在一条带分隔线的消息上（分隔线与消息同一个 seq）：两行一起丢，边界落在别的 seq 上（两个方向各一例）。
#[test]
fn trim_keeps_same_seq_rows_together() {
    fn msg(id: &str, seq: u64, project: Option<&str>) -> pb::Message {
        let mut m = support::assistant_msg(id, seq, pb::MessageStatus::Completed, "x");
        m.context_project_id = project.map(Into::into);
        m
    }
    // 10：没有项目；20：切到 x（分隔线 + 消息）；30：切回没有项目（分隔线 + 消息）
    let page = || {
        support::page(
            100,
            vec![
                msg("m10", 10, None),
                msg("m20", 20, Some("x")),
                msg("m30", 30, None),
            ],
            false,
        )
    };
    let seqs = |s: &Store| keys(s).iter().map(|(k, _)| k.seq).collect::<Vec<_>>();

    // 旧的一端：上限 3，看最新一行。五行：10、20（分隔线 + 消息）、30（分隔线 + 消息）。先丢 10 剩四行，还超；
    // 再丢 20 的两行（按行丢的话只丢分隔线就停了，边界落在 20 的两行中间）。30 的分隔线留着：它说的「切换」本身
    // 成立，往上翻回 20 时照样对
    let mut s = store(3);
    s.apply(support::snapshot(CONV, 100, vec![page()]));
    assert_eq!(seqs(&s), [30, 30], "{:?}", keys(&s));
    assert_eq!(tl(&s).edge_older, Some(30));

    // 新的一端：上限 4，看最旧一行。丢 30 的两行，边界是 20
    let mut s = store(4);
    s.set_window(CID, 0, 0, false);
    s.apply(support::snapshot(
        CONV,
        100,
        vec![support::page(100, vec![], false)],
    ));
    s.set_window(CID, 21, 21, false);
    s.apply(Item::ViewResult {
        purpose: QueryPurpose::AroundPage {
            conversation_id: CID.into(),
            seq: 10,
        },
        result: Box::new(page()),
    });
    assert_eq!(seqs(&s), [10, 20, 20], "{:?}", keys(&s));
    assert_eq!(tl(&s).edge_newer, Some(20));
    assert!(!tl(&s).at_tail);
}

/// 时间线整个重建（快照的最新页、居中的一段、回到最新）时两端的边界清空，再按这次的结果重新记。
#[test]
fn edge_cleared_on_rebuild() {
    let mut w = World::new();
    let approval = w.long_turn("x", 120, Some(60)).unwrap();
    let mut s = store(30);
    open(&mut s, &w);
    around(&mut s, &w, approval);
    assert!(
        tl(&s).edge_older.is_some() && tl(&s).edge_newer.is_some(),
        "对照：裁过"
    );
    // 回到最新：at_tail 为假时的最新页整个重建
    s.apply(Item::ViewResult {
        purpose: QueryPurpose::LatestPage {
            conversation_id: CID.into(),
        },
        result: Box::new(w.get(pb::GetConversation {
            limit: PAGE_SIZE,
            ..Default::default()
        })),
    });
    let t = tl(&s);
    assert_eq!(t.edge_newer, None, "新的一端接着最新，没有边界");
    assert!(t.at_tail);
    // 最新页本身超上限，旧的一端按这次的结果重新记了边界
    assert_eq!(t.edge_older, t.identities().first().map(|(k, _)| k.seq));

    // 小到不超上限的对话：重建后两端都没有边界
    let mut small = World::new();
    small.long_turn("y", 3, None);
    let mut s = store(30);
    open(&mut s, &small);
    let t = tl(&s);
    assert_eq!((t.edge_older, t.edge_newer), (None, None));
}

/// 行级翻页的一页里，最后一行属于一个很久以后才结束的回合（结果的 turns 带着它的 ended_seq）：这一页合并后没有它的
/// 结束行，再翻两页才出现；一路拼出来的行中间不缺（方案 2.2 第 4 条「行级翻页的结果怎么进时间线」）。
#[test]
fn row_page_skips_out_of_range_end_row() {
    // 上限远大于一页（ROW_PAGE = 200）：合并一页后只会从旧的一端裁，提前建出来的结束行不会被裁剪顺手藏掉
    // （它会把 edge_newer 带到远处，中间留洞）。3000 次工具调用，结束行在好几页之后。
    const CAP: usize = 2000;
    let mut w = World::new();
    w.long_turn("x", 3000, None);
    let full = unlimited(&w);
    let first_question = full.iter().find(|(_, id)| id == "q-x").unwrap().0.seq;

    let mut s = store(CAP);
    open(&mut s, &w);
    // 跳回开头：新的一端被裁，之后往后翻用行级游标
    around(&mut s, &w, first_question);
    assert!(tl(&s).edge_newer.is_some(), "对照：新的一端裁过");
    let mut seen: std::collections::BTreeMap<RowKey, String> = keys(&s).into_iter().collect();
    let mut pages_without_end = 0;
    loop {
        look_at_bottom(&mut s);
        if !newer(&mut s, &w) {
            break;
        }
        seen.extend(keys(&s));
        // 一路见过的行是整段的一个前缀：中间不缺
        let got: Vec<_> = seen.iter().map(|(k, id)| (*k, id.clone())).collect();
        assert_eq!(
            got,
            full[..got.len()],
            "第 {} 页之后中间缺了行",
            pages_without_end + 1
        );
        if seen.values().any(|id| id == "end:t-x") {
            break;
        }
        // 回合 t-x 已经在这一页的 turns 里（它的工具行被选中了），可结束行不在选中的那些行里：不建
        assert!(tl(&s).turns.contains_key("t-x"));
        pages_without_end += 1;
    }
    // 每页推进多少行取决于裁剪（离窗口远的一端丢），不定是 200；要守的是：带着结束位置的回合已经在结果里，
    // 结束行没被挑中的那几页都不建它
    assert!(
        pages_without_end >= 2,
        "至少两页没有结束行（实际 {pages_without_end}）"
    );
    // 翻到头
    while {
        look_at_bottom(&mut s);
        newer(&mut s, &w)
    } {
        seen.extend(keys(&s));
    }
    assert_eq!(seen.into_iter().collect::<Vec<_>>(), full);
}

fn pending_with(card: Option<pb::ResumeCard>) -> Item {
    support::snapshot(
        support::PENDING,
        1,
        vec![pb::QueryResult {
            seq: 1,
            kind: Some(pb::query_result::Kind::ListPendingItems(
                pb::ListPendingItemsResult {
                    items: card
                        .into_iter()
                        .map(|c| pb::PendingItem {
                            item: Some(pb::pending_item::Item::Resume(c)),
                        })
                        .collect(),
                },
            )),
        }],
    )
}

/// 继续 / 放弃卡片跟着已加载范围走：落在范围外时不进时间线；翻回它的位置时出现；被裁掉之后待处理表再更新，
/// 也不会被放回到没加载的位置上（方案 2.2 第 4 条）。
#[test]
fn resume_card_follows_loaded_range() {
    let mut w = World::new();
    w.push(f::user_message("q-r", CID, "先跑一半"));
    w.push(f::turn_queued("t-r", CID, f::IWORK));
    w.push(f::turn_state("t-r", T::Queued, T::Running, None));
    let interrupted_at = w.push(f::turn_state("t-r", T::Running, T::Interrupted, None));
    w.long_turn("x", 120, None);
    let card = pb::ResumeCard {
        turn_id: "t-r".into(),
        conversation_id: CID.into(),
        reason: pb::InterruptReason::Crash as i32,
        created_seq: interrupted_at,
    };
    let has_card = |s: &Store| tl(s).row("resume:t-r").is_some();

    let mut s = store(30);
    s.register_subscription(support::PENDING, ScopeKind::Pending);
    open(&mut s, &w);
    s.apply(pending_with(Some(card.clone())));
    assert!(tl(&s).has_older, "对照：最新页之前还有");
    assert!(!has_card(&s), "卡片的位置还没加载：不进时间线");
    // 往上翻到它的位置
    while !has_card(&s) {
        look_at_top(&mut s);
        assert!(older(&mut s, &w), "翻到头也没出现");
    }
    let k = tl(&s).key_of("resume:t-r").unwrap();
    assert_eq!(k.seq, interrupted_at, "回到原位");
    // 往下翻回最新：旧的一端（连同卡片）被裁掉
    while {
        look_at_bottom(&mut s);
        newer(&mut s, &w)
    } {}
    assert!(tl(&s).at_tail && tl(&s).has_older);
    assert!(!has_card(&s), "卡片随旧的一端裁掉");
    // 待处理表更新（卡片还在表里）：范围外，不放回去
    s.apply(pending_with(Some(card)));
    assert!(!has_card(&s));
    assert!(tl(&s).len() <= 30);
}

mod never_exceeds {
    use super::*;
    use proptest::prelude::*;

    #[derive(Debug, Clone)]
    enum Op {
        /// 实时到一条：工具调用或用户消息。
        Live(bool),
        Open,
        Older,
        Newer,
        /// 以第 n 行（取模）为中心加载。
        Around(usize),
        /// 可见窗口放在第 n 行（取模）上。
        Window(usize),
        Pending(bool),
    }

    fn op() -> impl Strategy<Value = Op> {
        prop_oneof![
            3 => any::<bool>().prop_map(Op::Live),
            1 => Just(Op::Open),
            3 => Just(Op::Older),
            3 => Just(Op::Newer),
            2 => any::<usize>().prop_map(Op::Around),
            3 => any::<usize>().prop_map(Op::Window),
            1 => any::<bool>().prop_map(Op::Pending),
        ]
    }

    proptest! {
        #![proptest_config(ProptestConfig { cases: 64, ..ProptestConfig::default() })]

        /// 随机的实时事件、各种查询结果、可见窗口位置：任何一步之后行数都不超上限（上限设成 40 逼出裁剪；R14）。
        #[test]
        fn timeline_never_exceeds_cap(ops in proptest::collection::vec(op(), 1..60)) {
            const CAP: usize = 40;
            let mut w = World::new();
            w.long_turn("a", 60, Some(30));
            w.push(f::user_message("q-live", CID, "再来"));
            w.push(f::turn_queued("t-live", CID, f::IWORK));
            w.push(f::turn_state("t-live", T::Queued, T::Running, None));
            let mut s = store(CAP);
            s.register_subscription(support::PENDING, ScopeKind::Pending);
            open(&mut s, &w);
            prop_assert!(tl(&s).len() <= CAP);
            let mut n = 0;
            for op in ops {
                match op.clone() {
                    Op::Live(tool) => {
                        n += 1;
                        let e = if tool {
                            f::tool_started("t-live", &format!("live-{n}"), "Bash", "ls")
                        } else {
                            f::user_message(&format!("live-q{n}"), CID, "又一句")
                        };
                        let seq = w.push(e.clone());
                        s.apply(support::persisted(seq, &[CONV], e.event.unwrap()));
                    }
                    Op::Open => {
                        open(&mut s, &w);
                    }
                    Op::Older => {
                        older(&mut s, &w);
                    }
                    Op::Newer => {
                        newer(&mut s, &w);
                    }
                    Op::Around(i) => {
                        let all = unlimited(&w);
                        let seq = all[i % all.len()].0.seq;
                        around(&mut s, &w, seq);
                    }
                    Op::Window(i) => {
                        let ids = tl(&s).identities();
                        if !ids.is_empty() {
                            let o = ids[i % ids.len()].0.ordinal();
                            s.set_window(CID, o, o + 6, i % 2 == 0);
                        }
                    }
                    Op::Pending(with) => {
                        s.apply(pending_with(with.then(|| pb::ResumeCard {
                            turn_id: "t-a".into(),
                            conversation_id: CID.into(),
                            reason: pb::InterruptReason::Crash as i32,
                            created_seq: 7,
                        })));
                    }
                }
                prop_assert!(tl(&s).len() <= CAP, "{:?} 之后 {} 行", op, tl(&s).len());
            }
        }
    }
}

// ---------- 连真假后台：会话层按边界挑游标（审查 5/F-3 的复现） ----------

#[cfg(unix)]
mod live {
    use super::*;
    use iwork_client_core::{App, DirectDialer, Options};
    use iwork_fake_daemon::{FakeDaemon, scenario, self_trusting_identity};
    use pb::ui_intent::Kind as I;
    use std::collections::BTreeSet;
    use std::time::Duration;
    use support::web::Web;
    use tokio::time::timeout;

    const WAIT: Duration = Duration::from_secs(30);
    const CAP: usize = 12;
    const C: &str = "c-rev";

    async fn intent(app: &App, kind: I) {
        let ack = app.intent(pb::UiIntent { kind: Some(kind) }).await;
        assert_eq!(ack.error, None);
    }

    async fn window(app: &App, first: i64, last: i64) {
        intent(
            app,
            I::SetVisibleRows(pb::SetVisibleRows {
                conversation_id: C.into(),
                first_ordinal: first,
                last_ordinal: last,
                by_user: false,
            }),
        )
        .await;
    }

    /// 取帧直到条件满足；每一帧之后网页上的行都不超上限，见过的行记进 `seen`。
    async fn pump(
        app: &App,
        web: &mut Web,
        seen: &mut BTreeSet<String>,
        what: &str,
        cond: impl Fn(&Web) -> bool,
    ) {
        let r = timeout(WAIT, async {
            while !cond(web) {
                if let Ok(Ok(c)) =
                    timeout(Duration::from_millis(300), app.next_frame(web.request())).await
                {
                    web.accept(&c);
                    assert!(
                        web.state.rows.len() <= CAP,
                        "网页上 {} 行，超过上限 {CAP}",
                        web.state.rows.len()
                    );
                    seen.extend(web.state.rows.keys().cloned());
                }
            }
        })
        .await;
        assert!(r.is_ok(), "等不到：{what}");
    }

    fn ordinals(web: &Web) -> (i64, i64) {
        let o: Vec<i64> = web.state.rows.values().map(|r| r.ordinal).collect();
        (*o.iter().min().unwrap(), *o.iter().max().unwrap())
    }

    /// 审查 5/F-3 的复现：上限 12；一条用户消息、一个回合近千次工具调用、正中间一个审批、最后一条助手消息。
    /// 跳到审批锚点后两端都没有消息行；往前翻到头、往后翻到头，见过的行 = 整个对话的全部行，任何一帧都不超上限。
    #[tokio::test(flavor = "multi_thread")]
    async fn reviewer_repro_long_tool_turn_pages_back_both_ways() {
        let dir = tempfile::Builder::new()
            .prefix("t09-rowcap")
            .tempdir_in("/tmp")
            .unwrap();
        let d = FakeDaemon::start(dir.path().join("d.sock")).await.unwrap();
        d.run_scenario("empty").await.unwrap();
        d.append(f::project_conversation(C, scenario::PROJECT));
        d.append(f::user_message("q", C, "全量跑一遍"));
        d.append(f::turn_queued("t", C, f::IWORK));
        d.append(f::turn_state("t", T::Queued, T::Running, None));
        for i in 0..996 {
            d.append(f::tool_started(
                "t",
                &format!("tc{i}"),
                "Bash",
                "cargo test",
            ));
            if i == 498 {
                d.append(f::approval(
                    "ap",
                    C,
                    "t",
                    f::command_action("rm -rf build"),
                    pb::ApprovalKind::Ask,
                    &[],
                ));
            }
        }
        d.append(f::assistant_message("a", C, "t", f::IWORK, "跑完了"));

        // 对照：整个对话的全部行（一次全取）
        let all: BTreeSet<String> = match d
            .query(&pb::Query {
                kind: Some(pb::query::Kind::GetConversation(pb::GetConversation {
                    conversation_id: C.into(),
                    limit: 10_000,
                    ..Default::default()
                })),
            })
            .kind
            .unwrap()
        {
            pb::query_result::Kind::GetConversation(r) => {
                let mut ids: BTreeSet<String> = r.messages.iter().map(|m| m.id.clone()).collect();
                ids.extend(
                    r.tool_calls
                        .iter()
                        .map(|t| format!("{}/{}", t.turn_id, t.tool_call_id)),
                );
                ids.extend(r.approvals.iter().map(|a| a.id.clone()));
                ids
            }
            other => panic!("{other:?}"),
        };
        assert!(all.len() > 990);

        let dialer = DirectDialer::new(d.endpoint(), self_trusting_identity().unwrap(), "test");
        let (app, effects) = App::start(
            dialer,
            Options {
                timeline_cap: CAP,
                ..Options::default()
            },
        );
        std::mem::forget(effects);
        let mut web = Web::new(app.attach().await.unwrap());
        intent(
            &app,
            I::SetVisible(pb::SetVisible {
                sidebar: true,
                conversation_id: Some(C.into()),
                ..Default::default()
            }),
        )
        .await;
        let mut seen = BTreeSet::new();
        pump(
            &app,
            &mut web,
            &mut seen,
            "等你处理那一处的锚点",
            |w| {
                w.state
                    .anchors
                    .as_ref()
                    .is_some_and(|a| a.items.iter().any(|i| i.kind() == pb::AnchorKind::Waiting))
            },
        )
        .await;
        let key = web
            .state
            .anchors
            .as_ref()
            .unwrap()
            .items
            .iter()
            .find(|i| i.kind() == pb::AnchorKind::Waiting)
            .unwrap()
            .key
            .clone();
        intent(
            &app,
            I::JumpToAnchor(pb::JumpToAnchor {
                conversation_id: C.into(),
                key,
            }),
        )
        .await;
        pump(&app, &mut web, &mut seen, "落到审批", |w| {
            w.state
                .scroll_to
                .as_ref()
                .is_some_and(|s| s.row_key == "ap")
        })
        .await;
        assert!(
            !web.state.rows.contains_key("q") && !web.state.rows.contains_key("a"),
            "对照：两端都没有消息行"
        );
        seen.clear();
        seen.extend(web.state.rows.keys().cloned());

        // 往前翻到头：网页一直顶着最旧一行
        let mut steps = 0;
        while web.state.window.as_ref().is_some_and(|w| w.has_older) {
            let (lo, _) = ordinals(&web);
            window(&app, lo, lo + 2).await;
            let before = lo;
            pump(&app, &mut web, &mut seen, "往前翻了一页", |w| {
                ordinals(w).0 < before || !w.state.window.as_ref().is_some_and(|x| x.has_older)
            })
            .await;
            steps += 1;
            assert!(steps < 500, "往前翻不到头");
        }
        assert!(seen.contains("q"), "翻回了第一条消息");
        // 往后翻到头：网页一直底着最新一行
        while !web.state.window.as_ref().is_some_and(|w| w.at_tail) {
            let (_, hi) = ordinals(&web);
            window(&app, hi - 2, hi).await;
            let before = hi;
            pump(&app, &mut web, &mut seen, "往后翻了一页", |w| {
                ordinals(w).1 > before || w.state.window.as_ref().is_some_and(|x| x.at_tail)
            })
            .await;
            steps += 1;
            assert!(steps < 1000, "往后翻不到头");
        }
        let entity: BTreeSet<String> = seen.into_iter().filter(|k| all.contains(k)).collect();
        assert_eq!(entity, all, "见过的行 = 整个对话的全部行");
    }
}

// ---------- 审查第 6 轮的复现（reviewer e036441c，收成正式用例） ----------

/// 6/F-3：「回到最新」重建成最新页时，不能沿用重建前网页声明的旧窗口当焦点（旧窗口指着已经不在的行），
/// 否则会把最新的一端裁掉、at_tail 变假，回到最新之后又不接着最新。
#[test]
fn latest_rebuild_reattaches_tail() {
    let mut w = World::new();
    let approval = w.long_turn("x", 200, Some(10)).unwrap();
    let mut s = store(40);
    open(&mut s, &w);
    around(&mut s, &w, approval);
    look_at_top(&mut s);
    assert!(!tl(&s).at_tail, "对照：在看旧的一段");
    s.apply(Item::ViewResult {
        purpose: QueryPurpose::LatestPage {
            conversation_id: CID.into(),
        },
        result: Box::new(w.get(pb::GetConversation {
            limit: PAGE_SIZE,
            ..Default::default()
        })),
    });
    assert!(
        tl(&s).at_tail,
        "回到最新之后没接着最新：{:?}",
        tl(&s).identities()
    );
    assert_eq!(tl(&s).edge_newer, None);
    // 旧窗口也不能留着：下一条实时事件之后照样接着最新，新消息进了时间线
    let seq = w.push(f::user_message("q-after", CID, "又一句"));
    s.apply(support::persisted(
        seq,
        &[CONV],
        f::user_message("q-after", CID, "又一句").event.unwrap(),
    ));
    assert!(
        tl(&s).at_tail && tl(&s).row("q-after").is_some(),
        "下一条事件后脱离了最新"
    );
}

/// 6/F-2：裁剪之后不能再派生出新的行（模型切换行）把行数顶回上限之上：任何一条事件之后都不超上限。
#[test]
fn model_note_trim_stays_under_cap() {
    let mut s = store(4);
    s.apply(support::snapshot(
        CONV,
        0,
        vec![support::page(0, vec![], false)],
    ));
    let events = vec![
        (1, f::with_model(f::turn_queued("old", CID, f::IWORK), "a")),
        (2, f::turn_state("old", T::Queued, T::Running, None)),
        (3, f::turn_state("old", T::Running, T::Interrupted, None)),
        (5, f::model_changed(CID, "b")),
        (8, f::user_message("q", CID, "go")),
        (10, f::with_model(f::turn_queued("new", CID, f::IWORK), "b")),
        (11, f::tool_started("new", "x", "Bash", "ls")),
        (12, f::tool_started("new", "y", "Bash", "ls")),
        (13, f::tool_started("new", "z", "Bash", "ls")),
    ];
    for (seq, e) in events {
        s.apply(support::persisted(seq, &[CONV], e.event.unwrap()));
        assert!(
            tl(&s).len() <= 4,
            "seq {seq} 之后 {} 行：{:?}",
            tl(&s).len(),
            tl(&s).identities()
        );
    }
}
