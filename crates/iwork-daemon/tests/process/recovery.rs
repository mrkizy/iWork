//! 崩溃恢复（卡验收 3 的 T04 部分；S4、S8；方案 2.5、2.6）。

use std::collections::{BTreeMap, BTreeSet};
use std::time::Duration;

use iwork_domain::event::DomainEvent;
use iwork_domain::message::{MessageBody, MessageStatus};
use iwork_domain::turn::{InterruptReason, TurnState};
use iwork_ipc::client::IncomingItem;
use iwork_ipc::rendezvous::{ExitCode, Phase, Refusal};
use iwork_ports::read_model::{MessagePage, PendingItem, ReadModelPort};
use iwork_proto::pb;

use crate::fixtures::*;
use crate::seed::{self, *};
use crate::support::{Sandbox, code, signal};

const START: Duration = Duration::from_secs(20);

/// 起到就绪、用 SIGTERM 停掉。
fn run_once(sb: &Sandbox) {
    let mut d = sb.spawn(&[], &[]);
    sb.wait_ready(d.pid(), START);
    signal(d.pid(), libc::SIGTERM);
    assert_eq!(code(d.wait_exit(START)), Some(0), "{}", d.stderr());
}

fn kinds(events: &[iwork_ports::store::StoredEvent]) -> BTreeMap<&'static str, usize> {
    let mut m = BTreeMap::new();
    for e in events {
        *m.entry(e.event.kind()).or_default() += 1;
    }
    m
}

/// 恢复提交的那一组事件：每个持有进程的回合一条状态变化，取消中的另加 `TurnFinished`，流式消息一条结束，
/// 三张卡片各一条，末尾一条 `RecoveryCompleted`。没有审批事件、没有卡片的启动 / 执行 / 结束一类事件。
fn expected_recovery_kinds() -> BTreeMap<&'static str, usize> {
    BTreeMap::from([
        ("turn_state_changed", 4),
        ("turn_finished", 1),
        ("message_completed", 1),
        ("user_action_state_changed", 3),
        ("recovery_completed", 1),
    ])
}

fn assert_recovered(sb: &Sandbox, seeded_head: u64) {
    let new = events_after(sb, seeded_head);
    assert_eq!(kinds(&new), expected_recovery_kinds(), "{new:#?}");
    let completed = new
        .iter()
        .find_map(|e| match &e.event {
            DomainEvent::RecoveryCompleted(r) => Some(r.interrupted_turn_ids.clone()),
            _ => None,
        })
        .unwrap();
    let completed: BTreeSet<_> = completed.into_iter().collect();
    assert_eq!(
        completed,
        BTreeSet::from([
            tid(RUNNING_WITH_SESSION),
            tid(RUNNING_NO_SESSION),
            tid(WAITING_APPROVAL)
        ])
    );
    assert!(
        matches!(new.last().unwrap().event, DomainEvent::RecoveryCompleted(_)),
        "RecoveryCompleted 在最后"
    );
    for e in &new {
        assert_eq!(e.actor.0, "system:recovery");
    }

    for n in [RUNNING_WITH_SESSION, RUNNING_NO_SESSION, WAITING_APPROVAL] {
        assert_eq!(turn_state(sb, n), "interrupted_crash", "回合 {n}");
    }
    assert_eq!(turn_state(sb, CANCELLING), "cancelled");
    for n in [QUEUED_FRESH, QUEUED_OPENED_BEFORE_COMMIT] {
        assert_eq!(turn_state(sb, n), "queued", "回合 {n}：P1，恢复不碰");
    }

    let store = seed::open(sb);
    let read = read_model(&store);
    let view = block(read.conversation(
        cid(10),
        MessagePage {
            before: None,
            limit: 50,
        },
    ))
    .unwrap();
    let msg = view
        .messages
        .iter()
        .find(|m| m.id == mid(STREAMING_MESSAGE))
        .unwrap();
    assert_eq!(msg.status, MessageStatus::Interrupted);
    assert_eq!(
        msg.body,
        MessageBody::Inline(CHECKPOINT.into()),
        "正文是检查点内容"
    );
    let interrupted_turn = view
        .turns
        .iter()
        .find(|t| t.id == tid(RUNNING_WITH_SESSION));
    if let Some(t) = interrupted_turn {
        assert_eq!(
            t.state,
            TurnState::Interrupted {
                reason: InterruptReason::Crash
            }
        );
        assert_eq!(t.runtime_session_id.as_deref(), Some("rt-session-1"));
    }

    let pending = block(read.pending_items()).unwrap();
    let resumes: BTreeSet<_> = pending
        .iter()
        .filter_map(|p| match p {
            PendingItem::Resume(r) => Some(r.turn_id),
            _ => None,
        })
        .collect();
    assert_eq!(resumes, completed, "「继续」卡片");
    let approval = pending
        .iter()
        .find_map(|p| match p {
            PendingItem::Approval(a) => Some(a.clone()),
            _ => None,
        })
        .expect("审批仍在待处理里");
    assert_eq!(approval.id, apid(PENDING_APPROVAL));
    assert_eq!(
        approval.state,
        iwork_domain::approval::ApprovalState::Pending
    );
    let unsettled = block(read.unsettled()).unwrap();
    assert!(
        unsettled.turns.is_empty() && unsettled.streaming.is_empty() && unsettled.cards.is_empty(),
        "恢复之后没有没收尾的东西：{unsettled:?}"
    );
    store.close().unwrap();

    for n in CARDS {
        assert_eq!(
            sql_text(
                &sb.db(),
                "SELECT state_kind FROM user_actions WHERE id = ?1",
                uaid(n).to_string()
            ),
            "unknown",
            "卡片 {n}"
        );
    }
    assert_eq!(
        sql_text(
            &sb.db(),
            "SELECT state FROM conversations WHERE id = ?1",
            cid(DELETING_CONVERSATION).to_string()
        ),
        "deleting",
        "删除中的对话恢复不碰"
    );
}

