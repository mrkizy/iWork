//! 每个假后台场景：场景与客户端同时跑，网页逐帧折叠的结果 == 重新 attach 后那一帧全量（Rust 侧的录像对拍）。
//!
//! 守的是「行的视图模型读了行外的状态（回合状态、连接、摘要表），那份状态变了行就得重发」：
//! 漏标一处脏，折叠结果就停在旧值上，和全量对不上。网页的同名对拍（client/src/state/fixtures.test.ts）
//! 吃的是同一批场景的录像；这里不依赖 Node，cargo test 就能挡住。

// 假后台只在 Unix 上监听（iwork-fake-daemon）；M1 暂不支持 Windows。
#![cfg(unix)]

mod support;

use std::time::Duration;

use iwork_client_core::{App, DirectDialer, Options};
use iwork_fake_daemon::{FakeDaemon, scenario, self_trusting_identity};
use iwork_proto::pb;
use support::web::{Web, WebState};
use tokio::time::timeout;

/// 场景跑完后，这么久没有新块就算取空了。
const QUIET: Duration = Duration::from_millis(300);
const LIMIT: Duration = Duration::from_secs(120);

fn visible(conversation: Option<&str>) -> pb::UiIntent {
    pb::UiIntent {
        kind: Some(pb::ui_intent::Kind::SetVisible(pb::SetVisible {
            sidebar: true,
            pending: true,
            settings: true,
            conversation_id: conversation.map(Into::into),
            ..Default::default()
        })),
    }
}

fn exists(daemon: &FakeDaemon, conversation: &str) -> bool {
    daemon
        .try_query(&pb::Query {
            kind: Some(pb::query::Kind::GetConversation(pb::GetConversation {
                conversation_id: conversation.into(),
                before_message_id: None,
                limit: 1,
            })),
        })
        .is_ok()
}

/// 取一块交给网页；等不到返回 false。
async fn pull(app: &App, web: &mut Web, wait: Duration) -> bool {
    match timeout(wait, app.next_frame(web.request())).await {
        Ok(Ok(c)) => {
            web.accept(&c);
            true
        }
        Ok(Err(e)) => panic!("取帧失败：{e:?}"),
        Err(_) => false,
    }
}

/// 对拍时只比视图内容：跳转请求是一次性的事件，全量帧不重发它。
fn comparable(s: &WebState) -> WebState {
    WebState {
        navigate: None,
        ..s.clone()
    }
}

async fn fold_and_full(s: &scenario::Scenario) -> (WebState, WebState) {
    let dir = tempfile::Builder::new()
        .prefix("t09-fold")
        .tempdir_in("/tmp")
        .unwrap();
    let daemon = FakeDaemon::start(dir.path().join("d.sock")).await.unwrap();
    let dialer = DirectDialer::new(daemon.endpoint(), self_trusting_identity().unwrap(), "test");
    let (app, _effects) = App::start(dialer, Options::default());
    let mut web = Web::new(app.attach().await.unwrap());
    assert_eq!(app.intent(visible(None)).await.error, None);

    let mut focused = s.focus.is_none();
    let run = daemon.run_scenario(s.name);
    tokio::pin!(run);
    let mut done = false;
    loop {
        let got = if done {
            pull(&app, &mut web, QUIET).await
        } else {
            tokio::select! {
                r = &mut run => { r.unwrap(); done = true; continue; }
                g = pull(&app, &mut web, QUIET) => g,
            }
        };
        if !focused && s.focus.is_some_and(|c| exists(&daemon, c)) {
            assert_eq!(app.intent(visible(s.focus)).await.error, None);
            focused = true;
        }
        if !got && done && focused {
            break;
        }
    }

    let mut full = Web::new(app.attach().await.unwrap());
    while full.frames == 0 {
        assert!(
            pull(&app, &mut full, Duration::from_secs(5)).await,
            "重新 attach 后等不到全量帧"
        );
    }
    (comparable(&web.state), comparable(&full.state))
}

#[tokio::test]
async fn every_scenario_folds_to_full() {
    let mut mismatched = Vec::new();
    for s in scenario::REGISTRY {
        let (folded, full) = timeout(LIMIT, fold_and_full(s))
            .await
            .unwrap_or_else(|_| panic!("场景 {} 超时", s.name));
        // 对照：真比了东西
        assert!(full.sidebar.is_some(), "{}：全量帧带侧栏", s.name);
        if s.focus.is_some() {
            assert!(!full.rows.is_empty(), "{}：全量帧带对话行", s.name);
        }
        if folded != full {
            let diff: Vec<_> = full
                .rows
                .iter()
                .filter(|(k, v)| folded.rows.get(*k) != Some(*v))
                .map(|(k, _)| k.clone())
                .collect();
            mismatched.push(format!("{}（对不上的行：{diff:?}）", s.name));
        }
    }
    assert!(mismatched.is_empty(), "逐帧折叠与全量不等：{mismatched:#?}");
}
