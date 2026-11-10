//! 会话循环连假后台（方案 2.6：假数据走真协议）：验收 2 与第 7 节里要连后台的各条。
//! 客户端永远用真 `Client`；假后台是同一进程里的真 ipc `Server`（`self_trusting_identity`，不用 DevTrust）。

// 假后台只在 Unix 上监听（iwork-fake-daemon）；M1 暂不支持 Windows。
#![cfg(unix)]

mod support;

use std::collections::BTreeSet;
use std::time::Duration;

use iwork_client_core::{App, DirectDialer, Effect, Options};
use iwork_fake_daemon::{FakeDaemon, fixtures as f, scenario, self_trusting_identity};
use iwork_proto::pb;
use pb::ui_intent::Kind as I;
use support::web::{Web, WebState};
use tokio::sync::mpsc;
use tokio::time::timeout;

const WAIT: Duration = Duration::from_secs(20);

struct H {
    daemon: FakeDaemon,
    app: App,
    web: Web,
    effects: mpsc::UnboundedReceiver<Effect>,
    _dir: tempfile::TempDir,
}

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

async fn connect(daemon: &FakeDaemon) -> (App, Web, mpsc::UnboundedReceiver<Effect>) {
    let dialer = DirectDialer::new(daemon.endpoint(), self_trusting_identity().unwrap(), "test");
    let (app, effects) = App::start(dialer, Options { chunk_max: 4096 });
    let epoch = app.attach().await.unwrap();
    (app, Web::new(epoch), effects)
}

async fn start(scenarios: &[&str]) -> H {
    let (daemon, dir) = daemon(scenarios).await;
    let (app, web, effects) = connect(&daemon).await;
    H {
        daemon,
        app,
        web,
        effects,
        _dir: dir,
    }
}

fn visible(conversation: Option<&str>) -> pb::UiIntent {
    pb::UiIntent {
        kind: Some(I::SetVisible(pb::SetVisible {
            sidebar: true,
            conversation_id: conversation.map(Into::into),
            pending: true,
            ..Default::default()
        })),
    }
}

async fn ok(app: &App, kind: I) {
    let ack = app.intent(pb::UiIntent { kind: Some(kind) }).await;
    assert_eq!(ack.error, None);
}

/// 一直取帧，直到 `cond` 成立。
async fn until(app: &App, web: &mut Web, what: &str, cond: impl Fn(&WebState) -> bool) {
    let r = timeout(WAIT, async {
        while !cond(&web.state) {
            match timeout(Duration::from_millis(500), app.next_frame(web.request())).await {
                Ok(Ok(c)) => {
                    web.accept(&c);
                }
                Ok(Err(e)) => panic!("取帧失败：{e:?}"),
                Err(_) => {}
            }
        }
    })
    .await;
    assert!(
        r.is_ok(),
        "等不到：{what}；当前连接 {:?}",
        web.state
            .connection
            .as_ref()
            .map(|c| (c.state, c.title.clone()))
    );
}

fn live(s: &WebState) -> bool {
    s.connection
        .as_ref()
        .is_some_and(|c| c.state == pb::ConnectionState::Live as i32)
}

/// 网页上的稳定视图（去掉连接条）：比对重连前后与新客户端用。
fn stable(s: &WebState) -> WebState {
    WebState {
        connection: None,
        ..s.clone()
    }
}

/// 等界面稳定：连上且一段时间内不再变。
async fn settle(app: &App, web: &mut Web) {
    until(app, web, "连上", live).await;
    loop {
        let before = stable(&web.state);
        let _ = timeout(Duration::from_millis(400), async {
            loop {
                if let Ok(c) = app.next_frame(web.request()).await {
                    web.accept(&c);
                }
            }
        })
        .await;
        if stable(&web.state) == before {
            return;
        }
    }
}

