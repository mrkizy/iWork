//! 提醒（T09 方案 2.8，第 7 节「提醒」一组）：Dock 角标与跳动、报在看哪、被叫到前台时跳转。连假后台走真协议。

#![cfg(unix)]

mod support;

use std::time::{Duration, Instant, SystemTime};

use iwork_client_core::{App, DirectDialer, Effect, Options, ShellEvent};
use iwork_fake_daemon::{FakeDaemon, fixtures as f, self_trusting_identity};
use iwork_proto::pb;
use pb::ui_intent::Kind as I;
use support::web::{Web, WebState};
use tokio::sync::mpsc;
use tokio::time::{sleep, timeout};

const WAIT: Duration = Duration::from_secs(20);

struct H {
    daemon: FakeDaemon,
    app: App,
    effects: mpsc::UnboundedReceiver<Effect>,
    _dir: tempfile::TempDir,
}

async fn start(scenarios: &[&str]) -> H {
    let dir = tempfile::Builder::new()
        .prefix("t09-remind")
        .tempdir_in("/tmp")
        .unwrap();
    let daemon = FakeDaemon::start(dir.path().join("d.sock")).await.unwrap();
    for s in scenarios {
        daemon.run_scenario(s).await.unwrap();
    }
    let dialer = DirectDialer::new(daemon.endpoint(), self_trusting_identity().unwrap(), "test");
    let (app, effects) = App::start(dialer, Options::default());
    H {
        daemon,
        app,
        effects,
        _dir: dir,
    }
}

async fn web(app: &App) -> Web {
    Web::new(app.attach().await.unwrap())
}

async fn intent(app: &App, kind: I) {
    let ack = app.intent(pb::UiIntent { kind: Some(kind) }).await;
    assert_eq!(ack.error, None);
}