/// 卡验收 3 的 T04 部分：预置崩溃状态 → 起到就绪 → `kill -9` → 再起 → 停 → 直接读库断言。
#[test]
fn kill9_recovers_seeded_state() {
    let sb = Sandbox::new();
    let seeded = seed_crash_state(&sb);
    let outbox_before = outbox_states(&sb);
    assert_eq!(outbox_before, ["dispatched", "queued", "queued", "queued"]);

    let mut a = sb.spawn(&[], &[]);
    sb.wait_ready(a.pid(), START);
    a.kill9();
    run_once(&sb);

    assert_recovered(&sb, seeded);
    assert_eq!(
        outbox_states(&sb),
        outbox_before,
        "恢复不碰 outbox（投递任务对账归它自己）"
    );
}

/// S4：恢复提交前被杀 → 什么都没变；提交后被杀 → 恰好一次；再起不重复。
#[test]
fn kill_during_recovery_is_idempotent() {
    let sb = Sandbox::new();
    let seeded = seed_crash_state(&sb);

    let mut before = sb.spawn(&[], &[("IWORK_DEV_PAUSE_AT", "recovery-before-commit")]);
    before.wait_stderr("IWORK-PAUSED recovery-before-commit", START);
    before.kill9();
    assert!(events_after(&sb, seeded).is_empty(), "提交之前被杀：库没变");

    let mut after = sb.spawn(&[], &[("IWORK_DEV_PAUSE_AT", "recovery-after-commit")]);
    after.wait_stderr("IWORK-PAUSED recovery-after-commit", START);
    after.kill9();
    assert_eq!(kinds(&events_after(&sb, seeded)), expected_recovery_kinds());

    run_once(&sb);
    run_once(&sb);
    assert_recovered(&sb, seeded);
}

/// 连上、以 `from` 为游标订阅一个范围，收集收到的持久事件 seq，直到见到 head 后再安静 500ms。
async fn collect(sb: &Sandbox, scope: pb::SubscriptionScope, from: u64) -> (u64, Vec<u64>) {
    let (client, mut incoming) = sb.connect().await.expect("握手");
    let head = client.welcome().head_seq;
    client.subscribe(1, scope, from).await.unwrap();
    let mut seqs = Vec::new();
    let deadline = tokio::time::Instant::now() + Duration::from_secs(3);
    let mut quiet_until = None;
    loop {
        match tokio::time::timeout_at(quiet_until.unwrap_or(deadline), incoming.next()).await {
            Ok(Some(IncomingItem::Event(e))) => {
                if let Some(pb::event::Body::Persisted(p)) = e.body {
                    seqs.push(p.seq);
                }
            }
            Ok(Some(IncomingItem::Subscribed { .. })) => {}
            Ok(other) => panic!("{other:?}"),
            Err(_) => break,
        }
        if quiet_until.is_none() && seqs.last().is_some_and(|s| *s >= head) {
            quiet_until = Some(tokio::time::Instant::now() + Duration::from_millis(500));
        }
    }
    (head, seqs)
}

