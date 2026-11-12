//! 审查 1（3b36c750）返工的会话级用例：每条用例的注释写明对应的 finding。连假后台，只在 Unix 上编译。

#![cfg(unix)]

mod support;

use std::time::Duration;

use iwork_client_core::store::QueryPurpose;
use iwork_client_core::{App, DialError, Dialer, DirectDialer, FrameError, Options};
use iwork_fake_daemon::{FakeDaemon, fixtures as f, scenario, self_trusting_identity};
use iwork_ipc::client::{Client, Incoming};
use iwork_proto::pb;
use pb::ui_intent::Kind as I;
use support::web::{Web, WebState};
use tokio::time::{sleep, timeout};

const WAIT: Duration = Duration::from_secs(20);

async fn daemon(scenarios: &[&str]) -> (FakeDaemon, tempfile::TempDir) {
    let dir = tempfile::Builder::new()
        .prefix("t09")
        .tempdir_in("/tmp")
        .unwrap();
    let d = FakeDaemon::start(dir.path().join("d.sock")).await.unwrap();
    for s in scenarios {
        d.run_scenario(s).await.unwrap();
    }
    (d, dir)
}

async fn connect(d: &FakeDaemon, cap: usize) -> (App, Web) {
    let dialer = DirectDialer::new(d.endpoint(), self_trusting_identity().unwrap(), "test");
    let (app, effects) = App::start(
        dialer,
        Options {
            chunk_max: 4096,
            timeline_cap: cap,
            ..Options::default()
        },
    );
    std::mem::forget(effects);
    let web = Web::new(app.attach().await.unwrap());
    (app, web)
}

async fn intent(app: &App, kind: I) -> pb::IntentAck {
    app.intent(pb::UiIntent { kind: Some(kind) }).await
}

