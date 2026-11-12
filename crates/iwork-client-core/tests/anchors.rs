//! 消息锚点（T09 方案 2.9，第 7 节「锚点」一组）。连假后台的走真协议；状态机的单测在 `store::anchors` 里。

#![cfg(unix)]

mod support;

use std::time::{Duration, Instant};

use iwork_client_core::derive::anchors as derive_anchors;
use iwork_client_core::store::{QueryPurpose, SummaryTable};
use iwork_client_core::{App, DirectDialer, Options};
use iwork_fake_daemon::{FakeDaemon, fixtures as f, scenario, self_trusting_identity};
use iwork_proto::pb;
use pb::ui_intent::Kind as I;
use support::web::{Web, WebState};
use support::{CID, CONV, persisted, store_with_open};
use tokio::time::timeout;

const WAIT: Duration = Duration::from_secs(20);

async fn start(scenarios: &[&str], cap: usize) -> (FakeDaemon, App, Web, tempfile::TempDir) {
    let dir = tempfile::Builder::new()
        .prefix("t09-anchors")
        .tempdir_in("/tmp")
        .unwrap();
    let d = FakeDaemon::start(dir.path().join("d.sock")).await.unwrap();
    for s in scenarios {
        d.run_scenario(s).await.unwrap();
    }
    let dialer = DirectDialer::new(d.endpoint(), self_trusting_identity().unwrap(), "test");
    let (app, effects) = App::start(
        dialer,
        Options {
            timeline_cap: cap,
            ..Options::default()
        },
    );
    std::mem::forget(effects);
    let web = Web::new(app.attach().await.unwrap());
    (d, app, web, dir)
}

async fn intent(app: &App, kind: I) -> pb::IntentAck {
    app.intent(pb::UiIntent { kind: Some(kind) }).await
}

async fn open(app: &App, conv: &str, pending: bool) {
    let ack = intent(
        app,
        I::SetVisible(pb::SetVisible {
            sidebar: true,
            pending,
            conversation_id: Some(conv.into()),
            ..Default::default()
        }),
    )
    .await;
    assert_eq!(ack.error, None);
}

async fn window(app: &App, conv: &str, first: i64, last: i64) {
    intent(
        app,
        I::SetVisibleRows(pb::SetVisibleRows {
            conversation_id: conv.into(),
            first_ordinal: first,
            last_ordinal: last,
            by_user: false,
        }),
    )
    .await;
}

async fn until(app: &App, web: &mut Web, what: &str, cond: impl Fn(&WebState) -> bool) {
    let r = timeout(WAIT, async {
        while !cond(&web.state) {
            if let Ok(Ok(c)) =
                timeout(Duration::from_millis(300), app.next_frame(web.request())).await
            {
                web.accept(&c);
            }
        }
    })
    .await;
    assert!(r.is_ok(), "等不到：{what}");
}

fn items(s: &WebState) -> &[pb::AnchorItem] {
    s.anchors.as_ref().map_or(&[], |a| a.items.as_slice())
}

fn kinds(s: &WebState) -> Vec<pb::AnchorKind> {
    items(s).iter().map(|i| i.kind()).collect()
}

/// 打开对话后取到锚点窗口：三种都有，文字来自 Rust；当前位置是最新那一处之上最近的一个。
#[tokio::test(flavor = "multi_thread")]
async fn anchors_window_after_opening() {
    let (_d, app, mut web, _dir) = start(&["anchors-basic"], 5000).await;
    open(&app, "c-anchors", false).await;
    until(&app, &mut web, "锚点到了", |s| s.anchors.is_some()).await;
    let k = kinds(&web.state);
    for want in [
        pb::AnchorKind::User,
        pb::AnchorKind::Waiting,
        pb::AnchorKind::Failed,
    ] {
        assert!(k.contains(&want), "{want:?} 在列表里：{k:?}");
    }
    let a = web.state.anchors.as_ref().unwrap();
    assert!(a.has_waiting && a.has_failed);
    let failed = items(&web.state)
        .iter()
        .find(|i| i.kind() == pb::AnchorKind::Failed)
        .unwrap();
    assert_eq!(failed.kind_label, "失败");
    assert_eq!(failed.preview, "这一轮中途失败：模型暂时不可用");
    until(&app, &mut web, "当前位置", |s| {
        s.anchor_cursor
            .as_ref()
            .is_some_and(|c| !c.current_key.is_empty())
    })
    .await;
}