/// 验收 2：模拟后台服务重启，界面状态与重启前一致，无重复消息；也等于一个新客户端看到的。
#[tokio::test(flavor = "multi_thread")]
async fn reconnect_keeps_view() {
    let mut h = start(&["conversation-basic"]).await;
    ok(&h.app, visible(Some("c-basic")).kind.unwrap()).await;
    settle(&h.app, &mut h.web).await;
    let before = stable(&h.web.state);
    assert!(!before.rows.is_empty(), "对照：时间线有行");

    h.daemon.disconnect_all().await;
    until(&h.app, &mut h.web, "掉线", |s| !live(s)).await;
    settle(&h.app, &mut h.web).await;
    assert_eq!(stable(&h.web.state), before);

    // 后台在断线后又来了新消息：重连后有，且每条只一次。
    h.daemon.disconnect_all().await;
    h.daemon
        .append(f::user_message("m-after", "c-basic", "断线期间发的"));
    // 先等新消息出现（等到断线被察觉、重连、续传都走完），再等界面稳定后查重复。
    until(&h.app, &mut h.web, "断线期间的新消息", |s| {
        s.rows.contains_key("m-after")
    })
    .await;
    settle(&h.app, &mut h.web).await;
    let keys = h.web.row_keys();
    let unique: BTreeSet<_> = keys.iter().collect();
    assert_eq!(unique.len(), keys.len(), "没有重复行");
    assert!(keys.contains(&"m-after".to_owned()));

    let (fresh_app, mut fresh, _e) = connect(&h.daemon).await;
    ok(&fresh_app, visible(Some("c-basic")).kind.unwrap()).await;
    settle(&fresh_app, &mut fresh).await;
    assert_eq!(stable(&h.web.state).rows, stable(&fresh.state).rows);
    assert_eq!(stable(&h.web.state).sidebar, stable(&fresh.state).sidebar);
}

/// S9：命令发出后断线，按原 command_id 重发；后台按 command_id 去重，每条只落一次。
#[tokio::test(flavor = "multi_thread")]
async fn inflight_command_resent_once() {
    let mut h = start(&["conversation-basic"]).await;
    ok(&h.app, visible(Some("c-basic")).kind.unwrap()).await;
    settle(&h.app, &mut h.web).await;
    let stats = h.app.stats();
    let mut sends = Vec::new();
    for i in 0..30 {
        let app = h.app.clone();
        sends.push(tokio::spawn(async move {
            app.intent(pb::UiIntent {
                kind: Some(I::SendMessage(pb::SendMessageIntent {
                    conversation_id: "c-basic".into(),
                    text: format!("burst-{i}"),
                    attachment_ids: vec![],
                })),
            })
            .await
        }));
    }
    tokio::task::yield_now().await;
    h.daemon.disconnect_all().await;
    for s in sends {
        let ack = timeout(WAIT, s).await.unwrap().unwrap();
        // 断线前没送到会话的命令按 D8 当场拒绝；送出去的最终都受理。
        if let Some(e) = ack.error {
            assert_eq!(e.code, "offline");
        }
    }
    settle(&h.app, &mut h.web).await;
    let page = h.daemon.query(&pb::Query {
        kind: Some(pb::query::Kind::GetConversation(pb::GetConversation {
            conversation_id: "c-basic".into(),
            before_message_id: None,
            limit: 200,
        })),
    });
    let Some(pb::query_result::Kind::GetConversation(r)) = page.kind else {
        panic!()
    };
    let texts: Vec<String> = r
        .messages
        .iter()
        .filter_map(|m| match m.body.as_ref()?.content.as_ref()? {
            pb::message_body::Content::InlineText(t) if t.starts_with("burst-") => Some(t.clone()),
            _ => None,
        })
        .collect();
    let unique: BTreeSet<_> = texts.iter().collect();
    assert_eq!(unique.len(), texts.len(), "每条只落一次：{texts:?}");
    assert!(
        stats.borrow().resent_commands > 0,
        "这次没有命令在飞时断线，测试没证明重发"
    );
}

/// S8：卡片已作废时点「允许」：后台回错误，原样显示成提示；卡片按事件置灰。
#[tokio::test(flavor = "multi_thread")]
async fn decide_after_void_shows_error() {
    let mut h = start(&["approval-states"]).await;
    ok(&h.app, visible(Some("c-approval")).kind.unwrap()).await;
    settle(&h.app, &mut h.web).await;
    let ack = h
        .app
        .intent(pb::UiIntent {
            kind: Some(I::DecideApproval(pb::DecideApprovalIntent {
                approval_id: "ap-voided".into(),
                allow: true,
                remember: 0,
            })),
        })
        .await;
    assert_eq!(
        ack.error.map(|e| e.code).as_deref(),
        Some("approval_not_pending")
    );
    let Some(pb::row_view::Row::Approval(a)) = &h.web.state.rows["ap-voided"].row else {
        panic!()
    };
    assert!(a.muted && !a.can_decide);
}

/// S14 第 2 条：后台换了身份（版本不一致）时停在中性提示并退避；后台换回来后自动连上。
#[tokio::test(flavor = "multi_thread")]
async fn rejected_then_recovers_after_daemon_restart() {
    let mut h = start(&["empty"]).await;
    ok(&h.app, visible(None).kind.unwrap()).await;
    settle(&h.app, &mut h.web).await;
    // set_rejecting 本身就断开现有连接（像后台换了版本重启）。
    h.daemon.set_rejecting(true).await;
    until(&h.app, &mut h.web, "中性提示", |s| {
        s.connection.as_ref().is_some_and(|c| {
            c.state == pb::ConnectionState::Waiting as i32
                && c.title == "后台服务与客户端版本不一致"
        })
    })
    .await;
    h.daemon.set_rejecting(false).await;
    until(&h.app, &mut h.web, "恢复", live).await;
}

