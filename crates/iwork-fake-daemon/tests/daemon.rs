//! 假后台经真 ipc 连得上、场景都能跑完、断开与「换身份」的行为、命令幂等。客户端用真 `iwork_ipc::client::Client`。
#![cfg(unix)]

use std::time::Duration;

use iwork_fake_daemon::{FakeDaemon, scenario, self_trusting_identity};
use iwork_ipc::LocalConnector;
use iwork_ipc::client::{
    Client, ClientConfig, ClientError, ConnectError, Incoming, IncomingItem, WaitPrompt,
};
use iwork_proto::pb;

async fn daemon() -> (tempfile::TempDir, FakeDaemon) {
    let dir = tempfile::tempdir().unwrap();
    let d = FakeDaemon::start(dir.path().join("d.sock")).await.unwrap();
    (dir, d)
}

async fn connect(d: &FakeDaemon) -> Result<(Client, Incoming), ConnectError> {
    // 发布同款的 RequireSigner：客户端与假后台在同一个测试进程里，自信任身份互认（不需要 DevTrust）。
    let cfg = ClientConfig::new(pb::ClientKind::DevTool, "fake-daemon-test");
    let own = self_trusting_identity().unwrap();
    Client::connect(&LocalConnector, &d.endpoint(), &own, cfg).await
}

async fn next(inc: &mut Incoming) -> IncomingItem {
    tokio::time::timeout(Duration::from_secs(10), inc.next())
        .await
        .expect("10 秒内应有下一项")
        .expect("Incoming 不该结束")
}