/// 等你处理那一处的预览与「待你处理」页同一份文案；处理掉以后不再是锚点（随摘要轮重取）。
#[tokio::test(flavor = "multi_thread")]
async fn waiting_anchor_text_matches_pending_page_and_goes_when_decided() {
    let (_d, app, mut web, _dir) = start(&["anchors-basic"], 5000).await;
    open(&app, "c-anchors", true).await;
    until(&app, &mut web, "锚点与待处理页", |s| {
        s.anchors.is_some() && s.pending.as_ref().is_some_and(|p| !p.entries.is_empty())
    })
    .await;
    let waiting = items(&web.state)
        .iter()
        .find(|i| i.kind() == pb::AnchorKind::Waiting)
        .unwrap()
        .clone();
    let page = web.state.pending.as_ref().unwrap();
    assert!(
        page.entries.iter().any(|i| i.title == waiting.preview),
        "预览「{}」与待处理页同一份文案：{:?}",
        waiting.preview,
        page.entries.iter().map(|i| &i.title).collect::<Vec<_>>()
    );
    let ack = intent(
        &app,
        I::DecideApproval(pb::DecideApprovalIntent {
            approval_id: "ap-ab-wait".into(),
            allow: true,
            remember: 0,
        }),
    )
    .await;
    assert_eq!(ack.error, None);
    until(&app, &mut web, "审批处理掉后不再是锚点", |s| {
        s.anchors.as_ref().is_some_and(|a| !a.has_waiting)
            && !kinds(s).contains(&pb::AnchorKind::Waiting)
    })
    .await;
}

/// 跳到已加载的那一处：发一次滚动指令，同一处再跳换一个 token。
#[tokio::test(flavor = "multi_thread")]
async fn jump_to_loaded_row_emits_scroll_once() {
    let (_d, app, mut web, _dir) = start(&["anchors-basic"], 5000).await;
    open(&app, "c-anchors", false).await;
    until(&app, &mut web, "锚点到了", |s| s.anchors.is_some()).await;
    let user = items(&web.state)
        .iter()
        .rfind(|i| i.kind() == pb::AnchorKind::User)
        .unwrap()
        .key
        .clone();
    let jump = |key: String| {
        I::JumpToAnchor(pb::JumpToAnchor {
            conversation_id: "c-anchors".into(),
            key,
        })
    };
    intent(&app, jump(user.clone())).await;
    until(&app, &mut web, "滚动指令", |s| s.scroll_to.is_some()).await;
    let first = web.state.scroll_to.clone().unwrap();
    assert!(
        web.state.rows.contains_key(&first.row_key),
        "滚到的行在时间线里：{}",
        first.row_key
    );
    assert_eq!(first.highlight_ms, 1000);
    intent(&app, jump(user)).await;
    until(&app, &mut web, "再跳一次", |s| {
        s.scroll_to.as_ref().is_some_and(|x| x.token != first.token)
    })
    .await;
}

/// 跳到没加载的那一处：时间线换成以它为中心的一段、落到那一行，不再接着最新（出现「回到最新」）；
/// 往下翻到 has_newer 为假后接回最新，之后的新消息直接进时间线。
#[tokio::test(flavor = "multi_thread")]
async fn jump_to_unloaded_centers_segment_then_page_forward_reattaches_tail() {
    let (d, app, mut web, _dir) = start(&["anchors-unloaded"], 5000).await;
    let cid = "c-anchors-long";
    open(&app, cid, false).await;
    until(&app, &mut web, "锚点到了", |s| s.anchors.is_some()).await;
    // 最老的用户消息不在打开时加载的那一页里；锚点窗口以最新为中心，前面还有：往前翻看到最老的一处
    let oldest = "m-al000-u";
    assert!(
        !web.state.rows.contains_key(oldest),
        "对照：最老的一条没加载"
    );
    let target = loop {
        if let Some(i) = items(&web.state)
            .iter()
            .find(|i| i.preview.starts_with("第 0 轮"))
        {
            break i.key.clone();
        }
        let first = items(&web.state).first().unwrap().key.clone();
        intent(
            &app,
            I::AnchorBrowse(pb::AnchorBrowse {
                conversation_id: cid.into(),
                key: first.clone(),
            }),
        )
        .await;
        until(&app, &mut web, "以前面那一处为中心重取", |s| {
            items(s).first().is_some_and(|i| i.key != first)
        })
        .await;
    };
    intent(
        &app,
        I::JumpToAnchor(pb::JumpToAnchor {
            conversation_id: cid.into(),
            key: target,
        }),
    )
    .await;
    until(&app, &mut web, "落到最老的那一条并高亮", |s| {
        s.scroll_to.as_ref().is_some_and(|x| x.row_key == oldest) && s.rows.contains_key(oldest)
    })
    .await;
    let w = web.state.window.unwrap();
    assert!(!w.at_tail, "不再接着最新");
    assert!(
        web.state.anchors.as_ref().unwrap().jump_loading.is_none(),
        "加载提示收掉"
    );
    // 往下翻：窗口一直顶着这一段的末尾
    let newest = format!("m-al{:03}-u", scenario::ANCHORS_LONG_EXCHANGES - 1);
    loop {
        window(&app, cid, i64::MAX - 1, i64::MAX).await;
        let reached = timeout(Duration::from_secs(2), async {
            while !web.state.window.as_ref().is_some_and(|w| w.at_tail) {
                if let Ok(c) = app.next_frame(web.request()).await {
                    web.accept(&c);
                }
            }
        })
        .await;
        if reached.is_ok() {
            break;
        }
    }
    assert!(web.state.rows.contains_key(&newest), "接回了最新");
    d.append(f::user_message("m-after-tail", cid, "接回之后的新消息"));
    until(&app, &mut web, "新消息直接进时间线", |s| {
        s.rows.contains_key("m-after-tail")
    })
    .await;
}