/// 第 9 步的参与方（这里是开发构建的假参与方）在观察者装上之后、`bind` 之前提交的事件，与第 8 步的恢复事件：
/// 连上的客户端从游标续传时，落在范围里的每条恰好收到一次（期望值按同一张路由表从库里算）。
#[test]
fn recovery_events_are_in_baseline_not_republished() {
    let sb = Sandbox::new();
    let seeded = seed_crash_state(&sb);
    let d = sb.spawn(&[], &[("IWORK_DEV_STARTUP_COMMIT", "1")]);
    sb.wait_ready(d.pid(), START);
    let conv = pb::SubscriptionScope {
        kind: Some(pb::subscription_scope::Kind::Conversation(
            pb::ConversationScope {
                conversation_id: cid(10).to_string(),
            },
        )),
    };
    let settings = pb::SubscriptionScope {
        kind: Some(pb::subscription_scope::Kind::Settings(pb::SettingsScope {})),
    };
    let new = events_after(&sb, seeded);
    // 恢复 10 条 + 假参与方 1 条
    assert_eq!(new.len(), 11);
    assert!(matches!(
        new.last().unwrap().event,
        DomainEvent::GlobalSettingsChanged(_)
    ));
    let rt = crate::support::rt();
    let mut covered = 0;
    for scope in [conv, settings] {
        let keys = iwork_app::subscription::scope_streams(&scope);
        let expected: BTreeSet<_> = new
            .iter()
            .filter(|e| {
                let routes = iwork_app::subscription::routes_for(&e.event);
                keys.iter().any(|k| routes.iter().any(|r| k.covers(r)))
            })
            .map(|e| e.seq.0)
            .collect();
        assert!(!expected.is_empty(), "对照：范围里确实有新事件");
        covered += expected.len();
        let (head, seqs) = rt.block_on(collect(&sb, scope, seeded));
        assert_eq!(head, seeded + 11);
        let unique: BTreeSet<_> = seqs.iter().copied().collect();
        assert_eq!(unique.len(), seqs.len(), "没有重复：{seqs:?}");
        assert_eq!(unique, expected, "每条恰好收到一次");
    }
    assert!(covered >= 2);
}

/// S8：库里有本版本不认识的事件种类 → 12、`Refused(StoreOpenFailed)` 点名种类、库的内容逐表不变、
/// 不做备份、托管时退出状态 0；删掉那条再起 → 正常就绪、head 与之前一致。
#[test]
fn unknown_event_kind_refuses_and_keeps_data() {
    let sb = Sandbox::new();
    seed_crash_state(&sb);
    run_once(&sb);
    let head_before = events_after(&sb, 0).last().unwrap().seq.0;
    append_future_event(&sb);
    let dump = dump_all(&sb);
    let backups = std::fs::read_dir(sb.data_root().join("backups"))
        .unwrap()
        .count();

    let (status, err) = sb.run_to_exit(&[], &[]);
    assert_eq!(code(status), Some(12), "{err}");
    match sb.status().unwrap().phase {
        Phase::Refused(Refusal::StoreOpenFailed { message }) => {
            assert!(message.contains("zz_future_kind"), "{message}");
        }
        other => panic!("{other:?}"),
    }
    let (status, err) = sb.run_to_exit(&["--service"], &[]);
    assert_eq!(code(status), Some(0), "{err}");
    assert_eq!(dump_all(&sb), dump, "库的内容逐表不变");
    assert_eq!(
        std::fs::read_dir(sb.data_root().join("backups"))
            .unwrap()
            .count(),
        backups,
        "不做备份"
    );

    remove_future_event(&sb);
    run_once(&sb);
    assert_eq!(events_after(&sb, 0).last().unwrap().seq.0, head_before);
}

/// 5/F-1：库里预置了第 6、8 步与投递本来会去改的东西，再追加一条不认识的事件 → 12，一样都没被写。
/// 对照：去掉那条未知事件再起 → 这些东西都被恢复改掉了。
#[test]
fn unknown_event_kind_with_unsettled_objects_writes_nothing() {
    let sb = Sandbox::new();
    let seeded = seed_crash_state(&sb);
    let leftover = sb.data_root().join("artifacts/tmp/half-written");
    std::fs::write(&leftover, b"half").unwrap();
    append_future_event(&sb);
    let dump = dump_all(&sb);

    let (status, err) = sb.run_to_exit(&[], &[]);
    assert_eq!(code(status), Some(12), "{err}");
    assert_eq!(dump_all(&sb), dump, "事件表与各投影表逐行相同");
    assert_eq!(outbox_states(&sb)[0], "dispatched");
    assert!(leftover.exists(), "制品残留没被清理");
    assert_eq!(turn_state(&sb, RUNNING_WITH_SESSION), "running");

    remove_future_event(&sb);
    run_once(&sb);
    assert!(!leftover.exists(), "对照：残留被清理了");
    assert_eq!(turn_state(&sb, RUNNING_WITH_SESSION), "interrupted_crash");
    assert_eq!(kinds(&events_after(&sb, seeded)), expected_recovery_kinds());
    let _ = ExitCode::StoreRefused;
}