fn visible(conversation: Option<&str>, pending: bool) -> I {
    I::SetVisible(pb::SetVisible {
        sidebar: true,
        pending,
        conversation_id: conversation.map(Into::into),
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

/// 收效果，直到满足条件（之前的照收不误，返回全部）。
async fn effects_until(
    rx: &mut mpsc::UnboundedReceiver<Effect>,
    what: &str,
    done: impl Fn(&[Effect]) -> bool,
) -> Vec<Effect> {
    let mut got = Vec::new();
    let r = timeout(WAIT, async {
        while !done(&got) {
            got.push(rx.recv().await.expect("效果通道关了"));
        }
    })
    .await;
    assert!(r.is_ok(), "等不到效果：{what}，已收到 {got:?}");
    got
}

/// 一段时间里收到的效果。
async fn effects_during(rx: &mut mpsc::UnboundedReceiver<Effect>, d: Duration) -> Vec<Effect> {
    let mut got = Vec::new();
    let _ = timeout(d, async {
        while let Some(e) = rx.recv().await {
            got.push(e);
        }
    })
    .await;
    got
}

fn badges(es: &[Effect]) -> Vec<Option<String>> {
    es.iter()
        .filter_map(|e| match e {
            Effect::SetBadge(b) => Some(b.clone()),
            _ => None,
        })
        .collect()
}

fn attentions(es: &[Effect]) -> usize {
    es.iter()
        .filter(|e| matches!(e, Effect::RequestAttention))
        .count()
}

fn new_approval(id: &str) -> pb::DomainEvent {
    f::approval(
        id,
        "c-approval",
        "t-approval",
        f::command_action("rm -rf build"),
        pb::ApprovalKind::Ask,
        &[],
    )
}

fn decided(id: &str) -> pb::DomainEvent {
    f::approval_decided(id, pb::ApprovalState::Denied, pb::ApprovalDecider::User)
}

// ---------- 角标与跳动 ----------

/// 角标文字与侧栏「待你处理」徽标是同一个函数算的，两处相等；启动时的第一份表不跳。
#[tokio::test(flavor = "multi_thread")]
async fn badge_equals_sidebar_pending_count() {
    let mut h = start(&["approval-states"]).await;
    let mut w = web(&h.app).await;
    intent(&h.app, visible(None, false)).await;
    until(&h.app, &mut w, "侧栏有待处理", |s| {
        s.sidebar
            .as_ref()
            .is_some_and(|v| v.pending_badge.is_some())
    })
    .await;
    let es = effects_until(&mut h.effects, "角标", |es| !badges(es).is_empty()).await;
    let sidebar = w
        .state
        .sidebar
        .as_ref()
        .unwrap()
        .pending_badge
        .clone()
        .unwrap();
    assert_eq!(
        badges(&es).last().unwrap().as_deref(),
        Some(sidebar.text.as_str())
    );
    assert_eq!(attentions(&es), 0, "第一份表不跳");
}

/// 处理完最后一项：角标清掉。
#[tokio::test(flavor = "multi_thread")]
async fn badge_cleared_at_zero() {
    let mut h = start(&["sidebar-dots"]).await;
    let _ = effects_until(&mut h.effects, "角标 1", |es| {
        badges(es).last() == Some(&Some("1".into()))
    })
    .await;
    h.daemon.append(decided("ap-dot"));
    let _ = effects_until(&mut h.effects, "角标清掉", |es| {
        badges(es).last() == Some(&None)
    })
    .await;
}

/// 断线期间：角标保留最后的数字，不清、不变。
#[tokio::test(flavor = "multi_thread")]
async fn badge_kept_while_disconnected() {
    let mut h = start(&["sidebar-dots"]).await;
    let _ = effects_until(&mut h.effects, "角标 1", |es| {
        badges(es).last() == Some(&Some("1".into()))
    })
    .await;
    h.daemon.stop_listening().await;
    let es = effects_during(&mut h.effects, Duration::from_secs(2)).await;
    assert!(badges(&es).is_empty(), "断线期间不动角标：{es:?}");
}

/// 新出现的条目才跳：同一份表重查不跳；一项处理掉同时新来一项，数字没变也跳。
#[tokio::test(flavor = "multi_thread")]
async fn attention_on_new_pending_id_only() {
    let mut h = start(&["sidebar-dots"]).await;
    let _ = effects_until(&mut h.effects, "第一份表", |es| !badges(es).is_empty()).await;
    h.daemon.append(new_approval("ap-a"));
    let _ = effects_until(&mut h.effects, "新来一项就跳", |es| {
        attentions(es) == 1
    })
    .await;
    // 不相干的事件触发重查：同一份表，不跳
    for _ in 0..3 {
        h.daemon.append(f::conversation_read("c-waiting", 0));
    }
    let es = effects_during(&mut h.effects, Duration::from_millis(1500)).await;
    assert_eq!(attentions(&es), 0, "同一份表重查不跳：{es:?}");
    // 处理掉一项、同时新来一项：数字不变也跳
    h.daemon.append(decided("ap-a"));
    h.daemon.append(new_approval("ap-b"));
    let es = effects_until(&mut h.effects, "换了一项也跳", |es| {
        attentions(es) >= 1
    })
    .await;
    assert!(
        badges(&es)
            .iter()
            .all(|b| b.as_deref() == Some("2") || b.as_deref() == Some("1")),
        "{es:?}"
    );
}

/// 重连后同一项不再跳（上一份表跨重连保留）；断线期间新来的照跳。
#[tokio::test(flavor = "multi_thread")]
async fn attention_not_repeated_after_reconnect() {
    let mut h = start(&["sidebar-dots"]).await;
    let _ = effects_until(&mut h.effects, "第一份表", |es| !badges(es).is_empty()).await;
    h.daemon.disconnect_all().await;
    let es = effects_during(&mut h.effects, Duration::from_secs(2)).await;
    assert_eq!(attentions(&es), 0, "重连拿到同一份表不跳：{es:?}");
    h.daemon.stop_listening().await;
    h.daemon.append(new_approval("ap-while-away"));
    h.daemon.disconnect_all().await;
    let _ = effects_until(&mut h.effects, "断线期间新来的照跳", |es| {
        attentions(es) == 1
    })
    .await;
}

/// 没有网页在取帧（窗口关着）：效果照常发。
#[tokio::test(flavor = "multi_thread")]
async fn effects_flow_with_no_window() {
    let mut h = start(&["sidebar-dots"]).await;
    h.app.shell(ShellEvent::WindowOpen(false));
    let _ = effects_until(&mut h.effects, "第一份表", |es| !badges(es).is_empty()).await;
    h.daemon.append(new_approval("ap-closed"));
    let _ = effects_until(&mut h.effects, "窗口关着也跳", |es| {
        attentions(es) == 1
    })
    .await;
}

// ---------- 报在看哪 ----------

async fn reports_until(d: &FakeDaemon, what: &str, done: impl Fn(&[pb::ReportPresence]) -> bool) {
    let deadline = Instant::now() + WAIT;
    while !done(&d.presence_reports()) {
        assert!(
            Instant::now() < deadline,
            "等不到在看哪：{what}，已收到 {:?}",
            d.presence_reports()
        );
        sleep(Duration::from_millis(20)).await;
    }
}

fn last(d: &FakeDaemon) -> pb::ReportPresence {
    d.presence_reports().last().cloned().unwrap_or_default()
}

/// 三个输入各变一次：窗口焦点、路由（对话页 / 待你处理页）、对话视图是否已加载完。
#[tokio::test(flavor = "multi_thread")]
async fn presence_follows_focus_route_ready() {
    let h = start(&["conversation-basic"]).await;
    reports_until(&h.daemon, "连上后发一次", |r| !r.is_empty()).await;
    assert_eq!(last(&h.daemon), pb::ReportPresence::default());
    h.app.shell(ShellEvent::Focused(true));
    reports_until(&h.daemon, "焦点", |r| {
        r.last().is_some_and(|p| p.window_focused)
    })
    .await;
    intent(&h.app, visible(Some("c-basic"), false)).await;
    reports_until(&h.daemon, "对话加载完才算在看", |r| {
        r.last()
            .is_some_and(|p| p.viewing_conversation_id.as_deref() == Some("c-basic"))
    })
    .await;
    intent(&h.app, visible(None, true)).await;
    reports_until(&h.daemon, "待你处理页", |r| {
        r.last()
            .is_some_and(|p| p.viewing_pending_list && p.viewing_conversation_id.is_none())
    })
    .await;
}

/// 离开立刻报：失焦后很快就收到，不等 500ms。
#[tokio::test(flavor = "multi_thread")]
async fn presence_leave_is_immediate() {
    let h = start(&["conversation-basic"]).await;
    h.app.shell(ShellEvent::Focused(true));
    reports_until(&h.daemon, "焦点", |r| {
        r.last().is_some_and(|p| p.window_focused)
    })
    .await;
    let n = h.daemon.presence_reports().len();
    let t = Instant::now();
    h.app.shell(ShellEvent::Focused(false));
    reports_until(&h.daemon, "失焦", |r| {
        r.len() > n && !r[r.len() - 1].window_focused
    })
    .await;
    assert!(
        t.elapsed() < Duration::from_millis(400),
        "离开立刻报：{:?}",
        t.elapsed()
    );
}

/// 到来延后报：获得焦点后 500ms 内不报；中途又失焦就不报到来。
#[tokio::test(flavor = "multi_thread")]
async fn presence_arrive_is_debounced() {
    let h = start(&["conversation-basic"]).await;
    reports_until(&h.daemon, "连上后发一次", |r| !r.is_empty()).await;
    let n = h.daemon.presence_reports().len();
    h.app.shell(ShellEvent::Focused(true));
    sleep(Duration::from_millis(250)).await;
    h.app.shell(ShellEvent::Focused(false));
    sleep(Duration::from_millis(700)).await;
    assert_eq!(h.daemon.presence_reports().len(), n, "闪一下就走：不报到来");
    let t = Instant::now();
    h.app.shell(ShellEvent::Focused(true));
    reports_until(&h.daemon, "稳定后报到来", |r| r.len() > n).await;
    assert!(
        t.elapsed() >= Duration::from_millis(450),
        "到来要等稳定：{:?}",
        t.elapsed()
    );
}

/// 重新连上后发一次当前值；不进在飞命令表（断线不重发旧值、不算重发的命令）。
#[tokio::test(flavor = "multi_thread")]
async fn presence_resent_after_reconnect_and_not_in_outbox() {
    let h = start(&["conversation-basic"]).await;
    h.app.shell(ShellEvent::Focused(true));
    reports_until(&h.daemon, "焦点", |r| {
        r.last().is_some_and(|p| p.window_focused)
    })
    .await;
    let n = h.daemon.presence_reports().len();
    let resent = h.app.stats().borrow().resent_commands;
    h.daemon.disconnect_all().await;
    reports_until(&h.daemon, "重连后发一次当前值", |r| r.len() > n).await;
    assert!(last(&h.daemon).window_focused);
    assert_eq!(
        h.app.stats().borrow().resent_commands,
        resent,
        "不进在飞命令表：重连时没有重发它"
    );
}

/// 关窗：什么都看不到。
#[tokio::test(flavor = "multi_thread")]
async fn presence_false_when_window_closed() {
    let h = start(&["conversation-basic"]).await;
    h.app.shell(ShellEvent::Focused(true));
    intent(&h.app, visible(Some("c-basic"), false)).await;
    reports_until(&h.daemon, "在看对话", |r| {
        r.last()
            .is_some_and(|p| p.viewing_conversation_id.is_some())
    })
    .await;
    h.app.shell(ShellEvent::WindowOpen(false));
    reports_until(&h.daemon, "关窗", |r| {
        r.last()
            .is_some_and(|p| *p == pb::ReportPresence::default())
    })
    .await;
}

// ---------- 被叫到前台时跳转 ----------

/// 同一个 token 只跳一次；跳的目标与「待你处理」页那一项同一个行身份。
#[tokio::test(flavor = "multi_thread")]
async fn open_target_navigates_once_per_token_like_pending_list() {
    let h = start(&["approval-states"]).await;
    let mut w = web(&h.app).await;
    intent(&h.app, visible(None, true)).await;
    until(&h.app, &mut w, "待你处理页", |s| {
        s.pending.as_ref().is_some_and(|p| !p.entries.is_empty())
    })
    .await;
    let entry = w.state.pending.as_ref().unwrap().entries[0].clone();
    h.daemon
        .set_open_target(&entry.item_id, entry.conversation_id.as_deref());
    h.app.shell(ShellEvent::Focused(true));
    until(&h.app, &mut w, "跳过去", |s| s.navigate.is_some()).await;
    let nav = w.state.navigate.clone().unwrap();
    assert_eq!(Some(nav.conversation_id.clone()), entry.conversation_id);
    assert_eq!(
        Some(nav.row_key.clone()),
        entry.row_key,
        "和列表项同一个行身份"
    );
    // 同一个 token 再被问到：不再跳
    h.app.shell(ShellEvent::Focused(false));
    h.app.shell(ShellEvent::Focused(true));
    sleep(Duration::from_millis(800)).await;
    let _ = timeout(Duration::from_millis(300), h.app.next_frame(w.request()))
        .await
        .map(|c| c.map(|c| w.accept(&c)));
    assert_eq!(w.state.navigate.as_ref().map(|n| n.seq), Some(nav.seq));
}

/// 30 秒以前点的不算。
#[tokio::test(flavor = "multi_thread")]
async fn open_target_expires() {
    let h = start(&["approval-states"]).await;
    let mut w = web(&h.app).await;
    intent(&h.app, visible(None, false)).await;
    until(&h.app, &mut w, "连上", |s| s.sidebar.is_some()).await;
    h.daemon.set_open_target_at(
        "ap-pending",
        Some("c-approval"),
        SystemTime::now() - Duration::from_secs(31),
    );
    h.app.shell(ShellEvent::Focused(true));
    sleep(Duration::from_millis(800)).await;
    let _ = timeout(Duration::from_millis(300), h.app.next_frame(w.request()))
        .await
        .map(|c| c.map(|c| w.accept(&c)));
    assert_eq!(w.state.navigate, None);
}

/// 没有对话归属的项（AGENTS.md 草稿）：打开「待你处理」页。
#[tokio::test(flavor = "multi_thread")]
async fn open_target_without_conversation_opens_pending() {
    let h = start(&["empty"]).await;
    let mut w = web(&h.app).await;
    intent(&h.app, visible(None, false)).await;
    until(&h.app, &mut w, "连上", |s| s.sidebar.is_some()).await;
    h.daemon.set_open_target("draft-1", None);
    h.app.shell(ShellEvent::Focused(true));
    until(&h.app, &mut w, "打开待你处理页", |s| {
        s.navigate.as_ref().is_some_and(|n| n.pending_list)
    })
    .await;
}

/// 后台没记目标（token 0）：不跳。
#[tokio::test(flavor = "multi_thread")]
async fn no_target_means_no_navigation() {
    let h = start(&["approval-states"]).await;
    let mut w = web(&h.app).await;
    intent(&h.app, visible(None, false)).await;
    until(&h.app, &mut w, "连上", |s| s.sidebar.is_some()).await;
    h.app.shell(ShellEvent::Focused(true));
    sleep(Duration::from_millis(800)).await;
    let _ = timeout(Duration::from_millis(300), h.app.next_frame(w.request()))
        .await
        .map(|c| c.map(|c| w.accept(&c)));
    assert_eq!(w.state.navigate, None);
}