/// 在飞查询表「锚点窗口」：发出后断线，重连后照样取到。
#[tokio::test(flavor = "multi_thread")]
async fn anchors_query_in_flight_survives_disconnect() {
    let (d, app, mut web, _dir) = start(&["anchors-basic"], 5000).await;
    open(&app, "c-anchors", false).await;
    d.disconnect_all().await;
    until(&app, &mut web, "重连后锚点到了", |s| {
        s.anchors.is_some()
    })
    .await;
}

/// 在飞查询表「居中加载」：跳到没加载的那一处后立即断线，重连后照样落到那一行。
#[tokio::test(flavor = "multi_thread")]
async fn jump_load_interrupted_resent() {
    let (d, app, mut web, _dir) = start(&["anchors-unloaded"], 5000).await;
    let cid = "c-anchors-long";
    open(&app, cid, false).await;
    until(&app, &mut web, "锚点到了", |s| s.anchors.is_some()).await;
    // 窗口里最前面那一处还没加载（打开时只取最新一页）
    let first = items(&web.state).first().unwrap().clone();
    assert!(
        !web.state
            .rows
            .values()
            .any(|r| r.key.ends_with("-u") && first.preview.starts_with(&r.key)),
        "对照"
    );
    intent(
        &app,
        I::JumpToAnchor(pb::JumpToAnchor {
            conversation_id: cid.into(),
            key: first.key.clone(),
        }),
    )
    .await;
    d.disconnect_all().await;
    until(&app, &mut web, "重连后落到那一处", |s| {
        s.scroll_to
            .as_ref()
            .is_some_and(|x| s.rows.contains_key(&x.row_key))
    })
    .await;
}

/// 在飞查询表「往后翻页」：跳到旧处后往下翻，第一次发出后断线，重连后照样接回最新。
#[tokio::test(flavor = "multi_thread")]
async fn newer_page_in_flight_survives_disconnect() {
    let (d, app, mut web, _dir) = start(&["anchors-unloaded"], 5000).await;
    let cid = "c-anchors-long";
    open(&app, cid, false).await;
    until(&app, &mut web, "锚点到了", |s| s.anchors.is_some()).await;
    let first = items(&web.state).first().unwrap().key.clone();
    intent(
        &app,
        I::JumpToAnchor(pb::JumpToAnchor {
            conversation_id: cid.into(),
            key: first,
        }),
    )
    .await;
    until(&app, &mut web, "离开最新", |s| {
        s.window.as_ref().is_some_and(|w| !w.at_tail)
    })
    .await;
    window(&app, cid, i64::MAX - 1, i64::MAX).await;
    d.disconnect_all().await;
    let newest = format!("m-al{:03}-u", scenario::ANCHORS_LONG_EXCHANGES - 1);
    let deadline = Instant::now() + WAIT;
    while !web.state.window.as_ref().is_some_and(|w| w.at_tail) {
        assert!(Instant::now() < deadline, "等不到接回最新");
        window(&app, cid, i64::MAX - 1, i64::MAX).await;
        if let Ok(Ok(c)) = timeout(Duration::from_millis(300), app.next_frame(web.request())).await
        {
            web.accept(&c);
        }
    }
    until(&app, &mut web, "最新一条在", |s| {
        s.rows.contains_key(&newest)
    })
    .await;
}

// ---------- 不连后台：状态库 ----------

