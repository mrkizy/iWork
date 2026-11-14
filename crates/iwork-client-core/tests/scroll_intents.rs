//! 一次性滚动意图的生命周期（锚点模块头的表；g3b 第 3 轮）：每个等待条件、每个作废原因一格，直接驱动 Store 与
//! 帧引擎（不靠对拍：对拍把一次性指令排除在外）。网页只执行带 token 的指令，这里看帧里发没发、发了几次。
//!
//! 只用 3a0a9ac 已有的接口写的格子能在改动前跑出红：等底两格、换代两格。等底期间用户滚动、首快照之后的快照、
//! 发送三格改动前也成立，是守护。

mod support;

use std::time::Instant;

use iwork_client_core::derive::Ctx;
use iwork_client_core::frame::{FRAME_INTERVAL, FrameEngine, Pull};
use iwork_client_core::store::{Item, QueryPurpose, ScopeKind, Store, Visible};
use iwork_proto::pb;
use support::web::Web;
use support::{CID, CONV};

fn ctx() -> Ctx {
    Ctx {
        now: std::time::SystemTime::UNIX_EPOCH,
    }
}

fn user_msg(id: &str, seq: u64) -> pb::Message {
    pb::Message {
        id: id.into(),
        conversation_id: CID.into(),
        created_seq: seq,
        author: Some(pb::Author {
            kind: Some(pb::author::Kind::User(pb::UserAuthor {})),
        }),
        status: pb::MessageStatus::Completed as i32,
        ..Default::default()
    }
}

fn user_anchor(seq: u64) -> pb::Anchor {
    pb::Anchor {
        created_seq: seq,
        at: None,
        kind: Some(pb::anchor::Kind::UserMessage(pb::UserMessageAnchor {
            message_id: format!("u{seq}"),
            excerpt: format!("第 {seq} 处"),
        })),
    }
}

/// 打开对话，还没有底（订阅没就绪、首份快照没到）：网页刚声明可见，紧接着就点了跳转（App 的真实顺序）。
/// 锚点窗口先到（3、5 两处）。
fn opened_without_base() -> Store {
    let mut s = Store::new();
    s.register_subscription(CONV, ScopeKind::Conversation(CID.into()));
    s.set_visible(Visible {
        sidebar: true,
        conversation_id: Some(CID.into()),
        ..Default::default()
    });
    s.apply(support::view_result(
        QueryPurpose::Anchors {
            conversation_id: CID.into(),
            center: 0,
            round: None,
        },
        pb::QueryResult {
            seq: 500,
            kind: Some(pb::query_result::Kind::ListAnchors(pb::ListAnchorsResult {
                anchors: vec![user_anchor(3), user_anchor(5)],
                ..Default::default()
            })),
        },
    ));
    s
}

/// 订阅就绪、首份快照到：带着这些消息。
fn first_snapshot(s: &mut Store, messages: Vec<pb::Message>) {
    s.apply(Item::Subscribed(pb::Subscribed {
        subscription_id: CONV,
        snapshot: Some(pb::Snapshot {
            results: vec![support::page(20, messages, true)],
        }),
        snapshot_seq: 20,
    }));
}

/// 以 `seq` 为中心的那一段到了（AroundPage）。
fn around_arrives(s: &mut Store, seq: u64) {
    let mut page = support::page(500, vec![user_msg(&format!("u{seq}"), seq)], true);
    if let Some(pb::query_result::Kind::GetConversation(g)) = page.kind.as_mut() {
        g.has_newer = true;
    }
    s.apply(support::view_result(
        QueryPurpose::AroundPage {
            conversation_id: CID.into(),
            seq,
        },
        page,
    ));
}

/// 网页那一侧：挂上、取帧；`sent` 记下每一帧里带的滚动指令（不是累计状态）。
struct Page {
    engine: FrameEngine,
    web: Web,
    now: Instant,
    sent: Vec<pb::ScrollTo>,
}

