//! 设置 › 审批 的记住的规则（步 7d）：连假后台走真协议。规则从后台查来、按「永久 / 本项目 / 本对话」分组；
//! 撤销一条，受理后重查（rule 流不在订阅范围里，方案 2.7），那一条从视图里消失。

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