fn user_msg(id: &str, seq: u64) -> pb::Message {
    pb::Message {
        id: id.into(),
        conversation_id: CID.into(),
        author: Some(pb::Author {
            kind: Some(pb::author::Kind::User(pb::UserAuthor {})),
        }),
        status: pb::MessageStatus::Sent.into(),
        body: f::text("hi"),
        created_seq: seq,
        ..Default::default()
    }
}

fn appended(m: pb::Message) -> pb::domain_event::Event {
    pb::domain_event::Event::MessageAppended(pb::MessageAppended { message: Some(m) })
}

/// 锚点只来自查询：时间线里有用户消息，没取过 ListAnchors 时视图模型里没有锚点。
#[test]
fn anchors_come_only_from_query() {
    let mut s = store_with_open();
    s.apply(persisted(10, &[CONV], appended(user_msg("u1", 10))));
    let tl = s.open.as_ref().unwrap();
    assert!(tl.row("u1").is_some(), "对照：消息进了时间线");
    let ctx = iwork_client_core::derive::Ctx {
        now: std::time::SystemTime::now(),
    };
    assert_eq!(derive_anchors::anchors(&s, tl, ctx), None);
}

fn with_window(s: &mut iwork_client_core::store::Store) {
    s.apply(iwork_client_core::store::Item::ViewResult {
        purpose: QueryPurpose::Anchors {
            conversation_id: CID.into(),
            center: 0,
            round: None,
        },
        result: Box::new(pb::QueryResult {
            seq: 5,
            kind: Some(pb::query_result::Kind::ListAnchors(pb::ListAnchorsResult {
                anchors: vec![pb::Anchor {
                    created_seq: 3,
                    at: None,
                    kind: Some(pb::anchor::Kind::UserMessage(pb::UserMessageAnchor {
                        message_id: "u0".into(),
                        excerpt: "第一句".into(),
                    })),
                }],
                ..Default::default()
            })),
        }),
    });
}

/// 对话里来了持久事件：锚点窗口失效，进摘要重查的同一轮（这一轮多带锚点那一条，它回来之前不算走完）。
#[test]
fn anchors_stale_on_conversation_event_and_join_summary_round() {
    let mut s = store_with_open();
    with_window(&mut s);
    assert!(!s.refresh.is_stale(), "对照：取到窗口后不失效");
    s.apply(persisted(10, &[CONV], appended(user_msg("u1", 10))));
    assert!(s.refresh.is_stale(), "对话的持久事件让锚点（随摘要）失效");
    let (round, tables) = s
        .refresh
        .due_with(Instant::now(), false, true)
        .expect("该发一轮");
    assert!(tables.contains(&SummaryTable::Anchors), "这一轮带锚点");
    for t in SummaryTable::ALL {
        s.refresh.result_applied(round, t);
    }
    assert!(s.refresh.is_stale(), "锚点那一条没回来，这一轮没走完");
    s.refresh.result_applied(round, SummaryTable::Anchors);
    assert!(!s.refresh.is_stale());
}

/// 没有锚点窗口时对话事件不让摘要失效（还没取过的那一份由打开对话时单独取）。
#[test]
fn conversation_event_without_window_does_not_invalidate() {
    let mut s = store_with_open();
    s.apply(persisted(10, &[CONV], appended(user_msg("u1", 10))));
    assert!(!s.refresh.is_stale());
}

/// 快照重建时间线（重连、Q9）：等着的跳转作废，锚点按最新位置重取。
#[test]
fn jump_dropped_when_snapshot_resets() {
    let mut s = store_with_open();
    with_window(&mut s);
    s.jump_to_anchor(CID, "3");
    assert!(
        s.open.as_ref().unwrap().anchors.pending_jump.is_some(),
        "对照：u0 没加载，等着取以它为中心的一段"
    );
    s.apply(support::snapshot(
        CONV,
        20,
        vec![support::page(20, vec![], false)],
    ));
    let a = &s.open.as_ref().unwrap().anchors;
    assert!(a.pending_jump.is_none());
    assert_eq!(a.want_center, Some(0));
}

