//! 设置 › 审批 的记住的规则（步 7d）：连假后台走真协议。规则从后台查来、按「永久 / 本项目 / 本对话」分组；
//! 撤销一条，受理后重查（方案 2.7），那一条从视图里消失。
//! rule 流（记住、撤销、连带删除）的事件来了：标过期、重查 ListRules（方案 2.2「失效后重查，不推演」）；
//! 重连走了快照也重查。假后台先把 rule 流放进设置范围（真后台在 T10a 步 7）。

#![cfg(unix)]

mod support;

use std::time::Duration;

use iwork_client_core::{App, DirectDialer, Options};
use iwork_fake_daemon::fixtures as f;
use iwork_fake_daemon::scenario::Step;
use iwork_fake_daemon::{FakeDaemon, self_trusting_identity};
use iwork_proto::pb;
use pb::ui_intent::Kind as I;
use support::web::{Web, WebState};
use tokio::time::timeout;

const WAIT: Duration = Duration::from_secs(20);

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

fn groups(s: &WebState) -> Vec<(String, Vec<String>)> {
    s.settings
        .as_ref()
        .map(|v| {
            v.rule_groups
                .iter()
                .map(|g| {
                    (
                        g.title.clone(),
                        g.rules.iter().map(|r| r.rule_id.clone()).collect(),
                    )
                })
                .collect()
        })
        .unwrap_or_default()
}

#[tokio::test(flavor = "multi_thread")]
async fn rules_are_grouped_and_revoke_removes_one() {
    let dir = tempfile::Builder::new()
        .prefix("t09-7d")
        .tempdir_in("/tmp")
        .unwrap();
    let daemon = FakeDaemon::start(dir.path().join("d.sock")).await.unwrap();
    daemon.run_scenario("settings-rules").await.unwrap();
    let dialer = DirectDialer::new(daemon.endpoint(), self_trusting_identity().unwrap(), "test");
    let (app, _effects) = App::start(dialer, Options::default());
    let mut web = Web::new(app.attach().await.unwrap());
    let ack = app
        .intent(pb::UiIntent {
            kind: Some(I::SetVisible(pb::SetVisible {
                sidebar: true,
                settings: true,
                ..Default::default()
            })),
        })
        .await;
    assert_eq!(ack.error, None);
    until(&app, &mut web, "三组规则", |s| groups(s).len() == 3).await;
    assert_eq!(
        groups(&web.state),
        vec![
            ("永久".to_owned(), vec!["rule-forever".to_owned()]),
            ("本项目".to_owned(), vec!["rule-project".to_owned()]),
            ("本对话".to_owned(), vec!["rule-conv".to_owned()]),
        ]
    );
    let ack = app
        .intent(pb::UiIntent {
            kind: Some(I::RevokeRule(pb::RuleIntent {
                rule_id: "rule-project".into(),
            })),
        })
        .await;
    assert_eq!(ack.error, None);
    until(&app, &mut web, "撤销后那一条没了", |s| {
        let g = groups(s);
        g.len() == 2
            && !g
                .iter()
                .any(|(_, ids)| ids.iter().any(|i| i == "rule-project"))
    })
    .await;
}

async fn connect_settings(scenario: &str) -> (FakeDaemon, App, Web, tempfile::TempDir) {
    let dir = tempfile::Builder::new()
        .prefix("t09-rules")
        .tempdir_in("/tmp")
        .unwrap();
    let daemon = FakeDaemon::start(dir.path().join("d.sock")).await.unwrap();
    daemon.run_scenario(scenario).await.unwrap();
    let dialer = DirectDialer::new(daemon.endpoint(), self_trusting_identity().unwrap(), "test");
    let (app, _effects) = App::start(dialer, Options::default());
    let web = Web::new(app.attach().await.unwrap());
    let ack = app
        .intent(pb::UiIntent {
            kind: Some(I::SetVisible(pb::SetVisible {
                sidebar: true,
                settings: true,
                ..Default::default()
            })),
        })
        .await;
    assert_eq!(ack.error, None);
    (daemon, app, web, dir)
}

fn ids(s: &WebState) -> Vec<String> {
    groups(s).into_iter().flat_map(|(_, ids)| ids).collect()
}

/// rule 流的三种事件：记住一条 → 出现；撤销 → 消失；连带删除 → 那几条没了。都是重查来的，不是按事件改列表。
#[tokio::test(flavor = "multi_thread")]
async fn rule_events_requery_the_list() {
    let (d, app, mut web, _dir) = connect_settings("empty").await;
    until(&app, &mut web, "规则列表查过了（空）", |s| {
        s.settings.as_ref().is_some_and(|v| !v.rules_loading)
    })
    .await;
    assert!(ids(&web.state).is_empty(), "对照：一开始没有规则");
    for (id, scope) in [
        ("r-a", pb::scope::Kind::Global(pb::GlobalScope {})),
        ("r-b", pb::scope::Kind::Global(pb::GlobalScope {})),
    ] {
        d.append(f::remembered_rule(id, "执行 `ls` 不再问", scope));
    }
    until(&app, &mut web, "记住的两条出现", |s| {
        ids(s) == ["r-a", "r-b"]
    })
    .await;
    d.append(f::ev(pb::domain_event::Event::RuleRevoked(
        pb::RuleRevoked {
            rule_id: "r-a".into(),
        },
    )));
    until(&app, &mut web, "撤销的那条消失", |s| {
        ids(s) == ["r-b"]
    })
    .await;
    d.append(f::ev(pb::domain_event::Event::RulesDeleted(
        pb::RulesDeleted {
            rule_ids: vec!["r-b".into()],
            ..Default::default()
        },
    )));
    until(&app, &mut web, "连带删除后空了", |s| {
        ids(s).is_empty()
            && s.settings
                .as_ref()
                .is_some_and(|v| !v.rules_empty_text.is_empty())
    })
    .await;
}

/// 断线期间记住了一条、重连走了快照（事件不再逐条重放）：快照之后照样重查，那条出现。
#[tokio::test(flavor = "multi_thread")]
async fn snapshot_after_reconnect_requeries_rules() {
    let (d, app, mut web, _dir) = connect_settings("empty").await;
    until(&app, &mut web, "规则列表查过了（空）", |s| {
        s.settings.as_ref().is_some_and(|v| !v.rules_loading)
    })
    .await;
    d.run_steps(vec![Step::StopListening]).await;
    d.append(f::remembered_rule(
        "r-offline",
        "执行 `make` 不再问",
        pb::scope::Kind::Global(pb::GlobalScope {}),
    ));
    d.run_steps(vec![Step::DisconnectForSnapshot]).await;
    until(&app, &mut web, "重连后那条出现", |s| {
        ids(s) == ["r-offline"]
    })
    .await;
}