impl Page {
    fn attach(s: &mut Store) -> Self {
        let mut engine = FrameEngine::new(1 << 20);
        let web = Web::new(engine.attach(s));
        Self {
            engine,
            web,
            now: Instant::now(),
            sent: Vec::new(),
        }
    }

    /// 重新挂上（换代次，如网页重载）。
    fn reattach(&mut self, s: &mut Store) {
        self.web = Web::new(self.engine.attach(s));
    }

    fn drain(&mut self, s: &mut Store) {
        self.now += FRAME_INTERVAL;
        loop {
            let req = self.web.request();
            let applied = req.applied.map(|p| (p.frame, p.index));
            match self.engine.pull(s, req.epoch, applied, self.now, ctx()) {
                Pull::Chunk(c) => {
                    self.web.state.scroll_to = None;
                    self.web.accept(&c);
                    if let Some(x) = self.web.state.scroll_to.clone() {
                        self.sent.push(x);
                    }
                }
                Pull::Wait(_) | Pull::StaleEpoch => return,
            }
        }
    }

    fn sent_rows(&self) -> Vec<&str> {
        self.sent.iter().map(|x| x.row_key.as_str()).collect()
    }
}

// ---------- 等待条件 ----------

/// 等底，目标在首份快照里：首份快照是这次打开的底，不作废等着的跳转；底一到就发滚动（g3b 1/F-5）。
#[test]
fn waits_for_base_then_scrolls_when_the_row_is_in_it() {
    let mut s = opened_without_base();
    let mut p = Page::attach(&mut s);
    s.jump_to_anchor(CID, "3");
    p.drain(&mut s);
    assert_eq!(p.sent_rows(), Vec::<&str>::new(), "对照：还没有底，不发");
    first_snapshot(&mut s, vec![user_msg("u3", 3), user_msg("u5", 5)]);
    p.drain(&mut s);
    assert_eq!(p.sent_rows(), ["u3"]);
}

/// 等底，目标不在首份快照里：转等数据，以它为中心的那一段到了再发（g3b 1/F-5）。
#[test]
fn waits_for_base_then_for_the_segment_when_the_row_is_not_in_it() {
    let mut s = opened_without_base();
    let mut p = Page::attach(&mut s);
    s.jump_to_anchor(CID, "3");
    first_snapshot(&mut s, vec![user_msg("u5", 5)]);
    p.drain(&mut s);
    assert_eq!(p.sent_rows(), Vec::<&str>::new(), "对照：u3 不在这一页");
    around_arrives(&mut s, 3);
    p.drain(&mut s);
    assert_eq!(p.sent_rows(), ["u3"]);
}

// ---------- 作废原因 ----------

/// 首份快照到之前用户就滚了：作废，底到了也不再跳（守护）。
#[test]
fn user_scroll_before_the_base_cancels() {
    let mut s = opened_without_base();
    let mut p = Page::attach(&mut s);
    s.jump_to_anchor(CID, "3");
    s.set_window(CID, 0, 10, true);
    first_snapshot(&mut s, vec![user_msg("u3", 3)]);
    around_arrives(&mut s, 3);
    p.drain(&mut s);
    assert_eq!(p.sent_rows(), Vec::<&str>::new());
}

/// 首份快照之后又走了快照（重连、Q9）：回到最新，等着的跳转作废，迟到的那一段不再跳（守护）。
#[test]
fn a_later_snapshot_cancels() {
    let mut s = opened_without_base();
    let mut p = Page::attach(&mut s);
    first_snapshot(&mut s, vec![user_msg("u5", 5)]);
    s.jump_to_anchor(CID, "3");
    p.drain(&mut s);
    assert!(
        s.open.as_ref().unwrap().anchors.row.segment_seq().is_some(),
        "对照：在等那一段"
    );
    first_snapshot(&mut s, vec![user_msg("u5", 5)]);
    around_arrives(&mut s, 3);
    p.drain(&mut s);
    assert_eq!(p.sent_rows(), Vec::<&str>::new());
}