fn visible(conv: Option<&str>, list: bool) -> I {
    I::SetVisible(pb::SetVisible {
        sidebar: true,
        conversation_id: conv.map(Into::into),
        conversation_list: list.then(|| pb::ConversationListQuery {
            project_id: scenario::PROJECT.into(),
            filter: String::new(),
        }),
        ..Default::default()
    })
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

fn live(s: &WebState) -> bool {
    s.connection
        .as_ref()
        .is_some_and(|c| c.state == pb::ConnectionState::Live as i32)
}

async fn settle(app: &App, web: &mut Web) {
    until(app, web, "连上", live).await;
    loop {
        let before = web.state.clone();
        let _ = timeout(Duration::from_millis(400), async {
            loop {
                if let Ok(c) = app.next_frame(web.request()).await {
                    web.accept(&c);
                }
            }
        })
        .await;
        if web.state == before {
            return;
        }
    }
}

/// 一段时间内主循环转了几轮。
async fn turns_during(app: &App, d: Duration) -> u64 {
    let stats = app.stats();
    let before = stats.borrow().loop_turns;
    sleep(d).await;
    let after = app.stats().borrow().loop_turns;
    after - before
}

/// 空闲时主循环的轮数上界：2 秒里最多这么多轮（实际只有定时器与心跳级别的唤醒）。
const IDLE_TURNS_MAX: u64 = 40;

/// 1/F-1：发过一次 MarkRead 之后空闲，主循环不空转。
#[tokio::test(flavor = "multi_thread")]
async fn idle_loop_is_bounded_after_mark_read() {
    let (d, _dir) = daemon(&["conversation-basic"]).await;
    let (app, mut web) = connect(&d, 5000).await;
    intent(&app, visible(Some("c-basic"), false)).await;
    intent(&app, I::SetFocused(pb::SetFocused { focused: true })).await;
    until(&app, &mut web, "徽标归零", |s| {
        s.sidebar
            .as_ref()
            .is_some_and(|v| v.projects.iter().all(|p| p.unread_badge.is_none()))
    })
    .await;
    settle(&app, &mut web).await;
    let n = turns_during(&app, Duration::from_secs(2)).await;
    assert!(n <= IDLE_TURNS_MAX, "空闲 2 秒主循环转了 {n} 轮");
}

/// 拨号时 panic 的拨号器：拨号任务异常结束，拨号器随之丢失。
struct Panicking;
impl Dialer for Panicking {
    async fn dial(&mut self) -> Result<(Client, Incoming), DialError> {
        panic!("测试：拨号任务异常结束");
    }
}

/// 1/F-1 同类：拨号任务异常后点「重试」，主循环也不空转。
#[tokio::test(flavor = "multi_thread")]
async fn retry_after_dial_task_panic_does_not_spin() {
    let (app, effects) = App::start(Panicking, Options::default());
    std::mem::forget(effects);
    let mut web = Web::new(app.attach().await.unwrap());
    until(&app, &mut web, "整页说明", |s| {
        s.connection
            .as_ref()
            .is_some_and(|c| c.state == pb::ConnectionState::Environment as i32)
    })
    .await;
    intent(&app, I::Retry(pb::Retry {})).await;
    let n = turns_during(&app, Duration::from_secs(1)).await;
    assert!(n <= IDLE_TURNS_MAX / 2, "重试后 1 秒主循环转了 {n} 轮");
}

/// 1/F-2：「全部对话」的查询在飞时断线（Live 之后、结果到达之前），重连后列表照样取回。重复 5 次。
#[tokio::test(flavor = "multi_thread")]
async fn list_query_in_flight_survives_disconnect() {
    for round in 0..5 {
        let (d, _dir) = daemon(&["conversation-basic"]).await;
        let (app, mut web) = connect(&d, 5000).await;
        until(&app, &mut web, "连上", live).await;
        intent(&app, visible(None, true)).await;
        d.disconnect_all().await;
        until(
            &app,
            &mut web,
            &format!("第 {round} 次：列表取回"),
            |s| {
                s.list
                    .as_ref()
                    .is_some_and(|l| !l.loading && !l.items.is_empty())
            },
        )
        .await;
    }
}

/// 1/F-3：「全部对话」列表页开着时随事件刷新（摘要轮多带一条 ListConversations）。
#[tokio::test(flavor = "multi_thread")]
async fn list_page_refreshes_on_events() {
    let (d, _dir) = daemon(&["conversation-basic"]).await;
    let (app, mut web) = connect(&d, 5000).await;
    intent(&app, visible(None, true)).await;
    until(&app, &mut web, "列表首批", |s| {
        s.list.as_ref().is_some_and(|l| !l.items.is_empty())
    })
    .await;
    d.append(f::project_conversation("c-new", scenario::PROJECT));
    until(&app, &mut web, "新对话出现在列表里", |s| {
        s.list
            .as_ref()
            .is_some_and(|l| l.items.iter().any(|c| c.conversation_id == "c-new"))
    })
    .await;
}

/// 1/F-5：视图查询失败后按退避重发、界面上看得到失败，不是每轮立即重发。
#[tokio::test(flavor = "multi_thread")]
async fn failed_query_backs_off_and_is_visible() {
    let (d, _dir) = daemon(&["conversation-basic"]).await;
    let (app, mut web) = connect(&d, 5000).await;
    intent(&app, visible(None, true)).await;
    until(&app, &mut web, "列表首批", |s| {
        s.list.as_ref().is_some_and(|l| !l.items.is_empty())
    })
    .await;
    // 让「加载下一批」的游标失效：后台把游标对话删掉，之后每次都回 INVALID_ARGUMENT。
    let last = web
        .state
        .list
        .as_ref()
        .unwrap()
        .items
        .last()
        .unwrap()
        .conversation_id
        .clone();
    d.mutate_tables(|t| {
        t.conversations.remove(&last);
    });
    let before = app.stats().borrow().view_queries;
    intent(&app, I::LoadMoreConversations(pb::LoadMoreConversations {})).await;
    until(&app, &mut web, "连接条上的失败说明", |s| {
        s.connection
            .as_ref()
            .is_some_and(|c| c.query_notice.is_some())
    })
    .await;
    sleep(Duration::from_millis(1500)).await;
    let sent = app.stats().borrow().view_queries - before;
    // 退避 200ms、400ms、800ms…：1.5 秒里最多四五次，立即重发则是成千上万次。
    assert!((1..=8).contains(&sent), "1.5 秒里发了 {sent} 条查询");
}

/// 方案第 7 节：同一时刻最多挂一个取帧，新来的顶掉旧的。
#[tokio::test(flavor = "multi_thread")]
async fn second_waiter_replaces_first() {
    let (d, _dir) = daemon(&["empty"]).await;
    let (app, mut web) = connect(&d, 5000).await;
    settle(&app, &mut web).await;
    let first = {
        let app = app.clone();
        let req = web.request();
        tokio::spawn(async move { app.next_frame(req).await })
    };
    sleep(Duration::from_millis(100)).await;
    let second = {
        let app = app.clone();
        let req = web.request();
        tokio::spawn(async move { timeout(Duration::from_millis(500), app.next_frame(req)).await })
    };
    assert_eq!(
        timeout(WAIT, first).await.unwrap().unwrap(),
        Err(FrameError::Superseded)
    );
    assert!(second.await.unwrap().is_err(), "第二个照常挂着（没有新帧）");
}

/// 方案第 7 节 refresh_interrupted_after_k_results（连会话）：摘要重查发出后在不同时刻断线，之后没有新事件；
/// 重连后整轮重发，侧栏最终等于后台的值（与新客户端一致）。断点用延时撒开，覆盖「一条都没回」到「回完」。
#[tokio::test(flavor = "multi_thread")]
async fn refresh_interrupted_after_k_results() {
    for delay_ms in [0u64, 1, 2, 5, 10, 30] {
        let (d, _dir) = daemon(&["conversation-basic"]).await;
        let (app, mut web) = connect(&d, 5000).await;
        intent(&app, visible(None, false)).await;
        settle(&app, &mut web).await;
        d.mutate_tables(|t| {
            t.conversations
                .get_mut("c-basic")
                .unwrap()
                .conversation
                .title = "后台的新标题".into();
        });
        d.append(f::conversation_read("c-basic", 0));
        sleep(Duration::from_millis(delay_ms)).await;
        d.disconnect_all().await;
        until(
            &app,
            &mut web,
            &format!("断在 {delay_ms}ms：侧栏对齐"),
            |s| {
                s.sidebar.as_ref().is_some_and(|v| {
                    v.projects
                        .iter()
                        .flat_map(|p| &p.recent)
                        .any(|c| c.title == "后台的新标题")
                })
            },
        )
        .await;
    }
}

/// 一个有 n 条消息的对话（全部是用户消息，id m-0..）。
fn long_conversation(d: &FakeDaemon, id: &str, n: usize) {
    d.append(f::project_conversation(id, scenario::PROJECT));
    for i in 0..n {
        d.append(f::user_message(&format!("m-{i}"), id, "x"));
    }
}

/// 在飞查询表「往前翻页」：发出后断线，重连后窗口还顶着就重发，最终翻到最旧的一条。
#[tokio::test(flavor = "multi_thread")]
async fn older_page_in_flight_survives_disconnect() {
    let (d, _dir) = daemon(&["empty"]).await;
    long_conversation(&d, "c-long", 120);
    let (app, mut web) = connect(&d, 5000).await;
    intent(&app, visible(Some("c-long"), false)).await;
    settle(&app, &mut web).await;
    assert!(
        !web.state.rows.contains_key("m-0"),
        "对照：首屏没有最旧的一条"
    );
    // 窗口顶到最上面：每次结果回来都继续往前翻；第一次发出后立即断线。
    intent(
        &app,
        I::SetVisibleRows(pb::SetVisibleRows {
            conversation_id: "c-long".into(),
            first_ordinal: i64::MIN,
            last_ordinal: i64::MIN + 1,
            by_user: false,
        }),
    )
    .await;
    d.disconnect_all().await;
    until(&app, &mut web, "翻到最旧", |s| {
        s.rows.contains_key("m-0")
    })
    .await;
}

/// 在飞查询表「回到最新」：时间线超过上限脱离最新后点「回到最新」并立即断线，重连后照样回到最新。
#[tokio::test(flavor = "multi_thread")]
async fn jump_to_latest_in_flight_survives_disconnect() {
    let (d, _dir) = daemon(&["empty"]).await;
    long_conversation(&d, "c-long", 200);
    let (app, mut web) = connect(&d, 60).await;
    intent(&app, visible(Some("c-long"), false)).await;
    settle(&app, &mut web).await;
    // 一直往上翻，直到超过上限（60 行）从新的一端丢掉、脱离最新。
    intent(
        &app,
        I::SetVisibleRows(pb::SetVisibleRows {
            conversation_id: "c-long".into(),
            first_ordinal: i64::MIN,
            last_ordinal: i64::MIN + 1,
            by_user: false,
        }),
    )
    .await;
    until(&app, &mut web, "脱离最新", |s| {
        s.window.as_ref().is_some_and(|w| !w.at_tail)
    })
    .await;
    intent(
        &app,
        I::SetVisibleRows(pb::SetVisibleRows {
            conversation_id: "c-long".into(),
            first_ordinal: i64::MAX - 1,
            last_ordinal: i64::MAX,
            by_user: false,
        }),
    )
    .await;
    intent(
        &app,
        I::JumpToLatest(pb::JumpToLatest {
            conversation_id: "c-long".into(),
        }),
    )
    .await;
    d.disconnect_all().await;
    until(&app, &mut web, "回到最新", |s| {
        s.window.as_ref().is_some_and(|w| w.at_tail) && s.rows.contains_key("m-199")
    })
    .await;
}

/// 停在最新时也按上限截（supervisor 2026-11-11）：超出时丢掉最老的行，标成还能往上翻取回。
#[tokio::test(flavor = "multi_thread")]
async fn tail_is_capped_and_older_can_be_fetched_again() {
    let (d, _dir) = daemon(&["empty"]).await;
    long_conversation(&d, "c-long", 10);
    let (app, mut web) = connect(&d, 30).await;
    intent(&app, visible(Some("c-long"), false)).await;
    settle(&app, &mut web).await;
    for i in 10..80 {
        d.append(f::user_message(&format!("m-{i}"), "c-long", "x"));
    }
    until(&app, &mut web, "收到最新", |s| {
        s.rows.contains_key("m-79")
    })
    .await;
    settle(&app, &mut web).await;
    let w = web.state.window.unwrap();
    assert!(w.total <= 30, "时间线不超过上限：{}", w.total);
    assert!(w.has_older && w.at_tail, "最老的行丢了，标成可以往上翻");
    assert!(!web.state.rows.contains_key("m-0"));
}

/// 守卫：在飞视图查询登记表的每种用途都要有「发出后断线，重连后最终对齐」的用例（方案 2.3）。
/// 加了用途而这里没写对应用例，编译就过不去（穷举 match）。
#[test]
fn every_query_purpose_has_a_disconnect_case() {
    fn case(p: &QueryPurpose) -> &'static str {
        match p {
            QueryPurpose::Summary { .. } => "refresh_interrupted_after_k_results",
            QueryPurpose::OlderPage { .. } => "older_page_in_flight_survives_disconnect",
            QueryPurpose::LatestPage { .. } => "jump_to_latest_in_flight_survives_disconnect",
            QueryPurpose::ConversationList { .. } => "list_query_in_flight_survives_disconnect",
            QueryPurpose::Rules | QueryPurpose::LocalConfig { .. } => {
                "session.rs::pending_query_table_cleared_on_close"
            }
            QueryPurpose::Anchors { .. } => {
                "anchors.rs::anchors_query_in_flight_survives_disconnect"
            }
            QueryPurpose::NewerPage { .. } => {
                "anchors.rs::newer_page_in_flight_survives_disconnect"
            }
            QueryPurpose::AroundPage { .. } => "anchors.rs::jump_load_interrupted_resent",
        }
    }
    let src = format!(
        "{}{}{}",
        include_str!("session_more.rs"),
        include_str!("session.rs"),
        include_str!("anchors.rs")
    );
    use iwork_client_core::store::SummaryTable;
    for p in [
        QueryPurpose::Summary {
            round: 0,
            table: SummaryTable::Projects,
        },
        QueryPurpose::LatestPage {
            conversation_id: String::new(),
        },
        QueryPurpose::OlderPage {
            conversation_id: String::new(),
            before: String::new(),
        },
        QueryPurpose::Rules,
        QueryPurpose::LocalConfig {
            runtime_id: String::new(),
        },
        QueryPurpose::ConversationList {
            project_id: String::new(),
            filter: String::new(),
            append: false,
            round: None,
        },
        QueryPurpose::Anchors {
            conversation_id: String::new(),
            center: 0,
            round: None,
        },
        QueryPurpose::NewerPage {
            conversation_id: String::new(),
            after: String::new(),
        },
        QueryPurpose::AroundPage {
            conversation_id: String::new(),
            seq: 0,
        },
    ] {
        let name = case(&p).rsplit("::").next().unwrap();
        assert!(
            src.contains(&format!("async fn {name}")),
            "用途 {p:?} 的用例 {name} 不存在"
        );
    }
}

