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
    let (installed, upcoming): (Vec<_>, Vec<_>) =
        m.runtimes.iter().partition(|r| !r.value.is_empty());
    assert!(!installed.is_empty());
    assert!(
        installed
            .iter()
            .all(|r| !r.enabled && r.tip == "要换运行时请开新对话")
    );
    assert!(installed.iter().any(|r| r.selected));
    // 还没接入的运行时灰着列在末尾（画布 01-Main-modelmenu）
    assert_eq!(
        upcoming
            .iter()
            .map(|r| (r.label.as_str(), r.enabled, r.tip.as_str()))
            .collect::<Vec<_>>(),
        [("Codex", false, "M2 接入")]
    );
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

// ---------- 首页：新建对话并发出首条消息（审查 2a 1/F-3、F-9） ----------

async fn home(app: &App, project: &str) {
    let ack = app
        .intent(pb::UiIntent {
            kind: Some(I::SetVisible(pb::SetVisible {
                sidebar: true,
                home: Some(pb::HomeQuery {
                    project_id: project.into(),
                }),
                ..Default::default()
            })),
        })
        .await;
    assert_eq!(ack.error, None);
}

async fn act(app: &App, kind: I) -> pb::IntentAck {
    app.intent(pb::UiIntent { kind: Some(kind) }).await
}

fn start_intent(text: &str) -> I {
    I::StartConversation(pb::StartConversationIntent {
        project_id: iwork_fake_daemon::scenario::PROJECT.into(),
        text: text.into(),
    })
}

/// 假后台里每个对话收到的用户消息正文。
fn messages_by_conversation(d: &FakeDaemon) -> Vec<(String, Vec<String>)> {
    let mut out = Vec::new();
    d.mutate_tables(|t| {
        out = t
            .conversations
            .iter()
            .map(|(id, row)| {
                let texts = row
                    .messages
                    .iter()
                    .filter_map(|m| match m.body.as_ref()?.content.as_ref()? {
                        pb::message_body::Content::InlineText(t) => Some(t.clone()),
                        _ => None,
                    })
                    .collect();
                (id.clone(), texts)
            })
            .filter(|(_, v): &(String, Vec<String>)| !v.is_empty())
            .collect();
    });
    out
}

async fn live(app: &App, web: &mut Web) {
    until(app, web, "连上", |s| {
        s.connection
            .as_ref()
            .is_some_and(|c| c.state == pb::ConnectionState::Live as i32)
    })
    .await;
}

