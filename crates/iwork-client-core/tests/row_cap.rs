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
        self.get_in(CID, g)
    }

    fn get_in(&self, conversation_id: &str, g: pb::GetConversation) -> pb::QueryResult {
        self.t
            .query(
                self.head,
                &pb::Query {
                    kind: Some(pb::query::Kind::GetConversation(pb::GetConversation {
                        conversation_id: conversation_id.into(),
                        ..g
                    })),
                },
            )
            .expect("假后台拒了查询")
    }

    /// 一个回合：用户问一句，排队、运行，`tools` 次工具调用（第 `approval_at` 次后面夹一个审批），助手答一句，结束。
    /// 返回审批的 seq。
    fn long_turn(&mut self, n: &str, tools: usize, approval_at: Option<usize>) -> Option<u64> {
        self.long_turn_in(CID, n, tools, approval_at)
    }

    fn long_turn_in(
        &mut self,
        conv: &str,
        n: &str,
        tools: usize,
        approval_at: Option<usize>,
    ) -> Option<u64> {
        let turn = format!("t-{n}");
        self.push(f::user_message(&format!("q-{n}"), conv, "跑一遍"));
        self.push(f::turn_queued(&turn, conv, f::IWORK));
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
                    conv,
                    &turn,
                    f::command_action("rm -rf build"),
                    pb::ApprovalKind::Ask,
                    &[],
                )));
            }
        }
        self.push(f::assistant_message(
            &format!("a-{n}"),
            conv,
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

    // 旧的一端：上限 3，看最新一行。五行：10、20（分隔线 + 消息）、30（分隔线 + 消息）。丢掉 10 之后，20 的分隔线
    // 没了依据（上一条消息不在了），同一个循环里随即去掉，剩三行就停（6/F-2 的循环：同步派生行 → 查上限）。
    // 所以旧的一端，带分隔线的那一组一成为最外一组，分隔线就没了；成组丢在新的一端才看得出来（下一例）
    let mut s = store(3);
    s.apply(support::snapshot(CONV, 100, vec![page()]));
    assert_eq!(seqs(&s), [20, 30, 30], "{:?}", keys(&s));
    assert_eq!(tl(&s).edge_older, Some(20));

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
    // 待处理表里这一项没了（用户处理掉了）：卡片在已加载范围里也要消失；再回来时重新出现
    s.apply(pending_with(None));
    assert!(!has_card(&s), "待处理项移除后卡片消失");
    s.apply(pending_with(Some(card.clone())));
    assert!(has_card(&s), "待处理项回来，卡片回到原位");
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
        /// 换模型并留下当次说明，再开一个用新模型的回合（派生出模型切换行；之后的裁剪会碰到「正好丢掉说明」）。
        Model(bool),
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
            2 => any::<bool>().prop_map(Op::Model),
        ]
    }

    proptest! {
        #![proptest_config(ProptestConfig { cases: 64, ..ProptestConfig::default() })]

        /// 随机的实时事件（含换模型、当次说明）、各种查询结果、可见窗口位置：任何一步之后（同步完成之后，派生行也算）
        /// 行数都不超上限（上限设成 40 逼出裁剪；R14）。
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
                    Op::Model(a) => {
                        n += 1;
                        let model = if a { "fake-small" } else { "fake-large" };
                        let turn = format!("tm-{n}");
                        for e in [
                            f::model_changed(CID, model),
                            f::with_model(f::turn_queued(&turn, CID, f::IWORK), model),
                            f::tool_started(&turn, &format!("{turn}-c"), "Bash", "ls"),
                        ] {
                            let seq = w.push(e.clone());
                            s.apply(support::persisted(seq, &[CONV], e.event.unwrap()));
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
fn back_to_latest_ends_at_tail() {
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
    assert_eq!(s.window, None, "旧窗口作废");
    assert!(tl(&s).row("a-x").is_some(), "最新一行在");
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

/// 6/F-2（reviewer 的复现原样）：裁剪丢掉一条模型当次说明后，原来被它压着的模型切换行贴了出来；同步与裁剪在一个
/// 循环里，最后一步永远是查上限，所以任何一条事件之后都不超上限。
#[test]
fn derived_row_appearing_after_trim_is_trimmed_too() {
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

/// 6/F-1：对话之间穿插了别的事件，全局 seq 跳了一大段。往前翻时取回的一页必须留下、边界必须往前走，
/// 不能因为「离窗口远」把刚取回的整页又丢掉，翻页原地打转。
#[test]
fn older_paging_advances_across_sparse_seq() {
    let mut w = World::new();
    w.long_turn("old", 60, None);
    // 别的对话里的事件把全局 seq 消耗掉一大段
    w.head += 10_000;
    w.long_turn("new", 80, None);
    let mut s = store(40);
    open(&mut s, &w);
    for step in 0..50 {
        look_at_top(&mut s);
        let before = tl(&s).identities()[0].0.seq;
        if !older(&mut s, &w) {
            return;
        }
        let after = tl(&s).identities()[0].0.seq;
        assert!(
            after < before,
            "第 {step} 步没往前走：before={before}, after={after}, cursor={:?}, has_older={}",
            tl(&s).older_cursor(),
            tl(&s).has_older
        );
    }
    panic!("往前翻不到头");
}

/// 6/F-1 的另一个方向：往后翻同样必须前进。
#[test]
fn newer_paging_advances_across_sparse_seq() {
    let mut w = World::new();
    w.long_turn("old", 80, None);
    w.head += 10_000;
    w.long_turn("new", 60, None);
    let mut s = store(40);
    open(&mut s, &w);
    around(&mut s, &w, 3);
    for step in 0..50 {
        look_at_bottom(&mut s);
        let before = tl(&s).identities().last().unwrap().0.seq;
        if !newer(&mut s, &w) {
            return;
        }
        let after = tl(&s).identities().last().unwrap().0.seq;
        assert!(
            after > before,
            "第 {step} 步没往后走：before={before}, after={after}, cursor={:?}, at_tail={}",
            tl(&s).newer_cursor(),
            tl(&s).at_tail
        );
    }
    panic!("往后翻不到头");
}

// ---------- 方案第 7 节第 6 轮之后加的（2.2 第 4 条「丢哪一端」） ----------

/// 两段行之间隔一万个 seq（别的对话占掉的号），上限 40。往前一页页翻，每一步最旧一行的 seq 严格变小，直到
/// `has_older == false`；再往后翻，每一步最新一行严格变大，直到 `at_tail`。窗口声明过、没声明过各跑一遍。
#[test]
fn sparse_seq_paging_makes_strict_progress() {
    for declare_window in [true, false] {
        let mut w = World::new();
        w.long_turn("old", 70, None);
        w.head += 10_000;
        w.long_turn("new", 70, None);
        let full = unlimited(&w);
        let mut s = store(40);
        open(&mut s, &w);
        let mut seen: std::collections::BTreeMap<RowKey, String> = keys(&s).into_iter().collect();
        let mut steps = 0;
        loop {
            if declare_window {
                look_at_top(&mut s);
            }
            let before = keys(&s)[0].0.seq;
            if !older(&mut s, &w) {
                break;
            }
            let after = keys(&s)[0].0.seq;
            assert!(
                after < before,
                "窗口={declare_window} 第 {steps} 步往前没动：{before} → {after}"
            );
            seen.extend(keys(&s));
            steps += 1;
            assert!(steps <= full.len(), "往前翻不到头");
        }
        assert!(!tl(&s).has_older);
        loop {
            if declare_window {
                look_at_bottom(&mut s);
            }
            let before = keys(&s).last().unwrap().0.seq;
            if !newer(&mut s, &w) {
                break;
            }
            let after = keys(&s).last().unwrap().0.seq;
            assert!(
                after > before,
                "窗口={declare_window} 第 {steps} 步往后没动：{before} → {after}"
            );
            seen.extend(keys(&s));
            steps += 1;
            assert!(steps <= 2 * full.len(), "往后翻不到头");
        }
        assert!(tl(&s).at_tail);
        assert_eq!(
            seen.into_iter().collect::<Vec<_>>(),
            full,
            "窗口={declare_window}：不重、不漏"
        );
    }
}

mod terminates {
    use super::*;
    use proptest::prelude::*;

    proptest! {
        #![proptest_config(ProptestConfig { cases: 48, ..ProptestConfig::default() })]

        /// 随机的稀疏 seq（几段之间各隔一大段）、随机上限（不小于 2）、随机窗口（声明不声明、贴在哪一行）：往一个方向
        /// 翻，途中最外一行严格单调，步数不超过对话的行数就到头。
        #[test]
        fn paging_always_terminates(
            gaps in proptest::collection::vec(0u64..20_000, 1..4),
            tools in 5usize..60,
            cap in 2usize..60,
            window_at in proptest::option::of(0usize..10),
            older_first in any::<bool>(),
        ) {
            let mut w = World::new();
            for (i, gap) in gaps.iter().enumerate() {
                w.long_turn(&format!("s{i}"), tools, None);
                w.head += gap;
            }
            let total = unlimited(&w).len();
            let mut s = store(cap);
            open(&mut s, &w);
            if !older_first {
                // 先跳到最旧的一段，再往后翻
                let first = unlimited(&w)[0].0.seq;
                around(&mut s, &w, first);
            }
            let mut steps = 0;
            loop {
                if let Some(i) = window_at {
                    let ids = tl(&s).identities();
                    // 窗口贴着要翻的那一端（发起翻页的条件），离端点 i 行以内
                    let o = if older_first {
                        ids[0].0.ordinal()
                    } else {
                        ids[ids.len() - 1 - i.min(ids.len() - 1)].0.ordinal()
                    };
                    let (lo, hi) = if older_first { (o, ids[i.min(ids.len() - 1)].0.ordinal()) } else { (o, ids.last().unwrap().0.ordinal()) };
                    s.set_window(CID, lo, hi, false);
                }
                let edge = |s: &Store| if older_first { keys(s)[0].0.seq } else { keys(s).last().unwrap().0.seq };
                let before = edge(&s);
                let moved = if older_first { older(&mut s, &w) } else { newer(&mut s, &w) };
                if !moved {
                    break;
                }
                let after = edge(&s);
                let advanced = if older_first { after < before } else { after > before };
                prop_assert!(advanced, "第 {} 步没动：{} → {}", steps, before, after);
                prop_assert!(tl(&s).len() <= cap);
                steps += 1;
                prop_assert!(steps <= total, "{} 步还没到头（共 {} 行）", steps, total);
            }
            let reached_end = if older_first { !tl(&s).has_older } else { tl(&s).at_tail };
            prop_assert!(reached_end, "停下时没到头");
        }
    }
}

/// 订阅快照重建成最新页：锚是最新一组、旧窗口作废，重建之后一定接着最新。
#[test]
fn snapshot_rebuild_ends_at_tail() {
    let mut w = World::new();
    let approval = w.long_turn("x", 200, Some(10)).unwrap();
    let mut s = store(40);
    open(&mut s, &w);
    around(&mut s, &w, approval);
    look_at_top(&mut s);
    assert!(!tl(&s).at_tail, "对照：在看旧的一段");
    open(&mut s, &w);
    assert!(
        tl(&s).at_tail && tl(&s).row("a-x").is_some(),
        "{:?}",
        keys(&s)
    );
    assert_eq!(s.window, None, "旧窗口作废");
    assert!(tl(&s).len() <= 40);
}

/// 居中加载：旧窗口说的是旧时间线的行，作废；锚是目标行，留着。
#[test]
fn around_load_discards_old_window() {
    let mut w = World::new();
    let approval = w.long_turn("x", 200, Some(150)).unwrap();
    let mut s = store(40);
    open(&mut s, &w);
    look_at_bottom(&mut s);
    assert!(s.window.is_some(), "对照：声明过窗口");
    around(&mut s, &w, approval);
    assert_eq!(s.window, None, "旧窗口作废");
    assert!(tl(&s).row("ap-x").is_some(), "目标行留着");
    assert!(tl(&s).len() <= 40);
}

/// 结果到达时窗口已经离开那一端（中间隔着别的已加载的行），锚、中间的行和整个窗口一起装不下：不合并，时间线不动。
/// （会话层：结果到达时在飞表这一项已经清掉、`want_older` 也清了，窗口不贴着最旧一行就不会再发；贴回去才重发——
/// 发不发看的是 `Store::window_touches_oldest`，和这里判断合不合并是同一个条件。）
#[test]
fn stale_page_result_is_dropped_when_window_left_the_edge() {
    let mut w = World::new();
    w.long_turn("x", 300, None);
    let mut s = store(40);
    open(&mut s, &w);
    look_at_top(&mut s);
    // 会话按顶着最旧一行发出往前翻页，结果回来之前用户往下滚走了：窗口在最新那几行
    let before = tl(&s).older_cursor().unwrap();
    let page = w.get(match &before {
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
    });
    look_at_bottom(&mut s);
    assert!(!s.window_touches_oldest(), "对照：窗口已离开最旧一端");
    let rows_before = keys(&s);
    s.apply(Item::ViewResult {
        purpose: QueryPurpose::OlderPage {
            conversation_id: CID.into(),
            before,
        },
        result: Box::new(page),
    });
    assert_eq!(keys(&s), rows_before, "不合并，时间线不动");
    // 窗口贴回最旧一端：照常合并、往前走
    look_at_top(&mut s);
    assert!(s.window_touches_oldest());
    let oldest = keys(&s)[0].0.seq;
    assert!(older(&mut s, &w));
    assert!(keys(&s)[0].0.seq < oldest, "贴回去之后翻页往前走了");
}

/// 对照：窗口还贴着那一端时，哪怕锚加窗口已经超上限也合并；窗口远的那一侧被裁，边界前进。
#[test]
fn page_result_merges_while_window_touches_the_edge() {
    let mut w = World::new();
    w.long_turn("x", 300, None);
    let mut s = store(40);
    open(&mut s, &w);
    // 窗口很大：从最旧一行一直到最新一行，贴着最旧一端
    let first = keys(&s)[0].0.ordinal();
    let last = keys(&s).last().unwrap().0.ordinal();
    s.set_window(CID, first, last, false);
    let oldest = keys(&s)[0].0.seq;
    assert!(older(&mut s, &w));
    assert!(keys(&s)[0].0.seq < oldest, "合并了、边界往前走了");
    assert!(!tl(&s).at_tail, "新的一端被裁");
    assert!(tl(&s).len() <= 40);
}

/// 锚所在那一组永远不被裁（方案 2.2 第 4 条锚表，表驱动）：五种操作各一行，另加「窗口离锚很远」一行。
#[test]
fn anchor_group_is_never_trimmed() {
    use iwork_client_core::store::{PageMode, PageSide, Timeline, TrimPlan};
    // 一页 30 条消息，seq 10、20 … 300；上限 5
    let msgs = |range: std::ops::RangeInclusive<u64>| -> pb::GetConversationResult {
        match support::page(
            1000,
            range
                .map(|i| {
                    support::assistant_msg(
                        &format!("m{i}"),
                        i * 10,
                        pb::MessageStatus::Completed,
                        "x",
                    )
                })
                .collect(),
            true,
        )
        .kind
        .unwrap()
        {
            pb::query_result::Kind::GetConversation(g) => g,
            _ => unreachable!(),
        }
    };
    struct Case {
        name: &'static str,
        mode: PageMode,
        plan: TrimPlan,
        anchor_row: &'static str,
    }
    let ord = |seq: u64| RowKey::entity(seq).ordinal();
    let cases = [
        Case {
            name: "往前翻的结果：锚是这一页最新的一组",
            mode: PageMode::Older,
            plan: TrimPlan {
                anchor: Some(300),
                window: None,
                page: Some(PageSide::Older),
            },
            anchor_row: "m30",
        },
        Case {
            name: "往后翻的结果：锚是这一页最旧的一组",
            mode: PageMode::Newer,
            plan: TrimPlan {
                anchor: Some(10),
                window: None,
                page: Some(PageSide::Newer),
            },
            anchor_row: "m1",
        },
        Case {
            name: "居中加载：锚是目标行",
            mode: PageMode::Around { center: 150 },
            plan: TrimPlan {
                anchor: Some(150),
                window: None,
                page: None,
            },
            anchor_row: "m15",
        },
        Case {
            name: "重建成最新页：锚是最新一组",
            mode: PageMode::Snapshot,
            plan: TrimPlan {
                anchor: None,
                window: None,
                page: None,
            },
            anchor_row: "m30",
        },
        Case {
            name: "实时事件：锚是窗口里最新的一组",
            mode: PageMode::Snapshot,
            plan: TrimPlan {
                anchor: None,
                window: Some((ord(100), ord(120))),
                page: None,
            },
            anchor_row: "m12",
        },
        Case {
            name: "窗口离锚很远：锚照样留着",
            mode: PageMode::Older,
            plan: TrimPlan {
                anchor: Some(300),
                window: Some((ord(10), ord(20))),
                page: Some(PageSide::Older),
            },
            anchor_row: "m30",
        },
    ];
    for c in cases {
        let mut t = Timeline::with_cap(CID, 5);
        t.apply_page(&msgs(1..=30), c.mode);
        t.fit(c.plan, std::iter::empty());
        assert!(t.len() <= 5, "{}：{} 行", c.name, t.len());
        assert!(
            t.row(c.anchor_row).is_some(),
            "{}：锚 {} 被裁了，剩 {:?}",
            c.name,
            c.anchor_row,
            t.identities()
        );
    }
}

/// 到窗口的距离按已加载的行数数，不按 seq 相减（方案 2.2 第 4 条「丢哪一端」第 3 条）：旧的一端只隔 5 行、seq 却差
/// 一万；新的一端隔 27 行、seq 只差 27。按行数是新的一端远，丢新的一端（按 seq 相减会反过来丢旧的一端）。
#[test]
fn distance_counts_rows_not_seq() {
    use iwork_client_core::store::{PageMode, Timeline, TrimPlan};
    let msg =
        |i: u64| support::assistant_msg(&format!("m{i}"), i, pb::MessageStatus::Completed, "x");
    let mut all: Vec<pb::Message> = (1..=5).map(msg).collect();
    all.extend((10_001..=10_030).map(msg));
    let page = match support::page(20_000, all, false).kind.unwrap() {
        pb::query_result::Kind::GetConversation(g) => g,
        _ => unreachable!(),
    };
    let mut t = Timeline::with_cap(CID, 30);
    t.apply_page(&page, PageMode::Snapshot);
    assert_eq!(t.len(), 35, "对照：超了 5 行");
    // 窗口在第二段开头三行
    let window = Some((
        RowKey::entity(10_001).ordinal(),
        RowKey::entity(10_003).ordinal(),
    ));
    t.fit(
        TrimPlan {
            anchor: None,
            window,
            page: None,
        },
        std::iter::empty(),
    );
    assert_eq!(t.len(), 30);
    assert!(
        t.row("m1").is_some(),
        "旧的一端（只隔 5 行）留着：{:?}",
        t.identities()
    );
    assert!(t.row("m10030").is_none(), "新的一端（隔 27 行）丢了");
    assert!(!t.at_tail);
}

/// 没有网页窗口时，拿锚这一组当窗口（审查 7/F-2）：窗口只覆盖这一组的合法行号，不能把 BTreeMap 取同组用的
/// slot 哨兵当成行号（那样窗口会延伸到锚之后 127 个 seq，裁剪偏向新的一端）。九条连续的 seq、上限 5：
/// 锚在中间两端交替裁，锚靠近哪端就留哪端。
#[test]
fn no_window_trims_around_anchor_group() {
    use iwork_client_core::store::{PageMode, Timeline, TrimPlan};
    let kept = |anchor: u64| {
        let msgs = (1..=9)
            .map(|i| support::assistant_msg(&format!("m{i}"), i, pb::MessageStatus::Completed, "x"))
            .collect();
        let page = match support::page(100, msgs, false).kind.unwrap() {
            pb::query_result::Kind::GetConversation(g) => g,
            _ => unreachable!(),
        };
        let mut t = Timeline::with_cap(CID, 5);
        t.apply_page(&page, PageMode::Around { center: anchor });
        t.fit(
            TrimPlan {
                anchor: Some(anchor),
                window: None,
                page: None,
            },
            std::iter::empty(),
        );
        t.identities()
            .iter()
            .map(|(k, _)| k.seq)
            .collect::<Vec<_>>()
    };
    assert_eq!(kept(5), vec![3, 4, 5, 6, 7], "锚在中间：两边各留两条");
    assert_eq!(kept(2), vec![1, 2, 3, 4, 5], "锚靠旧端：丢新的一端");
    assert_eq!(kept(8), vec![5, 6, 7, 8, 9], "锚靠新端：丢旧的一端");
}

/// 切到别的对话后，上一个对话的居中加载 / 最新页结果迟到（审查 7/F-4）：不合并，也不能动当前对话的窗口。
fn late_other_conversation_page_keeps_window(purpose: impl Fn() -> QueryPurpose) {
    let mut w = World::new();
    let approval = w.long_turn("x", 200, Some(10)).unwrap();
    let mut s = store(40);
    open(&mut s, &w);
    around(&mut s, &w, approval);
    look_at_top(&mut s);
    let before = s.window;
    let rows = keys(&s);
    assert!(
        before.is_some() && !tl(&s).at_tail,
        "对照：在读旧消息、有窗口"
    );
    let mut old = w.get(pb::GetConversation {
        limit: PAGE_SIZE,
        ..Default::default()
    });
    match old.kind.as_mut() {
        Some(pb::query_result::Kind::GetConversation(page)) => {
            page.conversation.as_mut().unwrap().id = "previous-conversation".into();
        }
        _ => panic!("expected conversation query"),
    }
    s.apply(Item::ViewResult {
        purpose: purpose(),
        result: Box::new(old),
    });
    assert_eq!(keys(&s), rows, "上一个对话的页没有合并进来");
    assert_eq!(s.window, before, "当前对话的窗口不动");
    assert!(s.back_to_latest.is_none(), "也不触发回到最新");
}

#[test]
fn late_other_conversation_around_keeps_window() {
    late_other_conversation_page_keeps_window(|| QueryPurpose::AroundPage {
        conversation_id: "previous-conversation".into(),
        seq: 5,
    });
}

#[test]
fn late_other_conversation_latest_keeps_window() {
    late_other_conversation_page_keeps_window(|| QueryPurpose::LatestPage {
        conversation_id: "previous-conversation".into(),
    });
}

// ---------- 随机交错（第 8 轮）：两个对话、迟到的结果、重连、窗口上报 ----------

/// 随机交错（T09 2b 第 8 轮）：把实时事件（各类持久事件，含派生行触发）、往前 / 往后翻页、居中跳转、回到最新、
/// 断线重连与订阅快照、切换对话及另一对话迟到的结果（居中、最新页、行级翻页、快照、实时事件）、网页窗口上报（含
/// 首尾不变的重报）、待处理表更新任意混合，每一步之后查全部不变量，最后往两端翻到头、拼回全集。
///
/// 照会话层的做法模拟：查询发出时定下用途与参数（游标、中心），送达时才向假后台要结果（真后台也是收到才算；本地
/// 操作可以插在发出与送达之间，事件不会跑到结果后面）。断线丢掉在途的查询，重新订阅拿快照；切换对话退订旧的、
/// 用新编号订阅新的。网页侧「reset 后重报窗口」由 ConversationReset.test.tsx 单独盖住，这里只按操作上报。
mod interleave {
    use super::*;
    use proptest::prelude::*;
    use std::collections::{BTreeMap, BTreeSet};

    const A: &str = CID;
    const B: &str = "c2";

    fn tag(conv: &str) -> &'static str {
        if conv == A { "A." } else { "B." }
    }
    fn other(conv: &'static str) -> &'static str {
        if conv == A { B } else { A }
    }

    /// 在途的一件事：送达时才算结果。
    #[derive(Debug, Clone)]
    enum Sent {
        Snapshot {
            sub: u32,
        },
        Older(PageCursor),
        Newer(PageCursor),
        Around(u64),
        /// 最新页。`back`：回到最新（会话层只在不接着最新时发，结果整条重建成最新页）；否则是检查点跟进（随时会发，
        /// 结果照常合并、按窗口裁）。`reports`：发出时网页报过几次窗口，送达前又报了（用户又滚走了）就不要求接着最新。
        Latest {
            back: bool,
            reports: usize,
        },
    }

    #[derive(Debug, Clone)]
    enum Op {
        /// 实时事件：`other` 为真时发给没打开的那个对话（用它旧的订阅号，模拟退订后迟到的事件）。
        Live {
            other: bool,
            kind: u8,
        },
        Older,
        Newer,
        Around(usize),
        Latest,
        Checkpoint,
        /// 送达在途的第 n 件（取模）：乱序、迟到都由它产生。
        Deliver(usize),
        DeliverAll,
        Reconnect,
        Switch,
        Window(usize, bool),
        /// 网页按上一次的首尾原样再报一次。
        Rereport,
        Pending(bool),
    }

    fn op() -> impl Strategy<Value = Op> {
        prop_oneof![
            4 => (any::<bool>(), 0u8..6).prop_map(|(other, kind)| Op::Live { other, kind }),
            3 => Just(Op::Older),
            3 => Just(Op::Newer),
            2 => any::<usize>().prop_map(Op::Around),
            1 => Just(Op::Latest),
            1 => Just(Op::Checkpoint),
            5 => any::<usize>().prop_map(Op::Deliver),
            1 => Just(Op::DeliverAll),
            1 => Just(Op::Reconnect),
            1 => Just(Op::Switch),
            3 => (any::<usize>(), any::<bool>()).prop_map(|(i, u)| Op::Window(i, u)),
            1 => Just(Op::Rereport),
            1 => any::<bool>().prop_map(Op::Pending),
        ]
    }

    struct H {
        w: World,
        s: Store,
        cap: usize,
        open: &'static str,
        /// 每个对话最近一次用的订阅号（退订以后迟到的事件还带着它）。
        sub: BTreeMap<&'static str, u32>,
        next_sub: u32,
        queue: Vec<(&'static str, Sent)>,
        last_report: Option<(i64, i64)>,
        reports: usize,
        /// 每个对话正在跑的回合。
        live_turn: BTreeMap<&'static str, String>,
        card: Option<pb::ResumeCard>,
        /// A 里中断回合的位置（继续卡片的 created_seq）。
        interrupted_at: u64,
        n: usize,
    }

    impl H {
        fn new(cap: usize, gaps: &[u64]) -> Self {
            let mut w = World::new();
            w.push(f::project_conversation(B, support::PID));
            // 两个对话的内容交错写进同一条事件日志：各自的 seq 都是稀疏的
            for (i, gap) in gaps.iter().enumerate() {
                w.long_turn_in(A, &format!("A.{i}"), 30, Some(12));
                w.long_turn_in(B, &format!("B.{i}"), 20, Some(5));
                w.head += gap;
            }
            // A 里一个中断的回合（继续卡片指着它）
            w.push(f::user_message("q-A.int", A, "跑一半"));
            w.push(f::turn_queued("t-A.int", A, f::IWORK));
            w.push(f::turn_state("t-A.int", T::Queued, T::Running, None));
            let interrupted_at = w.push(f::turn_state("t-A.int", T::Running, T::Interrupted, None));
            let mut live_turn = BTreeMap::new();
            for conv in [A, B] {
                let turn = format!("t-{}live0", tag(conv));
                w.push(f::user_message(
                    &format!("q-{}live0", tag(conv)),
                    conv,
                    "再来",
                ));
                w.push(f::turn_queued(&turn, conv, f::IWORK));
                w.push(f::turn_state(&turn, T::Queued, T::Running, None));
                live_turn.insert(conv, turn);
            }
            let mut s = Store::with_timeline_cap(cap);
            s.register_subscription(support::PENDING, ScopeKind::Pending);
            let mut h = H {
                w,
                s,
                cap,
                open: A,
                sub: BTreeMap::new(),
                next_sub: 10,
                queue: Vec::new(),
                last_report: None,
                reports: 0,
                live_turn,
                card: None,
                interrupted_at,
                n: 0,
            };
            h.subscribe(A);
            h.deliver_all();
            h
        }

        fn subscribe(&mut self, conv: &'static str) {
            self.s.set_visible(Visible {
                conversation_id: Some(conv.into()),
                ..Default::default()
            });
            let id = self.next_sub;
            self.next_sub += 1;
            self.s
                .register_subscription(id, ScopeKind::Conversation(conv.into()));
            self.sub.insert(conv, id);
            self.open = conv;
            self.last_report = None;
            self.queue.push((conv, Sent::Snapshot { sub: id }));
        }

        fn timeline(&self) -> Option<&Timeline> {
            self.s.open.as_ref().filter(|t| !t.identities().is_empty())
        }

        /// 不设上限、同样的待处理表时一次全取的行（对照）。
        fn full(&self, conv: &str) -> Vec<(RowKey, String)> {
            let mut s = Store::with_timeline_cap(100_000);
            s.register_subscription(support::PENDING, ScopeKind::Pending);
            s.register_subscription(1, ScopeKind::Conversation(conv.into()));
            s.set_visible(Visible {
                conversation_id: Some(conv.into()),
                ..Default::default()
            });
            let page = self.w.get_in(
                conv,
                pb::GetConversation {
                    limit: 100_000,
                    ..Default::default()
                },
            );
            s.apply(support::snapshot(1, self.w.head, vec![page]));
            s.apply(pending_with(self.card.clone()));
            s.open.as_ref().unwrap().identities()
        }

        fn live(&mut self, to_other: bool, kind: u8) {
            let conv = if to_other {
                other(self.open)
            } else {
                self.open
            };
            self.n += 1;
            let n = self.n;
            let p = tag(conv);
            let turn = self.live_turn[conv].clone();
            let events = match kind {
                0 => vec![f::user_message(&format!("q-{p}l{n}"), conv, "又一句")],
                1 => vec![f::tool_started(&turn, &format!("{p}l{n}"), "Bash", "ls")],
                2 => vec![f::approval(
                    &format!("ap-{p}l{n}"),
                    conv,
                    &turn,
                    f::command_action("rm -rf build"),
                    pb::ApprovalKind::Ask,
                    &[],
                )],
                3 => {
                    // 换模型再开一个用新模型的回合：派生出模型切换行
                    let model = if n.is_multiple_of(2) {
                        "fake-small"
                    } else {
                        "fake-large"
                    };
                    let next = format!("t-{p}m{n}");
                    let events = vec![
                        f::turn_finished(&turn, T::Completed, false, None),
                        f::model_changed(conv, model),
                        f::with_model(f::turn_queued(&next, conv, f::IWORK), model),
                        f::turn_state(&next, T::Queued, T::Running, None),
                        f::tool_started(&next, &format!("{p}m{n}-c"), "Bash", "ls"),
                    ];
                    self.live_turn.insert(conv, next);
                    events
                }
                4 => {
                    // 回合结束（回合结束行），接着开下一个
                    let next = format!("t-{p}r{n}");
                    let events = vec![
                        f::turn_finished(&turn, T::Completed, false, None),
                        f::user_message(&format!("q-{p}r{n}"), conv, "下一轮"),
                        f::turn_queued(&next, conv, f::IWORK),
                        f::turn_state(&next, T::Queued, T::Running, None),
                    ];
                    self.live_turn.insert(conv, next);
                    events
                }
                _ => vec![
                    f::assistant_message(&format!("a-{p}l{n}"), conv, &turn, f::IWORK, "好"),
                    f::message_completed(conv, &format!("a-{p}l{n}"), "好了"),
                ],
            };
            for e in events {
                let seq = self.w.push(e.clone());
                // 打开着的对话按当前订阅号收；另一个对话的事件带着它退订前的订阅号迟到（从没订阅过就不发）
                if let Some(&sub) = self.sub.get(conv) {
                    self.s
                        .apply(support::persisted(seq, &[sub], e.event.unwrap()));
                }
            }
        }

        fn issue_older(&mut self) {
            if let Some(c) = self
                .timeline()
                .filter(|t| t.has_older)
                .and_then(Timeline::older_cursor)
            {
                self.queue.push((self.open, Sent::Older(c)));
            }
        }

        fn issue_newer(&mut self) {
            if let Some(c) = self
                .timeline()
                .filter(|t| !t.at_tail)
                .and_then(Timeline::newer_cursor)
            {
                self.queue.push((self.open, Sent::Newer(c)));
            }
        }

        fn issue_around(&mut self, i: usize) {
            let all = self.full(self.open);
            let seq = all[i % all.len()].0.seq;
            self.queue.push((self.open, Sent::Around(seq)));
        }

        fn result(&self, conv: &str, sent: &Sent) -> Item {
            let get = |g| self.w.get_in(conv, g);
            let view = |purpose, g| Item::ViewResult {
                purpose,
                result: Box::new(get(g)),
            };
            match sent {
                Sent::Snapshot { sub } => support::snapshot(
                    *sub,
                    self.w.head,
                    vec![get(pb::GetConversation {
                        limit: PAGE_SIZE,
                        ..Default::default()
                    })],
                ),
                Sent::Older(c) => view(
                    QueryPurpose::OlderPage {
                        conversation_id: conv.into(),
                        before: c.clone(),
                    },
                    match c {
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
                    },
                ),
                Sent::Newer(c) => view(
                    QueryPurpose::NewerPage {
                        conversation_id: conv.into(),
                        after: c.clone(),
                    },
                    match c {
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
                    },
                ),
                Sent::Around(seq) => view(
                    QueryPurpose::AroundPage {
                        conversation_id: conv.into(),
                        seq: *seq,
                    },
                    pb::GetConversation {
                        around_seq: Some(*seq),
                        limit: 50,
                        ..Default::default()
                    },
                ),
                Sent::Latest { .. } => view(
                    QueryPurpose::LatestPage {
                        conversation_id: conv.into(),
                    },
                    pb::GetConversation {
                        limit: PAGE_SIZE,
                        ..Default::default()
                    },
                ),
            }
        }

        /// 送达一件在途的事，并查和这件事有关的不变量。
        fn deliver(&mut self, i: usize) -> Result<(), TestCaseError> {
            if self.queue.is_empty() {
                return Ok(());
            }
            let (conv, sent) = self.queue.remove(i % self.queue.len());
            let item = self.result(conv, &sent);
            // 快照只认还登记着的订阅号（退订了的、断线前的会话层已经不认）
            let current = match &sent {
                Sent::Snapshot { sub } => self.sub.get(conv) == Some(sub) && conv == self.open,
                _ => conv == self.open,
            };
            let before = (
                self.s.window,
                keys_of(&self.s),
                self.s.back_to_latest.clone(),
            );
            let was_tail = self.s.open.as_ref().is_some_and(|t| t.at_tail);
            self.s.apply(item);
            if !current {
                // 别的对话（或作废的订阅）的结果迟到：不合并，也不动当前对话的窗口与回到最新
                prop_assert_eq!(
                    (
                        self.s.window,
                        keys_of(&self.s),
                        self.s.back_to_latest.clone()
                    ),
                    before,
                    "{:?} 的 {:?} 迟到，动了当前对话（{}）",
                    conv,
                    sent,
                    self.open
                );
                return Ok(());
            }
            let Some(t) = self.timeline() else {
                return Ok(());
            };
            match sent {
                Sent::Snapshot { .. } => {
                    prop_assert!(t.at_tail, "快照之后没接着最新：{:?}", t.identities());
                }
                Sent::Latest {
                    back: true,
                    reports,
                } if !was_tail && reports == self.reports => {
                    prop_assert!(t.at_tail, "回到最新之后没接着最新：{:?}", t.identities());
                }
                Sent::Around(center) => {
                    prop_assert!(
                        t.identities().iter().any(|(k, _)| k.seq == center),
                        "居中加载的锚组 {} 被裁掉了：{:?}",
                        center,
                        t.identities()
                    );
                }
                _ => {}
            }
            Ok(())
        }

        fn deliver_all(&mut self) {
            while !self.queue.is_empty() {
                let _ = self.deliver(0);
            }
        }

        fn window(&mut self, i: usize, by_user: bool) {
            let ids = match self.timeline() {
                Some(t) => t.identities(),
                None => return,
            };
            let o = ids[i % ids.len()].0.ordinal();
            self.report(o, o + 6, by_user);
        }

        fn report(&mut self, first: i64, last: i64, by_user: bool) {
            self.s.set_window(self.open, first, last, by_user);
            self.last_report = Some((first, last));
            self.reports += 1;
        }

        /// 每一步之后都成立的。
        fn check(&self, op: &Op) -> Result<(), TestCaseError> {
            let Some(t) = self.s.open.as_ref() else {
                return Ok(());
            };
            prop_assert_eq!(t.conversation_id.as_str(), self.open);
            prop_assert!(
                t.len() <= self.cap,
                "{:?} 之后 {} 行，超过上限 {}",
                op,
                t.len(),
                self.cap
            );
            let foreign = tag(other(self.open));
            let leaked: Vec<_> = t
                .identities()
                .into_iter()
                .filter(|(_, id)| id.contains(foreign))
                .collect();
            prop_assert!(
                leaked.is_empty(),
                "{:?} 之后串进了另一个对话的行：{:?}",
                op,
                leaked
            );
            Ok(())
        }

        /// 最后：在途的都送达，往前翻到头、再往后翻到头（网页一直贴着要翻的那一端，会话层发翻页的条件），每一步
        /// 最外一行严格前进；一路见过的行拼起来等于全集，不重不漏。
        fn page_through(&mut self) -> Result<(), TestCaseError> {
            self.deliver_all();
            let full = self.full(self.open);
            let mut seen: BTreeMap<RowKey, String> = keys_of(&self.s).into_iter().collect();
            let mut steps = 0;
            for older_side in [true, false] {
                loop {
                    let ids = keys_of(&self.s);
                    let (first, last) = (ids[0].0.ordinal(), ids[ids.len() - 1].0.ordinal());
                    if older_side {
                        self.report(first, first + 4, false);
                        self.issue_older();
                    } else {
                        self.report(last - 4, last, false);
                        self.issue_newer();
                    }
                    if self.queue.is_empty() {
                        break;
                    }
                    let edge = |ids: &[(RowKey, String)]| {
                        if older_side {
                            ids[0].0.seq
                        } else {
                            ids[ids.len() - 1].0.seq
                        }
                    };
                    let before = edge(&ids);
                    self.deliver(0)?;

                    let after = edge(&keys_of(&self.s));
                    let advanced = if older_side {
                        after < before
                    } else {
                        after > before
                    };
                    prop_assert!(
                        advanced,
                        "翻页第 {} 步没前进：{} → {}",
                        steps,
                        before,
                        after
                    );
                    prop_assert!(self.s.open.as_ref().unwrap().len() <= self.cap);
                    seen.extend(keys_of(&self.s));
                    steps += 1;
                    prop_assert!(steps <= 2 * full.len() + 4, "翻不到头");
                }
            }
            let t = self.s.open.as_ref().unwrap();
            prop_assert!(!t.has_older || t.at_tail, "停下时没到头");
            prop_assert!(t.at_tail, "往后翻完没接回最新");
            // 当次说明（note:）只随实时事件出现、重新取页不再带出（和 properties.rs 的 keys_without_notes 同一口径）。
            // 两个回合之间有模型当次说明时不贴模型切换行（方案 2.2，`model_switch_row`）：实时看到的是说明，一次全取
            // 看到的是切换行，两者算同一处。本测试里换模型事件紧挨在新回合排队之前，说明的 seq = 切换行的 seq − 1。
            let notes: BTreeSet<u64> = seen
                .iter()
                .filter(|(_, id)| id.starts_with("note:"))
                .map(|(k, _)| k.seq)
                .collect();
            let seen: Vec<_> = seen
                .into_iter()
                .filter(|(_, id)| !id.starts_with("note:"))
                .collect();
            let full: Vec<_> = full
                .into_iter()
                .filter(|(k, id)| {
                    !(id.starts_with("model:") && notes.contains(&(k.seq - 1)))
                        || seen.iter().any(|(_, s)| s == id)
                })
                .collect();
            if seen != full {
                let a: BTreeSet<_> = seen.iter().collect();
                let b: BTreeSet<_> = full.iter().collect();
                prop_assert!(
                    false,
                    "拼出来的不等于全集：多了 {:?}，少了 {:?}",
                    a.difference(&b).collect::<Vec<_>>(),
                    b.difference(&a).collect::<Vec<_>>()
                );
            }
            Ok(())
        }
    }

    fn keys_of(s: &Store) -> Vec<(RowKey, String)> {
        s.open
            .as_ref()
            .map(Timeline::identities)
            .unwrap_or_default()
    }

    fn run(cap: usize, gaps: Vec<u64>, ops: Vec<Op>) -> Result<(), TestCaseError> {
        let mut h = H::new(cap, &gaps);
        for op in &ops {
            match op.clone() {
                Op::Live { other, kind } => h.live(other, kind),
                Op::Older => h.issue_older(),
                Op::Newer => h.issue_newer(),
                Op::Around(i) => h.issue_around(i),
                Op::Latest => {
                    if h.timeline().is_some_and(|t| !t.at_tail) {
                        let sent = Sent::Latest {
                            back: true,
                            reports: h.reports,
                        };
                        h.queue.push((h.open, sent));
                    }
                }
                Op::Checkpoint => {
                    let sent = Sent::Latest {
                        back: false,
                        reports: h.reports,
                    };
                    h.queue.push((h.open, sent));
                }
                Op::Deliver(i) => h.deliver(i)?,
                Op::DeliverAll => {
                    while !h.queue.is_empty() {
                        h.deliver(0)?;
                        h.check(op)?;
                    }
                }
                Op::Reconnect => {
                    // 断线：在途的查询丢了；重新订阅（同一个编号），拿新快照
                    h.queue.clear();
                    h.s.resubscribing();
                    let sub = h.sub[h.open];
                    h.queue.push((h.open, Sent::Snapshot { sub }));
                }
                Op::Switch => {
                    let old = h.open;
                    h.s.unregister_subscription(h.sub[old]);
                    h.subscribe(other(old));
                }
                Op::Window(i, by_user) => h.window(i, by_user),
                Op::Rereport => {
                    if let Some((first, last)) = h.last_report {
                        h.report(first, last, false);
                    }
                }
                Op::Pending(with) => {
                    h.card = with.then(|| pb::ResumeCard {
                        turn_id: "t-A.int".into(),
                        conversation_id: A.into(),
                        reason: pb::InterruptReason::Crash as i32,
                        created_seq: h.interrupted_at,
                    });
                    h.s.apply(pending_with(h.card.clone()));
                }
            }
            h.check(op)?;
        }
        h.page_through()
    }

    // 缩出来的反例（在旧版本上跑时撞出的），固定下来。
    fn pinned(cap: usize, ops: Vec<Op>) {
        if let Err(e) = run(cap, vec![0], ops) {
            panic!("{e}");
        }
    }
    use Op::*;
    const fn user(other: bool) -> Op {
        Live { other, kind: 0 }
    }

    /// 检查点最新页从对话开头一直带到最新（has_more=false）、按窗口合并进来：旧的一端已经到头，往前翻不该再发一次
    /// 空查询（has_older 留着旧值时会发，第一步不前进）。
    #[test]
    fn pinned_checkpoint_page_reaching_start_clears_has_older() {
        pinned(34, vec![Checkpoint, Window(10411622911427216448, false)]);
    }

    /// fefcce0：一条实时用户消息之后往前翻，第一步没前进（6/F 一类）。
    #[test]
    fn pinned_fefcce0_paging_stalls() {
        pinned(4, vec![user(false)]);
    }

    /// fefcce0：网页窗口在旧处、实时消息挤掉新的一端，回到最新的重建没回到最新（6/F-3）。
    #[test]
    fn pinned_fefcce0_back_to_latest_keeps_old_window() {
        pinned(
            7,
            vec![
                Window(3609487704107818428, false),
                user(false),
                Latest,
                Deliver(0),
            ],
        );
    }

    /// a469cff：A 的居中加载在途时切到 B，B 报了窗口，A 的结果迟到（7/F-4）。
    #[test]
    fn pinned_a469cff_late_around_from_previous_conversation() {
        pinned(
            31,
            vec![
                user(false),
                user(false),
                Deliver(801907787347),
                Around(1485811311852449722),
                Switch,
                user(false),
                Window(13143954130666040881, true),
                Deliver(11223925740640785348),
            ],
        );
    }

    /// 换模型以后往两端翻完：实时看到的当次说明和一次全取看到的模型切换行算同一处（比对口径，不是产品缺陷）。
    #[test]
    fn pinned_model_note_matches_switch_row() {
        pinned(
            39,
            vec![
                user(true),
                user(true),
                user(false),
                Live {
                    other: false,
                    kind: 3,
                },
                user(false),
                user(false),
            ],
        );
    }

    proptest! {
        // 默认 64 例随 cargo test 跑；完整一轮 `PROPTEST_CASES=2000 lockf /tmp/iwork-heavy-test.lock cargo test -p
        // iwork-client-core --test row_cap interleave`
        #![proptest_config(ProptestConfig {
            cases: std::env::var("PROPTEST_CASES").ok().and_then(|v| v.parse().ok()).unwrap_or(64),
            ..ProptestConfig::default()
        })]

        #[test]
        fn interleaved_ops_keep_invariants(
            cap in 4usize..60,
            gaps in proptest::collection::vec(0u64..5_000, 1..4),
            ops in proptest::collection::vec(op(), 1..80),
        ) {
            run(cap, gaps, ops)?;
        }
    }
}