/// 网页换代（重载）时还在等那一段：作废，旧的 AroundPage 在新代次落地后不发滚动（g3b 2/F-2）。
#[test]
fn reattach_cancels_a_waiting_jump() {
    let mut s = opened_without_base();
    let mut p = Page::attach(&mut s);
    first_snapshot(&mut s, vec![user_msg("u5", 5)]);
    s.jump_to_anchor(CID, "3");
    p.drain(&mut s);
    p.reattach(&mut s);
    around_arrives(&mut s, 3);
    p.drain(&mut s);
    assert_eq!(p.sent_rows(), Vec::<&str>::new());
}

/// 网页换代时滚动指令已产生、还没取走：不再发，全量帧里也没有（g3b 2/F-2）。
#[test]
fn reattach_drops_an_unsent_scroll() {
    let mut s = opened_without_base();
    let mut p = Page::attach(&mut s);
    first_snapshot(&mut s, vec![user_msg("u3", 3), user_msg("u5", 5)]);
    p.drain(&mut s);
    s.jump_to_anchor(CID, "3");
    p.reattach(&mut s);
    p.drain(&mut s);
    p.drain(&mut s);
    assert_eq!(p.sent_rows(), Vec::<&str>::new());
}

// ---------- 发送 ----------

/// 发送：一条指令只在一帧里出现一次；之后的帧、重新挂上的全量帧都不重发。两次跳转的 token 不同（守护）。
#[test]
fn a_scroll_is_sent_once_with_a_fresh_token() {
    let mut s = opened_without_base();
    let mut p = Page::attach(&mut s);
    first_snapshot(&mut s, vec![user_msg("u3", 3), user_msg("u5", 5)]);
    p.drain(&mut s);
    s.jump_to_anchor(CID, "3");
    p.drain(&mut s);
    p.drain(&mut s);
    assert_eq!(p.sent_rows(), ["u3"], "只发一次");
    p.reattach(&mut s);
    p.drain(&mut s);
    assert_eq!(p.sent_rows(), ["u3"], "全量帧不重发");
    s.jump_to_anchor(CID, "5");
    p.drain(&mut s);
    assert_eq!(p.sent_rows(), ["u3", "u5"]);
    assert_ne!(p.sent[0].token, p.sent[1].token);
}

// ---------- 锚点列表的头尾（Home / End） ----------

fn based_with_window() -> Store {
    let mut s = opened_without_base();
    first_snapshot(&mut s, vec![user_msg("u5", 5)]);
    s
}
// 头尾几格不取帧（看视图本身），换代那一格才挂网页

fn edge_window_arrives(s: &mut Store, center: u64) {
    s.apply(support::view_result(
        QueryPurpose::Anchors {
            conversation_id: CID.into(),
            center,
            round: None,
        },
        pb::QueryResult {
            seq: 600,
            kind: Some(pb::query_result::Kind::ListAnchors(pb::ListAnchorsResult {
                anchors: vec![user_anchor(1), user_anchor(3)],
                has_after: true,
                ..Default::default()
            })),
        },
    ));
}

fn landing(s: &Store) -> (bool, Option<pb::EdgeLanding>) {
    let t = s.open.as_ref().unwrap();
    let v = iwork_client_core::derive::anchors::anchors(s, t, ctx()).unwrap();
    (v.edge_pending, v.edge_landing)
}

/// 头尾：等以那一头为中心的窗口，到了给一个落点；在等的时候网页知道在等（才发作废）。
#[test]
fn edge_waits_for_its_window_then_lands_once() {
    let mut s = based_with_window();
    s.anchor_browse_edge(CID, false);
    assert_eq!(landing(&s), (true, None), "在等");
    edge_window_arrives(&mut s, 0);
    assert_eq!(landing(&s), (true, None), "对照：别的中心的窗口不算");
    edge_window_arrives(&mut s, 1);
    let (pending, l) = landing(&s);
    assert!(!pending);
    assert!(l.is_some_and(|l| !l.latest && l.token > 0));
}