/// 往下翻超过上限：从离可见窗口远的那一端（旧的一端）丢，标成还能往上翻取回（方案 2.2 第 4 条）。
#[test]
fn cap_drops_far_end() {
    use iwork_client_core::store::{Item, ScopeKind, Store, Visible};
    let mut s = Store::with_timeline_cap(4);
    s.register_subscription(CONV, ScopeKind::Conversation(CID.into()));
    s.set_visible(Visible {
        conversation_id: Some(CID.into()),
        ..Default::default()
    });
    s.apply(support::snapshot(
        CONV,
        10,
        vec![support::page(
            10,
            (1..=4).map(|i| user_msg(&format!("m{i}"), i)).collect(),
            false,
        )],
    ));
    let newer = support::page(
        12,
        (5..=6).map(|i| user_msg(&format!("m{i}"), i)).collect(),
        false,
    );
    s.apply(Item::ViewResult {
        purpose: QueryPurpose::NewerPage {
            conversation_id: CID.into(),
            after: "m4".into(),
        },
        result: Box::new(newer),
    });
    let tl = s.open.as_ref().unwrap();
    let ids: Vec<_> = tl.identities().into_iter().map(|(_, id)| id).collect();
    assert_eq!(ids, ["m3", "m4", "m5", "m6"], "丢的是旧的一端");
    assert!(tl.has_older, "丢掉的还能往上翻取回");
}

/// 跳到 `preview` 以「第 N 轮」开头的那一处，等它落地；返回那一行的 key。
async fn jump_to_round(app: &App, web: &mut Web, cid: &str, round: usize) -> String {
    let prefix = format!("第 {round} 轮");
    let key = items(&web.state)
        .iter()
        .find(|i| i.preview.starts_with(&prefix))
        .unwrap_or_else(|| panic!("锚点窗口里没有{prefix}"))
        .key
        .clone();
    intent(
        app,
        I::JumpToAnchor(pb::JumpToAnchor {
            conversation_id: cid.into(),
            key,
        }),
    )
    .await;
    let row = format!("m-al{round:03}-u");
    let want = row.clone();
    until(app, web, "落到那一处", move |s| {
        s.scroll_to.as_ref().is_some_and(|x| x.row_key == want) && s.rows.contains_key(&want)
    })
    .await;
    row
}

/// 相对跳转越过锚点窗口前端（前面还有）：先以当前位置为中心重取窗口，再落到前一处（方案 2.9「跳转」）。
/// 单测 `relative_at_window_edge_fetches_first` 只测状态机；这里走「重取 → 窗口回来 → 落下」的整条接线。
#[tokio::test(flavor = "multi_thread")]
async fn relative_jump_at_window_edge_fetches_first() {
    let (_d, app, mut web, _dir) = start(&["anchors-unloaded"], 5000).await;
    let cid = "c-anchors-long";
    open(&app, cid, false).await;
    until(&app, &mut web, "锚点到了", |s| s.anchors.is_some()).await;
    // 以最新为中心前后各 60 处：120 轮里窗口从第 60 轮开始，前面还有
    let first = items(&web.state).first().unwrap().clone();
    assert!(first.preview.starts_with("第 60 轮"), "{}", first.preview);
    assert!(web.state.anchors.as_ref().unwrap().has_before);
    // 跳到窗口最前那一处，网页报它是可见第一行：当前位置就是窗口的第一处
    let row = jump_to_round(&app, &mut web, cid, 60).await;
    let ordinal = web.state.rows[&row].ordinal;
    window(&app, cid, ordinal, ordinal + 3).await;
    until(&app, &mut web, "当前位置是第 60 轮", |s| {
        s.anchor_cursor.as_ref().map(|c| c.current_key.as_str()) == Some(first.key.as_str())
    })
    .await;
    let ack = intent(
        &app,
        I::JumpRelative(pb::JumpRelative {
            conversation_id: cid.into(),
            delta: -1,
        }),
    )
    .await;
    assert_eq!(ack.error, None);
    until(
        &app,
        &mut web,
        "窗口重取到前面、落到第 59 轮",
        |s| {
            items(s).iter().any(|i| i.preview.starts_with("第 59 轮"))
                && s.scroll_to
                    .as_ref()
                    .is_some_and(|x| x.row_key == "m-al059-u")
        },
    )
    .await;
}