/// 奇数次读钟时「超前」20ms、偶数次是真实时间的时钟。一轮循环里若读两次钟（service 一次、登记截止时间
/// 一次），第二次必然越过一个 16ms 的帧间隔：帧的截止时间正好落在两次读钟之间（审查 2/F-1 的确定复现）。
/// 一轮只读一次钟时，同一轮里判断与登记用的是同一个时刻，跨轮的来回跳只会让等待略长或略短。
fn skewed_clock() -> iwork_client_core::Clock {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicU64, Ordering};
    let calls = Arc::new(AtomicU64::new(0));
    Arc::new(move || {
        let n = calls.fetch_add(1, Ordering::SeqCst);
        tokio::time::Instant::now() + Duration::from_millis(if n % 2 == 1 { 20 } else { 0 })
    })
}

struct Never;
impl Dialer for Never {
    async fn dial(&mut self) -> Result<(Client, Incoming), DialError> {
        std::future::pending().await
    }
}

/// 2/F-1：帧的截止时间落在两次读钟之间时也不漏登，取帧不会挂住。
#[tokio::test(flavor = "multi_thread")]
async fn frame_due_between_clock_reads_is_not_lost() {
    let (app, effects) = App::start(
        Never,
        Options {
            clock: skewed_clock(),
            ..Options::default()
        },
    );
    std::mem::forget(effects);
    let mut web = Web::new(app.attach().await.unwrap());
    for i in 0..10 {
        let c = timeout(Duration::from_secs(3), app.next_frame(web.request()))
            .await
            .unwrap_or_else(|_| panic!("第 {i} 帧挂住了"))
            .unwrap();
        web.accept(&c);
        // 让下一帧有脏键：换一下可见视图。
        intent(&app, visible(None, i % 2 == 0)).await;
    }
}