/// 头尾：用户在列表里滚动或按键就作废，迟到的窗口照常换上但不落点（g3b 2/F-1）。
#[test]
fn edge_is_cancelled_by_list_input() {
    let mut s = based_with_window();
    s.anchor_browse_edge(CID, false);
    s.anchor_list_input(CID);
    edge_window_arrives(&mut s, 1);
    assert_eq!(landing(&s), (false, None));
}

/// 头尾：换代与之后的快照都作废（落了点还没用的也清掉）。
#[test]
fn edge_is_cancelled_by_reattach_and_by_a_later_snapshot() {
    let mut s = based_with_window();
    let mut p = Page::attach(&mut s);
    s.anchor_browse_edge(CID, true);
    p.reattach(&mut s);
    edge_window_arrives(&mut s, 0);
    assert_eq!(landing(&s), (false, None), "换代");
    s.anchor_browse_edge(CID, true);
    edge_window_arrives(&mut s, 0);
    assert!(landing(&s).1.is_some(), "对照：落了点");
    first_snapshot(&mut s, vec![user_msg("u5", 5)]);
    assert_eq!(landing(&s), (false, None), "之后的快照");
}

// ---------- 跳行最多一个：新的替换旧的（g3b 3/F-3） ----------

/// 有底（首份快照只有 u5），锚点窗口 3、5 两处、后面还有。
fn based_with_more_after() -> Store {
    let mut s = opened_without_base();
    first_snapshot(&mut s, vec![user_msg("u5", 5)]);
    s.apply(support::view_result(
        QueryPurpose::Anchors {
            conversation_id: CID.into(),
            center: 0,
            round: None,
        },
        pb::QueryResult {
            seq: 700,
            kind: Some(pb::query_result::Kind::ListAnchors(pb::ListAnchorsResult {
                anchors: vec![user_anchor(3), user_anchor(5)],
                has_after: true,
                ..Default::default()
            })),
        },
    ));
    s
}

/// 旧的跳转在等 u3 那一段，新的相对跳转要等锚点窗口：旧的被替换，u3 那一段迟到也不滚。
#[test]
fn a_relative_jump_replaces_a_waiting_jump() {
    let mut s = based_with_more_after();
    let mut p = Page::attach(&mut s);
    s.jump_to_anchor(CID, "3");
    // 阅读位置在 u5（不是用户滚动的回报），⌥↓：后面那一处在窗口外、后面还有，要先重取窗口
    let ord = s.open.as_ref().unwrap().key_of("u5").unwrap().ordinal();
    s.set_window(CID, ord, ord, false);
    s.jump_relative(CID, 1);
    around_arrives(&mut s, 3);
    p.drain(&mut s);
    assert_eq!(p.sent_rows(), Vec::<&str>::new());
}

/// 已经产生、还没取走的 u5，改跳没载入的 u3：u5 不再发。
#[test]
fn a_new_jump_replaces_an_unsent_scroll() {
    let mut s = based_with_more_after();
    let mut p = Page::attach(&mut s);
    p.drain(&mut s);
    s.jump_to_anchor(CID, "5");
    s.jump_to_anchor(CID, "3");
    p.drain(&mut s);
    assert_eq!(
        p.sent_rows(),
        Vec::<&str>::new(),
        "u3 还在等那一段，u5 已被替换"
    );
    around_arrives(&mut s, 3);
    p.drain(&mut s);
    assert_eq!(p.sent_rows(), ["u3"]);
}

/// 已经产生、还没取走的滚动指令，用户自己滚动了：不再发。
#[test]
fn user_scroll_cancels_an_unsent_scroll() {
    let mut s = opened_without_base();
    let mut p = Page::attach(&mut s);
    first_snapshot(&mut s, vec![user_msg("u3", 3), user_msg("u5", 5)]);
    p.drain(&mut s);
    s.jump_to_anchor(CID, "3");
    s.set_window(CID, 0, 10, true);
    p.drain(&mut s);
    assert_eq!(p.sent_rows(), Vec::<&str>::new());
}