/// 锚点窗口过时（目标那一条已经不在了）时跳过去：居中加载回来的一段里没有目标行，只显示这一段，
/// 不滚动、不高亮，加载提示收掉（方案 S19）。
#[tokio::test(flavor = "multi_thread")]
async fn jump_target_gone_shows_segment_without_highlight() {
    let (d, app, mut web, _dir) = start(&["anchors-unloaded"], 5000).await;
    let cid = "c-anchors-long";
    open(&app, cid, false).await;
    until(&app, &mut web, "锚点到了", |s| s.anchors.is_some()).await;
    let target = items(&web.state)
        .iter()
        .find(|i| i.preview.starts_with("第 60 轮"))
        .unwrap()
        .key
        .clone();
    assert!(
        !web.state.rows.contains_key("m-al060-u"),
        "对照：目标没加载"
    );
    // 后台那边这一条没了，客户端的锚点窗口还没重取（不产生事件，锚点不会失效）
    d.mutate_tables(|t| {
        let row = t.conversations.get_mut(cid).unwrap();
        row.messages.retain(|m| m.id != "m-al060-u");
    });
    let before = web.state.scroll_to.clone();
    intent(
        &app,
        I::JumpToAnchor(pb::JumpToAnchor {
            conversation_id: cid.into(),
            key: target,
        }),
    )
    .await;
    until(
        &app,
        &mut web,
        "换成那一段、加载提示收掉",
        |s| {
            s.rows.contains_key("m-al061-u")
                && s.anchors.as_ref().is_some_and(|a| a.jump_loading.is_none())
        },
    )
    .await;
    assert!(!web.state.rows.contains_key("m-al060-u"));
    assert_eq!(web.state.scroll_to, before, "目标不在：不滚动、不高亮");
    assert!(!web.state.window.unwrap().at_tail, "停在那一段，不接着最新");
}

/// 居中加载那一段不接着最新，也要守 5000 行（这里调成 12 行）上限（审查 4/F-8）：每条消息后面跟一串工具行，
/// 一段 50 条消息的居中结果远超上限。留下目标附近连续的一段，两端都标成还能翻回去。
#[test]
fn around_page_obeys_timeline_cap() {
    use iwork_client_core::store::{ScopeKind, Store, Visible};
    const CAP: usize = 12;
    let mut s = Store::with_timeline_cap(CAP);
    s.register_subscription(CONV, ScopeKind::Conversation(CID.into()));
    s.set_visible(Visible {
        conversation_id: Some(CID.into()),
        ..Default::default()
    });
    s.apply(support::snapshot(
        CONV,
        1000,
        vec![support::page(1000, vec![user_msg("latest", 999)], true)],
    ));
    // 10 条消息，每条后面 4 个工具行：seq 100、101..104、110、111..114……
    let mut messages = Vec::new();
    let mut tool_calls = Vec::new();
    for i in 0..10u64 {
        let base = 100 + i * 10;
        messages.push(user_msg(&format!("m{i}"), base));
        for j in 1..=4 {
            tool_calls.push(pb::ToolCall {
                turn_id: format!("t{i}"),
                tool_call_id: format!("tc{i}-{j}"),
                tool_name: "Bash".into(),
                created_seq: base + j,
                ..Default::default()
            });
        }
    }
    let center = 150; // m5
    let result = pb::QueryResult {
        seq: 1000,
        kind: Some(pb::query_result::Kind::GetConversation(
            pb::GetConversationResult {
                conversation: Some(support::conversation(CID, "", 0)),
                messages,
                tool_calls,
                has_more: true,
                has_newer: true,
                ..Default::default()
            },
        )),
    };
    s.apply(support::view_result(
        QueryPurpose::AroundPage {
            conversation_id: CID.into(),
            seq: center,
        },
        result,
    ));
    let tl = s.open.as_ref().unwrap();
    let ids = support::row_ids(&s);
    assert!(ids.len() <= CAP, "{} 行超过上限 {CAP}：{ids:?}", ids.len());
    assert!(ids.contains(&"m5".to_owned()), "目标留着：{ids:?}");
    // 留下的是连续的一段：按 seq 排好后，原段里夹在首尾之间的行一个不缺
    let seqs: Vec<u64> = tl.identities().iter().map(|(k, _)| k.seq).collect();
    let (lo, hi) = (seqs[0], *seqs.last().unwrap());
    let expected = (lo..=hi)
        .filter(|q| (100..200).contains(q) && (q % 10) <= 4)
        .count();
    assert_eq!(seqs.len(), expected, "中间没有洞：{seqs:?}");
    assert!(lo <= center && center <= hi);
    assert!(tl.has_older, "丢了旧的一端，还能往上翻");
    assert!(!tl.at_tail, "丢了新的一端（本来也不接着最新），往下翻取回");
}

// ---------- 审查第 4 轮：跳转与阅读位置 ----------

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

fn anchors_arrive(
    s: &mut iwork_client_core::store::Store,
    center: u64,
    seqs: &[u64],
    (before, after): (bool, bool),
) {
    s.apply(support::view_result(
        QueryPurpose::Anchors {
            conversation_id: CID.into(),
            center,
            round: None,
        },
        pb::QueryResult {
            seq: 500,
            kind: Some(pb::query_result::Kind::ListAnchors(pb::ListAnchorsResult {
                anchors: seqs.iter().map(|q| user_anchor(*q)).collect(),
                has_before: before,
                has_after: after,
                ..Default::default()
            })),
        },
    ));
}