/// 2/F-2：列表页那条（属于摘要轮的）查询一直出错，摘要重查照常走下一轮：新对话仍能进侧栏。
#[tokio::test(flavor = "multi_thread")]
async fn failing_list_query_does_not_stall_summary() {
    let (d, _dir) = daemon(&["conversation-basic"]).await;
    d.fail_queries(Some(Box::new(|q: &pb::Query| {
        matches!(&q.kind, Some(pb::query::Kind::ListConversations(l)) if l.title_contains.as_deref() == Some("坏"))
    })));
    let (app, mut web) = connect(&d, 5000).await;
    intent(
        &app,
        I::SetVisible(pb::SetVisible {
            sidebar: true,
            conversation_list: Some(pb::ConversationListQuery {
                project_id: scenario::PROJECT.into(),
                filter: "坏".into(),
            }),
            ..Default::default()
        }),
    )
    .await;
    settle(&app, &mut web).await;
    for i in 0..3 {
        let id = format!("c-new-{i}");
        d.append(f::project_conversation(&id, scenario::PROJECT));
        until(&app, &mut web, &format!("{id} 进侧栏"), |s| {
            s.sidebar.as_ref().is_some_and(|v| {
                v.projects
                    .iter()
                    .flat_map(|p| &p.recent)
                    .any(|c| c.conversation_id == id)
            })
        })
        .await;
    }
}

/// 2/F-3：断线造成的查询失败不算失败：重连后没有「数据没取到」的提示。
#[tokio::test(flavor = "multi_thread")]
async fn queries_cut_by_disconnect_are_not_failures() {
    let (d, _dir) = daemon(&["conversation-basic", "approval-states"]).await;
    let (app, mut web) = connect(&d, 5000).await;
    until(&app, &mut web, "连上", live).await;
    for _ in 0..3 {
        intent(
            &app,
            I::SetVisible(pb::SetVisible {
                sidebar: true,
                settings: true,
                conversation_list: Some(pb::ConversationListQuery {
                    project_id: scenario::PROJECT.into(),
                    filter: String::new(),
                }),
                ..Default::default()
            }),
        )
        .await;
        d.disconnect_all().await;
        settle(&app, &mut web).await;
        assert_eq!(
            web.state
                .connection
                .as_ref()
                .and_then(|c| c.query_notice.clone()),
            None
        );
    }
}
