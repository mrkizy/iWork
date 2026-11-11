//! 输入框一带的接线（步 7a）：连假后台走真协议。
//! - 「运行时 · 模型」菜单：打开对话后取到这个运行时的模型清单，选中项与说明来自 Rust；
//! - 新建对话：建好以后网页收到一个跳到新对话的请求。

#![cfg(unix)]

mod support;

use std::time::Duration;

use iwork_client_core::{App, DirectDialer, Options};
use iwork_fake_daemon::{FakeDaemon, self_trusting_identity};
use iwork_proto::pb;
use pb::ui_intent::Kind as I;
use support::web::{Web, WebState};
use tokio::time::timeout;

const WAIT: Duration = Duration::from_secs(20);

async fn start(scenario: &str) -> (FakeDaemon, App, Web, tempfile::TempDir) {
    let dir = tempfile::Builder::new()
        .prefix("t09-7a")
        .tempdir_in("/tmp")
        .unwrap();
    let daemon = FakeDaemon::start(dir.path().join("d.sock")).await.unwrap();
    daemon.run_scenario(scenario).await.unwrap();
    let dialer = DirectDialer::new(daemon.endpoint(), self_trusting_identity().unwrap(), "test");
    let (app, _effects) = App::start(dialer, Options::default());
    let web = Web::new(app.attach().await.unwrap());
    (daemon, app, web, dir)
}

async fn visible(app: &App, conversation: Option<&str>) {
    let ack = app
        .intent(pb::UiIntent {
            kind: Some(I::SetVisible(pb::SetVisible {
                sidebar: true,
                conversation_id: conversation.map(Into::into),
                ..Default::default()
            })),
        })
        .await;
    assert_eq!(ack.error, None);
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

fn menu(s: &WebState) -> Option<&pb::ModelMenuView> {
    s.composer.as_ref()?.model_menu.as_ref()
}

#[tokio::test(flavor = "multi_thread")]
async fn model_menu_fetches_catalog_for_the_open_conversation() {
    let (_d, app, mut web, _dir) = start("conversation-basic").await;
    visible(&app, Some("c-basic")).await;
    until(&app, &mut web, "取到模型清单", |s| {
        menu(s).is_some_and(|m| !m.models_loading && !m.models.is_empty())
    })
    .await;
    let m = menu(&web.state).unwrap();
    let labels: Vec<_> = m.models.iter().map(|c| c.label.as_str()).collect();
    assert_eq!(
        labels,
        ["Fake Large", "Fake Small"],
        "清单来自后台的 ListModels"
    );
    let selected = |v: &[pb::MenuChoice]| -> Vec<String> {
        v.iter()
            .filter(|c| c.selected)
            .map(|c| c.value.clone())
            .collect()
    };
    // 这个对话没单独选过模型：选中的是助手配置里的（假后台的内置助手配的是 fake-large / medium）
    assert_eq!(selected(&m.models), ["fake-large"]);
    assert_eq!(selected(&m.efforts), ["medium"]);
    assert_eq!(m.custom_model, "", "当前模型在清单里，不算手填");
    // 对话已经开始（有回合）：运行时一节整节不可选，提示写明原因
    assert!(!m.runtimes.is_empty());
    assert!(
        m.runtimes
            .iter()
            .all(|r| !r.enabled && r.tip == "要换运行时请开新对话")
    );
    assert!(m.runtimes.iter().any(|r| r.selected));
}

#[tokio::test(flavor = "multi_thread")]
async fn new_conversation_navigates_once_created() {
    let (_d, app, mut web, _dir) = start("empty").await;
    visible(&app, None).await;
    until(&app, &mut web, "连上", |s| {
        s.sidebar.is_some()
            && s.connection
                .as_ref()
                .is_some_and(|c| c.state == pb::ConnectionState::Live as i32)
    })
    .await;
    assert_eq!(web.state.navigate, None, "对照：没建对话前没有跳转");
    let ack = app
        .intent(pb::UiIntent {
            kind: Some(I::CreateConversation(pb::CreateConversationIntent {
                project_id: iwork_fake_daemon::scenario::PROJECT.into(),
            })),
        })
        .await;
    assert_eq!(ack.error, None);
    until(&app, &mut web, "跳到新对话", |s| s.navigate.is_some()).await;
    let nav = web.state.navigate.clone().unwrap();
    // 跳过去的就是侧栏里新出现的那个对话
    until(&app, &mut web, "侧栏出现新对话", |s| {
        s.sidebar.as_ref().is_some_and(|v| {
            v.projects.iter().any(|p| {
                p.recent
                    .iter()
                    .any(|c| c.conversation_id == nav.conversation_id)
            })
        })
    })
    .await;
    assert!(nav.row_key.is_empty(), "停在最新");
}

#[tokio::test(flavor = "multi_thread")]
async fn model_catalog_failure_notice_clears_once_it_loads() {
    let (d, app, mut web, _dir) = start("conversation-basic").await;
    d.fail_queries(Some(Box::new(|q: &pb::Query| {
        matches!(q.kind, Some(pb::query::Kind::ListModels(_)))
    })));
    visible(&app, Some("c-basic")).await;
    let notice = |s: &WebState| s.connection.as_ref().and_then(|c| c.query_notice.clone());
    until(&app, &mut web, "取不到模型清单时挂提示", |s| {
        notice(s).is_some()
    })
    .await;
    assert!(
        menu(&web.state).is_some_and(|m| m.models.is_empty()),
        "对照：确实没取到"
    );
    d.fail_queries(None);
    until(&app, &mut web, "退避后取到清单", |s| {
        menu(s).is_some_and(|m| !m.models.is_empty())
    })
    .await;
    until(&app, &mut web, "提示清掉", |s| notice(s).is_none()).await;
}