fn sidebar() -> pb::SubscriptionScope {
    pb::SubscriptionScope {
        kind: Some(pb::subscription_scope::Kind::Sidebar(pb::SidebarScope {})),
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn every_scenario_runs_to_the_end() {
    for name in scenario::names() {
        let (_dir, d) = daemon().await;
        d.run_scenario(name).await.unwrap_or_else(|e| panic!("{e}"));
        assert!(d.head() > 0, "{name} 没追加任何事件");
        d.shutdown().await;
    }
    let (_dir, d) = daemon().await;
    assert!(d.run_scenario("没有这个").await.is_err());
}

#[tokio::test(flavor = "multi_thread")]
async fn client_gets_snapshot_then_live_events_with_subscription_ids() {
    let (_dir, d) = daemon().await;
    d.run_scenario("conversation-basic").await.unwrap();
    let (client, mut inc) = connect(&d).await.expect("DevTrust 下连得上");
    client.subscribe_nowait(7, sidebar(), 0).unwrap();
    let IncomingItem::Subscribed { subscribed, .. } = next(&mut inc).await else {
        panic!("先来快照")
    };
    let snap = subscribed.snapshot.expect("from_seq = 0 给快照");
    assert_eq!(snap.results.len(), 3, "侧栏快照 = 项目、对话、助手三条查询");
    assert_eq!(subscribed.snapshot_seq, d.head());
    let seq = d.append(iwork_fake_daemon::fixtures::conversation_read("c-basic", 3));
    let IncomingItem::Event(e) = next(&mut inc).await else {
        panic!("再来事件")
    };
    assert_eq!(e.subscription_ids, [7]);
    match e.body {
        Some(pb::event::Body::Persisted(p)) => assert_eq!(p.seq, seq),
        other => panic!("{other:?}"),
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn turn_events_reach_conversation_scope_as_after_g5() {
    let (_dir, d) = daemon().await;
    d.run_scenario("conversation-basic").await.unwrap();
    let (client, mut inc) = connect(&d).await.unwrap();
    let scope = pb::SubscriptionScope {
        kind: Some(pb::subscription_scope::Kind::Conversation(
            pb::ConversationScope {
                conversation_id: "c-basic".into(),
            },
        )),
    };
    client.subscribe_nowait(1, scope, d.head()).unwrap();
    assert!(matches!(
        next(&mut inc).await,
        IncomingItem::Subscribed { .. }
    ));
    let seq = d.append(iwork_fake_daemon::fixtures::turn_state(
        "t-basic",
        pb::TurnState::Completed,
        pb::TurnState::Completed,
        None,
    ));
    let IncomingItem::Event(e) = next(&mut inc).await else {
        panic!()
    };
    let Some(pb::event::Body::Persisted(p)) = e.body else {
        panic!()
    };
    assert_eq!(
        p.seq, seq,
        "回合事件也落在 conversation:<id> 上（G5 之后的路由）"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn disconnect_all_closes_and_keeps_the_log() {
    let (_dir, d) = daemon().await;
    d.run_scenario("empty").await.unwrap();
    let head = d.head();
    let (_client, mut inc) = connect(&d).await.unwrap();
    d.disconnect_all().await;
    loop {
        if let IncomingItem::Closed(_) = next(&mut inc).await {
            break;
        }
    }
    let (_c2, _i2) = connect(&d).await.expect("重启后照样连得上");
    assert_eq!(d.head(), head, "日志保留");
}

#[tokio::test(flavor = "multi_thread")]
async fn rejecting_identity_closes_before_welcome_then_recovers() {
    let (_dir, d) = daemon().await;
    d.set_rejecting(true).await;
    match connect(&d).await {
        // 服务端拒绝后关连接；客户端在写 Hello 或读 Welcome 时发现，两者在 ipc 契约里是同一个状态（等待提示）。
        Err(e) => assert_eq!(
            e.wait_prompt(),
            Some(WaitPrompt::VersionMismatch),
            "真身份校验拒绝：握手前关连接，得到 {e:?}"
        ),
        Ok(_) => panic!("换了身份的后台不该接受客户端"),
    }
    d.set_rejecting(false).await;
    connect(&d).await.expect("换回来后连得上");
}

#[tokio::test(flavor = "multi_thread")]
async fn commands_are_idempotent_by_command_id() {
    let (_dir, d) = daemon().await;
    d.run_scenario("approval-states").await.unwrap();
    let (client, _inc) = connect(&d).await.unwrap();
    let cmd = |id: &str| pb::Command {
        command_id: id.into(),
        kind: Some(pb::command::Kind::DecideApproval(pb::DecideApproval {
            approval_id: "ap-pending".into(),
            decision: pb::ApprovalDecision::Allow.into(),
            remember: 0,
        })),
    };
    let send = |c: pb::Command| client.request(pb::request::Body::Command(c));
    let first = send(cmd("k1")).await.unwrap();
    let Some(pb::response::Body::Accepted(a1)) = first.body else {
        panic!("{first:?}")
    };
    assert!(!a1.replayed);
    let again = send(cmd("k1")).await.unwrap();
    let Some(pb::response::Body::Accepted(a2)) = again.body else {
        panic!("{again:?}")
    };
    assert!(a2.replayed);
    assert_eq!((a2.first_seq, a2.last_seq), (a1.first_seq, a1.last_seq));
    // 换一个 command_id 再批一次：已经不是待定，回 APPROVAL_NOT_PENDING（S8）。
    let Err(ClientError::Server(e)) = send(cmd("k2")).await else {
        panic!("应被拒绝")
    };
    assert_eq!(e.code, i32::from(pb::ErrorCode::ApprovalNotPending));
    // 没实现的命令点名「假后台未实现」。
    let rename = send(pb::Command {
        command_id: "k3".into(),
        kind: Some(pb::command::Kind::RenameConversation(
            pb::RenameConversation {
                conversation_id: "c-approval".into(),
                title: "x".into(),
            },
        )),
    })
    .await;
    let Err(ClientError::Server(e)) = rename else {
        panic!("{rename:?}")
    };
    assert_eq!(
        e.details.get("fake").map(String::as_str),
        Some("unimplemented")
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn confirming_unknown_card_finishes_pending_delete() {
    let (_dir, d) = daemon().await;
    d.run_scenario("user-action-unknown-deleting")
        .await
        .unwrap();
    let (client, _inc) = connect(&d).await.unwrap();
    let r = client
        .request(pb::request::Body::Command(pb::Command {
            command_id: "k".into(),
            kind: Some(pb::command::Kind::ConfirmUserActionEnded(
                pb::ConfirmUserActionEnded {
                    card_id: "ua-unknown-del".into(),
                },
            )),
        }))
        .await
        .unwrap();
    assert!(
        matches!(r.body, Some(pb::response::Body::Accepted(_))),
        "{r:?}"
    );
    let err = d
        .try_query(&pb::Query {
            kind: Some(pb::query::Kind::GetConversation(pb::GetConversation {
                conversation_id: "c-ua-deleting".into(),
                before_message_id: None,
                after_message_id: None,
                around_seq: None,
                limit: 0,
            })),
        })
        .unwrap_err();
    assert_eq!(err.code, i32::from(pb::ErrorCode::NotFound), "删除已做完");
}

#[tokio::test(flavor = "multi_thread")]
async fn rename_scenario_changes_title_and_hits_sidebar() {
    let (_dir, d) = daemon().await;
    d.run_scenario("empty").await.unwrap();
    let (client, mut inc) = connect(&d).await.unwrap();
    client.subscribe_nowait(3, sidebar(), d.head()).unwrap();
    assert!(matches!(
        next(&mut inc).await,
        IncomingItem::Subscribed { .. }
    ));
    // 只跑 base 之后的部分。
    let base = scenario::base().len();
    for step in scenario::get("rename-via-summary-refresh-until-t10")
        .unwrap()
        .steps()
        .into_iter()
        .skip(base)
    {
        match step {
            scenario::Step::Append(e) => {
                d.append(*e);
            }
            scenario::Step::Mutate(f) => d.mutate_tables(f),
            _ => unreachable!(),
        }
    }
    let mut last = None;
    while let Ok(Some(item)) = tokio::time::timeout(Duration::from_millis(500), inc.next()).await {
        if let IncomingItem::Event(e) = item {
            assert_eq!(e.subscription_ids, [3]);
            last = Some(e);
        }
    }
    let last = last.expect("最后一条事件落在侧栏订阅上");
    assert!(matches!(
        last.body,
        Some(pb::event::Body::Persisted(pb::PersistedEvent {
            event: Some(pb::DomainEvent {
                event: Some(pb::domain_event::Event::ConversationRead(_))
            }),
            ..
        }))
    ));
    let list = d.query(&pb::Query {
        kind: Some(pb::query::Kind::ListConversations(
            pb::ListConversations::default(),
        )),
    });
    let Some(pb::query_result::Kind::ListConversations(l)) = list.kind else {
        panic!()
    };
    let c = l.conversations.iter().find(|c| c.id == "c-rename").unwrap();
    assert_eq!(c.title, scenario::RENAMED_TITLE);
}