#[tokio::test(flavor = "multi_thread")]
async fn home_start_creates_sets_choices_sends_and_navigates() {
    let (d, app, mut web, _dir) = start("empty").await;
    home(&app, "").await;
    live(&app, &mut web).await;
    // 没声明项目：Rust 挑侧栏第一个；首页的输入框有模型菜单与档位
    until(&app, &mut web, "首页带输入框与模型清单", |s| {
        s.home.as_ref().is_some_and(|h| {
            h.composer
                .as_ref()
                .and_then(|c| c.model_menu.as_ref())
                .is_some_and(|m| !m.models.is_empty())
        })
    })
    .await;
    let h = web.state.home.clone().unwrap();
    assert_eq!(h.project_id, iwork_fake_daemon::scenario::PROJECT);
    assert_eq!(h.project_name, "demo");
    let c = h.composer.unwrap();
    assert!(c.can_send && !c.runtime_locked);
    assert_eq!(
        c.approval_mode.unwrap().current,
        pb::ApprovalMode::Manual as i32,
        "没选过：项目默认"
    );

    // 首页选模型与档位：记在 Rust，视图跟着变
    let ack = act(
        &app,
        I::SetHomeModel(pb::HomeModelIntent {
            model_id: "fake-small".into(),
            effort: None,
        }),
    )
    .await;
    assert_eq!(ack.error, None);
    let ack = act(
        &app,
        I::SetHomeApprovalMode(pb::HomeApprovalModeIntent {
            mode: pb::ApprovalMode::Auto as i32,
        }),
    )
    .await;
    assert_eq!(ack.error, None);
    until(&app, &mut web, "首页显示选过的值", |s| {
        s.home
            .as_ref()
            .and_then(|h| h.composer.as_ref())
            .is_some_and(|c| {
                c.runtime_model_label.contains("Fake Small")
                    || c.runtime_model_label.contains("fake-small")
            })
    })
    .await;

    assert_eq!(act(&app, start_intent("第一条")).await.error, None);
    until(&app, &mut web, "发出后跳过去", |s| {
        s.navigate
            .as_ref()
            .is_some_and(|n| n.started_project_id.is_some())
    })
    .await;
    let nav = web.state.navigate.clone().unwrap();
    assert_eq!(
        nav.started_project_id.as_deref(),
        Some(iwork_fake_daemon::scenario::PROJECT)
    );
    assert_eq!(
        messages_by_conversation(&d),
        vec![(nav.conversation_id.clone(), vec!["第一条".to_owned()])],
        "首条消息发进了新建的对话"
    );
    let mut conv = None;
    d.mutate_tables(|t| conv = t.conversations.get(&nav.conversation_id).cloned());
    let conv = conv.unwrap().conversation;
    assert_eq!(
        conv.model_override.map(|m| m.model_id).as_deref(),
        Some("fake-small")
    );
    assert_eq!(conv.approval_mode, Some(pb::ApprovalMode::Auto as i32));
    until(&app, &mut web, "首页回到默认、不再转圈", |s| {
        s.home.as_ref().is_some_and(|h| {
            !h.starting
                && h.composer
                    .as_ref()
                    .and_then(|c| c.approval_mode.as_ref())
                    .is_some_and(|m| m.current == pb::ApprovalMode::Manual as i32)
        })
    })
    .await;
}