fn around_arrives(s: &mut iwork_client_core::store::Store, seq: u64) {
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

fn pending_seq(s: &iwork_client_core::store::Store) -> Option<u64> {
    s.open
        .as_ref()?
        .anchors
        .pending_jump
        .as_ref()
        .map(|j| j.anchor.created_seq)
}

fn scroll_row(s: &iwork_client_core::store::Store) -> Option<String> {
    s.open
        .as_ref()?
        .anchors
        .scroll
        .as_ref()
        .map(|x| x.row_key.clone())
}

/// 先跳 A、再跳 B（都没加载），A 的居中结果先到：它不能消费 B 的等待，也不能发滚动；B 的到了才落到 B（4/F-5）。
#[test]
fn old_around_reply_must_not_consume_new_jump() {
    let mut s = store_with_open();
    anchors_arrive(&mut s, 0, &[3, 5], (false, false));
    s.jump_to_anchor(CID, "3");
    s.jump_to_anchor(CID, "5");
    assert_eq!(pending_seq(&s), Some(5), "对照：等的是 B");
    around_arrives(&mut s, 3);
    assert_eq!(pending_seq(&s), Some(5), "A 的结果过时，B 还在等");
    assert_eq!(scroll_row(&s), None, "过时的结果不滚动");
    around_arrives(&mut s, 5);
    assert_eq!(pending_seq(&s), None);
    assert_eq!(scroll_row(&s).as_deref(), Some("u5"), "B 的结果到了落到 B");
}

/// 等居中结果期间用户自己滚动：等着的跳转作废，结果迟到也不再把用户拉回去；跳转带来的回报不算（4/F-6）。
#[test]
fn user_scroll_cancels_pending_jump() {
    let mut s = store_with_open();
    anchors_arrive(&mut s, 0, &[3, 5], (false, false));
    s.jump_to_anchor(CID, "3");
    s.set_window(CID, 0, 10, false);
    assert_eq!(pending_seq(&s), Some(3), "对照：不是用户滚动的回报不取消");
    s.set_window(CID, 0, 20, true);
    assert_eq!(pending_seq(&s), None, "用户滚动了，作废");
    around_arrives(&mut s, 3);
    assert_eq!(scroll_row(&s), None, "迟到的结果不滚动");
}

/// 用户滚动也作废在等窗口的相对跳转（4/F-6 同类）。
#[test]
fn user_scroll_cancels_pending_relative() {
    let mut s = store_with_open();
    s.apply(support::snapshot(
        CONV,
        100,
        vec![support::page(100, vec![user_msg("u10", 10)], true)],
    ));
    anchors_arrive(&mut s, 0, &[10], (true, false));
    s.jump_relative(CID, -1);
    assert_eq!(
        s.open.as_ref().unwrap().anchors.pending_relative,
        Some(-1),
        "对照：越过窗口前端，等重取"
    );
    s.set_window(CID, 21, 21, true);
    assert_eq!(s.open.as_ref().unwrap().anchors.pending_relative, None);
}

/// 翻看换来的窗口不改真实阅读位置：当前位置还是用户正在看的那一处，⌥↑ 按真实位置跳（4/F-7）。
#[test]
fn browse_window_must_not_move_actual_current_anchor() {
    let mut s = store_with_open();
    s.apply(support::snapshot(
        CONV,
        100,
        vec![support::page(
            100,
            [10, 20, 30]
                .iter()
                .map(|q| user_msg(&format!("u{q}"), *q))
                .collect(),
            true,
        )],
    ));
    anchors_arrive(&mut s, 0, &[10, 20, 30], (true, false));
    // 可见第一行是 u20（ordinal = seq*2+1）
    s.set_window(CID, 41, 61, true);
    let current =
        |s: &iwork_client_core::store::Store| s.open.as_ref().unwrap().anchors.current.clone();
    assert_eq!(current(&s).as_deref(), Some("20"));
    // 翻看到很早的地方，换来一份不含阅读位置的窗口
    s.anchor_browse(CID, "10");
    anchors_arrive(&mut s, 2, &[1, 2, 3], (true, true));
    assert_eq!(current(&s).as_deref(), Some("20"), "只是预览，阅读位置不动");
    // ⌥↑：前一处是 10，不在这份窗口里 → 围绕真实位置重取，窗口到了再落
    s.jump_relative(CID, -1);
    let a = &s.open.as_ref().unwrap().anchors;
    assert_eq!(a.pending_relative, Some(-1));
    assert_eq!(a.want_center, Some(20), "围绕真实位置重取，不围绕浏览中心");
    anchors_arrive(&mut s, 20, &[10, 20, 30], (true, false));
    assert_eq!(
        scroll_row(&s).as_deref(),
        Some("u10"),
        "落到真实位置的前一处"
    );
}

/// 锚点窗口先到（等你处理那一处的说明还空着），「待你处理」表后到、中间网页取过一帧：表一到，信息卡上的
/// 说明要跟着补上，不能停在空的（审查 4/F-9）。
#[test]
fn pending_replacement_updates_anchor_preview() {
    use iwork_client_core::frame::{FRAME_INTERVAL, FrameEngine, Pull};
    use iwork_client_core::store::Connection;
    use support::web::Web;
    let ctx = iwork_client_core::derive::Ctx {
        now: std::time::SystemTime::UNIX_EPOCH + Duration::from_secs(1_762_100_000),
    };
    let mut s = store_with_open();
    s.set_connection(Connection::Live);
    s.apply(support::view_result(
        QueryPurpose::Anchors {
            conversation_id: CID.into(),
            center: 0,
            round: None,
        },
        pb::QueryResult {
            seq: 50,
            kind: Some(pb::query_result::Kind::ListAnchors(pb::ListAnchorsResult {
                anchors: vec![pb::Anchor {
                    created_seq: 40,
                    at: None,
                    kind: Some(pb::anchor::Kind::Waiting(pb::WaitingAnchor {
                        pending_item_id: "ap1".into(),
                    })),
                }],
                waiting_count: 1,
                ..Default::default()
            })),
        },
    ));
    let mut e = FrameEngine::new(1 << 20);
    let mut w = Web::new(e.attach(&mut s));
    let mut t = Instant::now();
    let mut drain = |s: &mut iwork_client_core::store::Store, w: &mut Web| {
        t += FRAME_INTERVAL;
        while let Pull::Chunk(c) = e.pull(s, w.epoch, w.applied, t, ctx) {
            w.accept(&c);
        }
    };
    drain(&mut s, &mut w);
    let preview = |w: &Web| w.state.anchors.as_ref().map(|a| a.items[0].preview.clone());
    assert_eq!(
        preview(&w).as_deref(),
        Some(""),
        "对照：表还没到，说明是空的"
    );
    s.apply(support::snapshot(
        support::PENDING,
        60,
        vec![pb::QueryResult {
            seq: 60,
            kind: Some(pb::query_result::Kind::ListPendingItems(
                pb::ListPendingItemsResult {
                    items: vec![pb::PendingItem {
                        item: Some(pb::pending_item::Item::Approval(pb::Approval {
                            id: "ap1".into(),
                            conversation_id: CID.into(),
                            action: Some(f::command_action("rm -rf build")),
                            ..Default::default()
                        })),
                    }],
                },
            )),
        }],
    ));
    drain(&mut s, &mut w);
    assert_eq!(preview(&w).as_deref(), Some("审批：rm -rf build"));
}

/// 相对跳转在等以某处为中心的窗口，这时到的是另一个中心的窗口（摘要重查轮带回的旧中心、更早的请求）：
/// 按这份窗口重新判断一次，前一处还在窗口外就接着等，不会落到别处；对的那份到了再落（4/F-5 的同类检查。
/// 锚点是全局的事实，哪份窗口里有前一处，落点都一样，所以不需要按中心过滤）。
#[test]
fn stale_anchor_window_must_not_consume_pending_relative() {
    let mut s = store_with_open();
    s.apply(support::snapshot(
        CONV,
        100,
        vec![support::page(
            100,
            [10, 20]
                .iter()
                .map(|q| user_msg(&format!("u{q}"), *q))
                .collect(),
            true,
        )],
    ));
    anchors_arrive(&mut s, 0, &[20], (true, false));
    s.set_window(CID, 41, 41, true);
    s.jump_relative(CID, -1);
    let want = s.open.as_ref().unwrap().anchors.want_center;
    assert_eq!(want, Some(20), "对照：以当前位置为中心重取");
    anchors_arrive(&mut s, 0, &[20], (true, false));
    assert_eq!(
        s.open.as_ref().unwrap().anchors.pending_relative,
        Some(-1),
        "中心不对的窗口判断不了，接着等"
    );
    assert_eq!(scroll_row(&s), None);
    anchors_arrive(&mut s, 20, &[10, 20], (false, false));
    assert_eq!(scroll_row(&s).as_deref(), Some("u10"));
}