/// S5：改名后侧栏更新。T10 补上 `ConversationRenamed` 前，用假后台直接改摘要里的标题，再发一条已有的、
/// 命中侧栏订阅的事件（`ConversationRead`）触发失效；守的是「失效 → 重查 → 新标题」这条通用路。
#[tokio::test(flavor = "multi_thread")]
async fn rename_refreshes_sidebar() {
    let mut h = start(&["conversation-basic"]).await;
    ok(&h.app, visible(None).kind.unwrap()).await;
    settle(&h.app, &mut h.web).await;
    h.daemon.mutate_tables(|t| {
        let row = t.conversations.get_mut("c-basic").unwrap();
        row.conversation.title = scenario::RENAMED_TITLE.into();
        row.conversation.title_source = pb::TitleSource::User.into();
    });
    h.daemon.append(f::conversation_read("c-basic", 0));
    until(&h.app, &mut h.web, "侧栏新标题", |s| {
        s.sidebar.as_ref().is_some_and(|v| {
            v.projects
                .iter()
                .flat_map(|p| &p.recent)
                .any(|c| c.title == scenario::RENAMED_TITLE)
        })
    })
    .await;
}

/// 未读：对话可见、窗口在前台、视图 Ready 时发 MarkRead，两级徽标随重查归零；不在前台时不发。
#[tokio::test(flavor = "multi_thread")]
async fn mark_read_when_visible_and_focused() {
    let mut h = start(&["conversation-basic"]).await;
    ok(&h.app, visible(Some("c-basic")).kind.unwrap()).await;
    settle(&h.app, &mut h.web).await;
    let unread = |s: &WebState| {
        s.sidebar
            .as_ref()
            .map_or(u32::MAX, |v| v.projects.iter().map(|p| p.unread).sum())
    };
    assert!(unread(&h.web.state) > 0, "对照：没聚焦前有未读");
    ok(&h.app, I::SetFocused(pb::SetFocused { focused: true })).await;
    until(&h.app, &mut h.web, "徽标归零", |s| unread(s) == 0).await;
}

/// 系统通知：窗口不在前台时新出现的待处理项发一次，之后不重复；启动快照带来的不发。
#[tokio::test(flavor = "multi_thread")]
async fn notify_once_per_item() {
    let mut h = start(&["approval-states"]).await;
    ok(&h.app, visible(None).kind.unwrap()).await;
    settle(&h.app, &mut h.web).await;
    assert!(
        h.effects.try_recv().is_err(),
        "启动快照里的待处理项不发通知"
    );
    h.daemon.append(f::approval(
        "ap-new",
        "c-approval",
        "t-approval",
        f::command_action("rm -rf build"),
        pb::ApprovalKind::Ask,
        &[],
    ));
    let e = timeout(WAIT, h.effects.recv()).await.unwrap().unwrap();
    assert!(matches!(e, Effect::Notify { conversation_id: Some(ref c), .. } if c == "c-approval"));
    // 再来几个不相干的事件触发重查：不重复通知。
    for _ in 0..3 {
        h.daemon.append(f::conversation_read("c-approval", 0));
        settle(&h.app, &mut h.web).await;
    }
    assert!(h.effects.try_recv().is_err());
}

/// 在飞查询表：发出后断线，重连后每种用途最终对齐（翻页、规则页、「全部对话」）。
#[tokio::test(flavor = "multi_thread")]
async fn pending_query_table_cleared_on_close() {
    let mut h = start(&["conversation-basic", "approval-states"]).await;
    // 规则页与「全部对话」：打开即发查询，紧接着断线。
    h.app
        .intent(pb::UiIntent {
            kind: Some(I::SetVisible(pb::SetVisible {
                sidebar: true,
                settings: true,
                conversation_list: Some(pb::ConversationListQuery {
                    project_id: scenario::PROJECT.into(),
                    filter: String::new(),
                }),
                ..Default::default()
            })),
        })
        .await;
    until(&h.app, &mut h.web, "连上", live).await;
    h.daemon.disconnect_all().await;
    until(&h.app, &mut h.web, "规则与列表对齐", |s| {
        s.settings
            .as_ref()
            .is_some_and(|x| !x.rules_empty_text.is_empty() || !x.rule_groups.is_empty())
            && s.list
                .as_ref()
                .is_some_and(|l| !l.loading && !l.items.is_empty())
    })
    .await;
}