/// 原来由网页编排时，任何跳转都会把草稿发出去。现在只有配上这次受理范围的对话才收到首条消息。
#[tokio::test(flavor = "multi_thread")]
async fn home_start_ignores_other_navigation() {
    let (d, app, mut web, _dir) = start("empty").await;
    home(&app, "").await;
    live(&app, &mut web).await;
    // 侧栏「新对话」与首页发出同时进行：前者先跳
    let open = act(
        &app,
        I::CreateConversation(pb::CreateConversationIntent {
            project_id: iwork_fake_daemon::scenario::PROJECT.into(),
        }),
    );
    let started = act(&app, start_intent("只发给新建的那个"));
    let (a, b) = tokio::join!(open, started);
    assert_eq!((a.error, b.error), (None, None));
    until(&app, &mut web, "首页那次收尾", |s| {
        s.navigate
            .as_ref()
            .is_some_and(|n| n.started_project_id.is_some())
    })
    .await;
    let target = web.state.navigate.clone().unwrap().conversation_id;
    let mut all = Vec::new();
    d.mutate_tables(|t| all = t.conversations.keys().cloned().collect());
    let created: Vec<_> = all
        .iter()
        .filter(|id| **id != iwork_fake_daemon::scenario::IWORK_DM)
        .collect();
    assert_eq!(created.len(), 2, "对照：确实建了两个对话");
    assert_eq!(
        messages_by_conversation(&d),
        vec![(target, vec!["只发给新建的那个".to_owned()])]
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn home_start_rejected_keeps_draft_and_reports() {
    let (d, app, mut web, _dir) = start("empty").await;
    home(&app, "").await;
    live(&app, &mut web).await;
    until(&app, &mut web, "首页", |s| {
        s.home.as_ref().is_some_and(|h| !h.project_id.is_empty())
    })
    .await;
    // 客户端还认这个项目，后台已经没有了：新建被拒
    d.mutate_tables(|t| {
        t.projects.clear();
    });
    assert_eq!(act(&app, start_intent("发不出去")).await.error, None);
    until(&app, &mut web, "报出原因、不再转圈", |s| {
        s.home
            .as_ref()
            .is_some_and(|h| !h.starting && h.start_error.is_some())
    })
    .await;
    assert!(
        web.state
            .home
            .as_ref()
            .unwrap()
            .start_error
            .as_deref()
            .unwrap()
            .starts_with("没能新建对话")
    );
    assert!(
        web.state
            .navigate
            .as_ref()
            .is_none_or(|n| n.started_project_id.is_none()),
        "没有收尾跳转：网页不清草稿"
    );
    assert_eq!(messages_by_conversation(&d), vec![]);
    // 一次只能有一个在发：失败之后可以再发
    assert_eq!(act(&app, start_intent("再试")).await.error, None);
}

async fn choose_home(app: &App, mode: pb::ApprovalMode) {
    let ack = act(
        app,
        I::SetHomeModel(pb::HomeModelIntent {
            model_id: "fake-small".into(),
            effort: None,
        }),
    )
    .await;
    assert_eq!(ack.error, None);
    let ack = act(
        app,
        I::SetHomeApprovalMode(pb::HomeApprovalModeIntent { mode: mode as i32 }),
    )
    .await;
    assert_eq!(ack.error, None);
}

/// 审查 2a 2/F-7：设模型、设档位任一步被拒，就停下报原因，首条消息不发出。
#[tokio::test(flavor = "multi_thread")]
async fn home_start_stops_when_a_step_is_rejected() {
    let (d, app, mut web, _dir) = start("empty").await;
    home(&app, "").await;
    live(&app, &mut web).await;
    choose_home(&app, pb::ApprovalMode::Auto).await;
    d.fail_commands(Some(Box::new(|c: &pb::Command| {
        matches!(c.kind, Some(pb::command::Kind::SetApprovalMode(_)))
    })));
    assert_eq!(act(&app, start_intent("不该发出去")).await.error, None);
    until(&app, &mut web, "报出原因、不再转圈", |s| {
        s.home
            .as_ref()
            .is_some_and(|h| !h.starting && h.start_error.is_some())
    })
    .await;
    assert!(
        web.state
            .home
            .as_ref()
            .and_then(|h| h.start_error.as_deref())
            .is_some_and(|e| e.contains("消息没发出去")),
        "原因：{:?}",
        web.state.home.as_ref().map(|h| &h.start_error)
    );
    assert_eq!(messages_by_conversation(&d), vec![], "首条消息没有发出");
    let mut models = 0;
    d.mutate_tables(|t| {
        models = t
            .conversations
            .values()
            .filter(|r| r.conversation.model_override.is_some())
            .count();
    });
    assert_eq!(models, 1, "对照：被拒之前的设模型发出去了");
}

/// 审查 2a 2/F-7：设模型、设档位在首条消息之前生效（首个回合就用它们）：后台日志里两者的 seq 都更小。
#[tokio::test(flavor = "multi_thread")]
async fn home_start_sets_choices_before_the_first_message() {
    use pb::domain_event::Event as E;
    let (d, app, mut web, _dir) = start("empty").await;
    home(&app, "").await;
    live(&app, &mut web).await;
    choose_home(&app, pb::ApprovalMode::Auto).await;
    assert_eq!(act(&app, start_intent("顺序")).await.error, None);
    until(&app, &mut web, "发出后跳过去", |s| {
        s.navigate
            .as_ref()
            .is_some_and(|n| n.started_project_id.is_some())
    })
    .await;
    let seq_of = |want: fn(&E) -> bool| {
        d.persisted_log()
            .into_iter()
            .find(|p| {
                p.event
                    .as_ref()
                    .and_then(|e| e.event.as_ref())
                    .is_some_and(want)
            })
            .map(|p| p.seq)
            .expect("日志里有这一条")
    };
    let model = seq_of(|e| matches!(e, E::ConversationModelChanged(_)));
    let mode = seq_of(|e| matches!(e, E::ApprovalModeChanged(_)));
    let message = seq_of(|e| matches!(e, E::MessageAppended(_)));
    assert!(
        model < message && mode < message,
        "设模型 {model}、设档位 {mode} 都在首条消息 {message} 之前"
    );
}
