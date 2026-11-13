//! 封口（T10a 方案 E3、E4）。
//!
//! E3「整趟被丢掉」：会话线任务停在准备的各个等待点上时封口 → 放开：这一趟没有再被推进（没有新的读、没有新入队的
//! 提交），`AbortReport` 3 秒内回来、清树做了。这几格验的是有偏 `select` 丢掉整趟，不验存储闸（5/F-2）。
//! E4「闸在入队点上」：经引擎的提交压在追平、令牌上，屏障的空提交、清孤儿、权限请求、单独的补记：封口之后都入不了队。

mod support;

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use iwork_domain::event::{DomainEvent, SecurityListEntryAdded};
use iwork_domain::rule::{RuleLayer, RuleMatcher, RuleScope};
use iwork_domain::turn::TurnState;
use iwork_ipc::server::RequestHandler;
use iwork_ports::store::StoragePort;
use support::fixtures::*;
use support::hooked::Stall;
use support::turns::*;

/// 从现在起真的入了队的提交数（`on_enqueued` 只在闸放行、调了真存储的那一下叫）。
fn count_enqueues(h: &H) -> Arc<AtomicUsize> {
    let n = Arc::new(AtomicUsize::new(0));
    let c = n.clone();
    h.gate.on_enqueued(move |_| {
        c.fetch_add(1, Ordering::SeqCst);
    });
    n
}

/// 两轮之间加一条清单条目（敏感目录），下一个回合换线（D8）。
async fn change_list(h: &H, n: u128) {
    let mut entry = rule(n, RuleLayer::SensitiveDirs, RuleScope::Global);
    entry.matcher = RuleMatcher::PathUnder(format!("/secret-{n}").into());
    h.raw
        .commit(cmd(
            1000 + n,
            vec![SecurityListEntryAdded { rule: entry }.into()],
        ))
        .await
        .unwrap();
}

fn rebinds_to(r: &iwork_ports::store::CommitRequest, id: &str) -> bool {
    r.events.iter().any(|e| {
        matches!(e, DomainEvent::TurnSessionBound(b) if b.runtime_session_id == id && b.replaced.is_none())
    })
}

/// 让这条会话线身上留下一笔没补上的绑定回退（T06 5.2a 的 `unconfirmed`）：换线续接 rt-0 时运行时说记录丢了 →
/// 新开 rt-1 → 首条 prompt 没收下 → 编排收尾补记 rt-0 的那一笔回 `Busy`。之后这条线上是 rt-0、库里最后一条绑定是 rt-1。
async fn line_with_pending_rebind(h: &mut H) {
    h.start_host();
    let a = h.send(1, "一").await;
    h.wait_state(a, |s| s == TurnState::Completed).await;
    change_list(h, 81).await;
    h.agent.lose_on_resume.store(true, Ordering::SeqCst);
    h.agent.fail_next_prompt.store(true, Ordering::SeqCst);
    h.gate.refuse_when(|r| rebinds_to(r, "rt-0"));
    let b = h.send(2, "二").await;
    h.wait_state(b, |s| s.is_terminal()).await;
    h.agent.lose_on_resume.store(false, Ordering::SeqCst);
    h.gate.stop_refusing();
    let opened = h.agent.opened_with.lock().unwrap().clone();
    assert_eq!(
        opened,
        vec![None, Some("rt-0".to_owned()), None],
        "布置：续接 rt-0 丢了、新开了一条"
    );
}

/// 封口，3 秒内要回来；回 `AbortReport`。
async fn seal(h: &H) -> iwork_daemon::work::AbortReport {
    use iwork_daemon::work::RunningWork;
    tokio::time::timeout(Duration::from_secs(3), h.host.abort_all())
        .await
        .expect("封口 3 秒内回来")
}

/// 放开停住的那一处，等它真的返回，再给这一趟留点时间（它被丢掉了就不会再动）。
async fn release(stall: &Stall) {
    stall.release();
    within("停住的那一次返回了", async {
        while !stall.all_returned() {
            tokio::task::yield_now().await;
        }
    })
    .await;
    tokio::time::sleep(Duration::from_millis(300)).await;
}

/// E3 的五个等待点（方案 0a.9 的 E3 行）。
#[derive(Debug, Clone, Copy)]
enum Wait {
    /// 准备的读（取回合、请求、算快照的输入）
    SnapshotRead,
    /// 换线里补记旧编号的那一笔（入队之前）
    RebindCommit,
    /// 换线里等旧进程树清空
    Retire,
    /// 取这个对话最后一条会话绑定
    LastBinding,
    /// 拍开始那份改动
    ChangeCapture,
}

/// 每个等待点停在哪（拍改动停在假版本控制上，没有夹具的停点）。
fn stall_of(h: &H, wait: Wait) -> Option<&Stall> {
    match wait {
        Wait::SnapshotRead => Some(&h.knobs.turn_with_conversation),
        Wait::RebindCommit => Some(&h.knobs.commit.stall),
        Wait::Retire => Some(&h.agent.terminate_stall),
        Wait::LastBinding => Some(&h.knobs.last_session_binding),
        Wait::ChangeCapture => None,
    }
}

/// 一格：把会话线停在 `wait` 上 → 封口 → 放开 → 这一趟没有再被推进（没有新的读、没有新入队的提交、没有开会话），
/// `AbortReport` 3 秒内回来、清树做了（有会话的都清空了）。
async fn seal_at(wait: Wait) {
    let mut h = harness(Opts {
        probe_enabled: matches!(wait, Wait::ChangeCapture),
        ..Opts::default()
    })
    .await;
    // 布置：要换线的三格先跑一个回合，让这条线上有会话
    match wait {
        Wait::RebindCommit => {
            line_with_pending_rebind(&mut h).await;
            h.knobs.commit.select(|r| rebinds_to(r, "rt-0"));
        }
        Wait::Retire => {
            h.start_host();
            let a = h.send(1, "一").await;
            h.wait_state(a, |s| s == TurnState::Completed).await;
        }
        Wait::SnapshotRead | Wait::LastBinding | Wait::ChangeCapture => h.start_host(),
    }
    let stall = stall_of(&h, wait);
    match stall {
        Some(s) => s.arm(),
        None => h.vcs.hang.store(true, Ordering::SeqCst),
    }
    if matches!(wait, Wait::RebindCommit | Wait::Retire) {
        change_list(&h, 82).await;
    }
    let _t = h.send(3, "停住").await;
    match stall {
        Some(s) => within("停住", s.wait_stuck()).await,
        None => {
            within("拍改动停住", async {
                while h.vcs.hanging.load(Ordering::SeqCst) == 0 {
                    tokio::task::yield_now().await;
                }
            })
            .await
        }
    }
    let reads = (
        h.knobs.turn_with_conversation.calls(),
        h.knobs.last_session_binding.calls(),
    );
    let opened = h.agent.opened_with.lock().unwrap().len();
    let prompts = h.agent.prompt_count();
    // 观测从封口之前装上：封口之后这一趟要是还往下走，走到哪一步都数得到
    let enqueued = count_enqueues(&h);
    let finished = h.host.counters().finished.load(Ordering::SeqCst);
    let abort = {
        let host = h.host.clone();
        tokio::spawn(async move {
            use iwork_daemon::work::RunningWork;
            tokio::time::timeout(Duration::from_secs(3), host.abort_all()).await
        })
    };
    if let (Wait::Retire, Some(s)) = (wait, stall) {
        // 第 2 次走到清树 = 会话线看到了封口、丢掉这一趟、进了封口那一支的清树；这时才放
        within("封口那一支开始清树", async {
            while s.calls() < 2 {
                tokio::task::yield_now().await;
            }
        })
        .await;
        s.release();
    }
    let report = within("封口回来", abort)
        .await
        .unwrap()
        .expect("封口 3 秒内回来");
    match (wait, stall) {
        (Wait::Retire, _) => tokio::time::sleep(Duration::from_millis(300)).await,
        (_, Some(s)) => release(s).await,
        (_, None) => {
            h.vcs.hang.store(false, Ordering::SeqCst);
            h.vcs.unhang.notify_waiters();
            tokio::time::sleep(Duration::from_millis(300)).await;
        }
    }
    assert_eq!(
        enqueued.load(Ordering::SeqCst),
        0,
        "{wait:?}：没有新入队的提交"
    );
    assert_eq!(
        (
            h.knobs.turn_with_conversation.calls(),
            h.knobs.last_session_binding.calls()
        ),
        reads,
        "{wait:?}：没有新的读"
    );
    assert_eq!(
        h.agent.opened_with.lock().unwrap().len(),
        opened,
        "{wait:?}：没有开会话"
    );
    assert_eq!(h.agent.prompt_count(), prompts, "{wait:?}：没有 prompt");
    assert_eq!(
        h.host.counters().finished.load(Ordering::SeqCst),
        finished,
        "{wait:?}：被丢掉的一趟不在锁里记结局"
    );
    assert_eq!(report.not_cleared, 0, "{wait:?}：清树做了：{report:?}");
    h.stop().await;
}

/// E3（4/F-2）：会话线依次停在准备的五个等待点上封口，这一趟整个被丢掉。
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn seal_at_each_prepare_wait() {
    for wait in [
        Wait::SnapshotRead,
        Wait::RebindCommit,
        Wait::Retire,
        Wait::LastBinding,
        Wait::ChangeCapture,
    ] {
        seal_at(wait).await;
    }
}

/// E3：删除对话的收线（X3）停在等清树上时封口 → 放开：封口 3 秒内回来、清树做了；没有新入队的提交。
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn seal_during_close_job() {
    let mut h = harness(Opts::default()).await;
    h.start_host();
    let a = h.send(1, "一").await;
    h.wait_state(a, |s| s == TurnState::Completed).await;
    h.agent.terminate_stall.arm();
    accepted(
        &h.ask(
            fresh_id(2),
            iwork_proto::pb::command::Kind::DeleteConversation(
                iwork_proto::pb::DeleteConversation {
                    conversation_id: conversation().to_string(),
                },
            ),
        )
        .await,
    );
    within("收线停在等清树", h.agent.terminate_stall.wait_stuck()).await;
    // 观测从封口之前装上
    let enqueued = count_enqueues(&h);
    let finished = h.host.counters().finished.load(Ordering::SeqCst);
    let abort = {
        let host = h.host.clone();
        tokio::spawn(async move {
            use iwork_daemon::work::RunningWork;
            tokio::time::timeout(Duration::from_secs(3), host.abort_all()).await
        })
    };
    // 第 2 次走到清树 = 收线的那一趟被封口丢掉、进了封口那一支的清树；这时才放。收线绕过封口裁决的话，第 2 次永远
    // 不会来（收线自己清完树就结束了）
    within("封口那一支开始清树", async {
        while h.agent.terminate_stall.calls() < 2 {
            tokio::task::yield_now().await;
        }
    })
    .await;
    h.agent.terminate_stall.release();
    let report = within("封口回来", abort)
        .await
        .unwrap()
        .expect("封口 3 秒内回来");
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert_eq!(enqueued.load(Ordering::SeqCst), 0, "没有新入队的提交");
    assert_eq!(
        h.host.counters().finished.load(Ordering::SeqCst),
        finished,
        "收线那一趟被丢掉，不在锁里记结局"
    );
    assert_eq!(report.not_cleared, 0, "清树做了：{report:?}");
    assert!(
        !events(&h.raw)
            .await
            .iter()
            .any(|(_, e)| matches!(e, DomainEvent::ConversationDeleted(_))),
        "删除没有往下走"
    );
    h.stop().await;
}

// ---------- E4：闸在入队点上（真引擎） ----------

const HELPER: u128 = 40;
const TURN: u128 = 20;
const APPROVAL: u128 = 30;

/// 夹具对话里助手 HELPER 的一个回合：等审批到期中断，审批还挂着（中断不作废审批）。
fn interrupted_with_pending_approval() -> Vec<DomainEvent> {
    use iwork_domain::event::{ApprovalRequested, AssistantCreated, TurnQueued, TurnStateChanged};
    use iwork_domain::turn::InterruptReason;
    let mut t = turn(TURN, CONV);
    t.assistant_id = aid(HELPER);
    let step = |from, to| -> DomainEvent {
        TurnStateChanged {
            turn_id: tid(TURN),
            conversation_id: Some(cid(CONV)),
            from,
            to,
            reason: None,
        }
        .into()
    };
    vec![
        AssistantCreated {
            assistant: assistant(HELPER, false),
        }
        .into(),
        TurnQueued { turn: t }.into(),
        step(TurnState::Queued, TurnState::Running),
        step(TurnState::Running, TurnState::WaitingApproval),
        ApprovalRequested {
            approval: approval(APPROVAL, TURN, CONV),
        }
        .into(),
        step(
            TurnState::WaitingApproval,
            TurnState::Interrupted {
                reason: InterruptReason::ApprovalWaitLimit,
            },
        ),
    ]
}

fn assistant_deleted() -> DomainEvent {
    iwork_domain::event::AssistantDeleted {
        assistant_id: aid(HELPER),
        name_snapshot: "a40".into(),
    }
    .into()
}

fn engine_events(evs: &[(u64, DomainEvent)]) -> usize {
    evs.iter()
        .filter(|(_, e)| {
            matches!(
                e,
                DomainEvent::ApprovalDecided(_)
                    | DomainEvent::ApprovalVoided(_)
                    | DomainEvent::TurnStateChanged(_)
                    | DomainEvent::TurnFinished(_)
            )
        })
        .count()
}

/// E4 ②：宿主补账（D5）经引擎提交，引擎压在追平上（存储的读不返回）时封口 → 放开：没有新入队的提交，库里没有
/// `Failed` 与审批作废。（等的人到没到 30 秒放手不影响入不入队，这里不等。）
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn sealed_commit_with_parked_on_catch_up() {
    let mut h = harness(Opts::default()).await;
    h.agent.hold.store(true, Ordering::SeqCst);
    h.gate.refuse_when(|r| finishes(r, TurnState::Completed));
    // 不起 engine-sync：追平只来自宿主（对账的一次、补账经引擎提交的一次）
    h.start_host_only();
    let t = h.send(1, "hi").await;
    h.agent.wait_prompts(1).await;
    h.knobs.read_events.arm();
    h.agent.finish_one();
    // 对账那一轮的追平 5 秒到时放手，之后补账经引擎提交，又压在追平上：两处都停着
    within("补账压在追平上", async {
        while h.knobs.read_events.stuck.load(Ordering::SeqCst) < 2 {
            tokio::task::yield_now().await;
        }
    })
    .await;
    let before = engine_events(&events(&h.raw).await);
    seal(&h).await;
    let enqueued = count_enqueues(&h);
    release(&h.knobs.read_events).await;
    assert_eq!(enqueued.load(Ordering::SeqCst), 0, "没有新入队的提交");
    assert_eq!(engine_events(&events(&h.raw).await), before);
    assert_ne!(h.turn_state(t).await, TurnState::Failed);
    h.stop().await;
}

/// E4 ②：引擎压在拿令牌上（同一个对话的令牌被一个停在提交前的 `DecideApproval` 占着）时，宿主收掉没了归属的已中断
/// 回合（经引擎作废它的审批）→ 封口 → 放开：两笔都入不了队，库里没有审批决定、作废与回合收尾。
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn sealed_commit_with_parked_on_token() {
    let mut h = harness(Opts {
        extra: interrupted_with_pending_approval(),
        ..Opts::default()
    })
    .await;
    h.start_host_only();
    let decide_id = fresh_id(50);
    h.knobs.commit.select(move |r| r.command_id == decide_id);
    h.knobs.commit.stall.arm();
    let decide = {
        let d = h.d.clone();
        tokio::spawn(async move {
            d.handle(
                &ctx(),
                iwork_proto::pb::Request {
                    request_id: 1,
                    body: Some(iwork_proto::pb::request::Body::Command(
                        iwork_proto::pb::Command {
                            command_id: decide_id.to_string(),
                            kind: Some(allow(apid(APPROVAL))),
                        },
                    )),
                },
            )
            .await
        })
    };
    within(
        "批准那一笔拿着令牌停在提交前",
        h.knobs.commit.stall.wait_stuck(),
    )
    .await;
    let before = engine_events(&events(&h.raw).await);
    // 助手没了：宿主要经引擎收掉这个已中断的回合，排在令牌上
    // 宿主那一轮会停在拿令牌上，轮数不会前进；看它经引擎提交之前那次追平的读发出去了
    let reads = h.knobs.read_events.calls();
    h.raw
        .commit(cmd(60, vec![assistant_deleted()]))
        .await
        .unwrap();
    // 删助手不在宿主的唤醒登记表里（正式接线里是 engine-sync 追平后叫醒它；这一格没起 engine-sync）
    h.host.wake();
    within("宿主经引擎提交，先追平", async {
        while h.knobs.read_events.calls() < reads + 2 {
            tokio::task::yield_now().await;
        }
    })
    .await;
    tokio::time::sleep(Duration::from_millis(200)).await;
    seal(&h).await;
    let enqueued = count_enqueues(&h);
    release(&h.knobs.commit.stall).await;
    let answer = within("批准回来", decide).await.unwrap();
    assert!(
        matches!(&answer.body, Some(iwork_proto::pb::response::Body::Error(e)) if e.retryable),
        "批准回可重试的错误：{answer:?}"
    );
    assert_eq!(enqueued.load(Ordering::SeqCst), 0, "两笔都入不了队");
    assert_eq!(engine_events(&events(&h.raw).await), before);
    h.stop().await;
}

/// E4 ②：屏障的空提交。占着令牌的那一笔在等回执时被中止（令牌记着「有一笔去向不明」）→ 封口 → 下一位拿令牌先要
/// 排一笔空提交：回可重试的错误，没有入队。
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn sealed_barrier_commit() {
    let h = harness(Opts {
        extra: interrupted_with_pending_approval(),
        ..Opts::default()
    })
    .await;
    let first = fresh_id(50);
    h.knobs.commit.select(move |r| r.command_id == first);
    h.knobs.commit.stall.arm();
    let task = {
        let d = h.d.clone();
        tokio::spawn(async move {
            d.handle(
                &ctx(),
                iwork_proto::pb::Request {
                    request_id: 1,
                    body: Some(iwork_proto::pb::request::Body::Command(
                        iwork_proto::pb::Command {
                            command_id: first.to_string(),
                            kind: Some(allow(apid(APPROVAL))),
                        },
                    )),
                },
            )
            .await
        })
    };
    within(
        "第一笔拿着令牌停在提交前",
        h.knobs.commit.stall.wait_stuck(),
    )
    .await;
    task.abort();
    let _ = task.await;
    h.knobs.commit.stall.release();
    seal(&h).await;
    let enqueued = count_enqueues(&h);
    let answer = h.ask(fresh_id(51), allow(apid(APPROVAL))).await;
    assert!(
        matches!(&answer.body, Some(iwork_proto::pb::response::Body::Error(e)) if e.retryable),
        "屏障那一笔入不了队，回可重试的错误：{answer:?}"
    );
    assert_eq!(enqueued.load(Ordering::SeqCst), 0, "屏障的空提交也没入队");
    assert!(
        !events(&h.raw)
            .await
            .iter()
            .any(|(_, e)| matches!(e, DomainEvent::ApprovalDecided(_)))
    );
    h.stop().await;
}

/// E4 ②：清孤儿。封口之后助手没了、它名下的审批成了孤儿：`sync()` 清孤儿那一笔入不了队，回错误；审批没被作废。
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn sealed_orphan_sweep() {
    use iwork_daemon::host::ports::HostEngine;
    let h = harness(Opts {
        extra: interrupted_with_pending_approval(),
        ..Opts::default()
    })
    .await;
    seal(&h).await;
    let enqueued = count_enqueues(&h);
    // 绕过闸直接落盘：造出孤儿
    h.raw
        .commit(cmd(60, vec![assistant_deleted()]))
        .await
        .unwrap();
    let r = HostEngine::sync(&h.engine).await;
    assert!(r.is_err(), "清孤儿那一笔入不了队：{r:?}");
    assert_eq!(enqueued.load(Ordering::SeqCst), 0);
    assert!(
        !events(&h.raw)
            .await
            .iter()
            .any(|(_, e)| matches!(e, DomainEvent::ApprovalVoided(_))),
        "审批没被作废"
    );
    h.stop().await;
}

/// E4 ②：权限请求路径。存储的闸封了之后，一个在跑的回合发来权限请求（直接交给引擎给这条会话的判定方）：要问用户，
/// 引擎记不下这条审批，答拒绝；没有入队，库里没有新的审批事件。
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn sealed_permission_path_denies() {
    use iwork_core::turn::PermissionDecider;
    use iwork_daemon::host::ports::{HostEngine, HostEnv};
    use iwork_daemon::host::prepare::prepare;
    use iwork_ports::read_model::ReadModelPort;
    let mut h = harness(Opts::default()).await;
    h.agent.hold.store(true, Ordering::SeqCst);
    h.start_host();
    let t = h.send(1, "跑着").await;
    h.agent.wait_prompts(1).await;
    let read = read_model(&h.raw);
    let (turn, c) = read.turn_with_conversation(t).await.unwrap().unwrap();
    let inputs = read.snapshot_inputs(c.id).await.unwrap();
    let env = HostEnv {
        platform: HostEnv::platform(),
        home: h.env.tmp.path().join("home"),
        data_dir: h.env.dir.root().into(),
        tmp_dirs: vec![std::env::temp_dir()],
    };
    let fs = iwork_host::fs::OsPathResolver::new(env.home.clone(), false);
    let keys = prepare(&env, &fs, &inputs, &turn).unwrap().keys;
    let decider = HostEngine::decider_for(&h.engine, keys);
    h.gate.seal();
    let enqueued = count_enqueues(&h);
    let req = iwork_ports::agent::PermissionRequest {
        tool: iwork_domain::action::ToolRef("Bash".into()),
        raw_input: iwork_ports::agent::RawToolInput(r#"{"command":"git push"}"#.into()),
        bypass_sandbox: false,
        updates_before: 1,
        cwd: None,
    };
    let decided = within("引擎答了", decider.decide(t, &req)).await;
    assert!(
        matches!(
            decided.decision,
            iwork_ports::agent::PermissionDecision::Deny { .. }
        ),
        "记不下就拒绝：{:?}",
        decided.decision
    );
    assert_eq!(decided.approval, None);
    assert_eq!(enqueued.load(Ordering::SeqCst), 0);
    assert!(
        !events(&h.raw)
            .await
            .iter()
            .any(|(_, e)| matches!(e, DomainEvent::ApprovalRequested(_))),
        "没有新的审批事件"
    );
    h.stop().await;
}

/// E4 ③：补记绑定回退的那一笔在清树回来之后才走到提交，封口发生在等清树的时候。这里不经宿主的 `select`，直接驱动
/// 一条会话线跑回合：续接 rt-0 时运行时说记录丢了 → 新开 rt-new → 首条 prompt 不收 → 编排收尾先清树（停住）再补记
/// rt-0。封口 → 放开：补记那一笔入不了队（是闸挡住的，不是被丢掉的），编排以存储忙的错误收场，库里没有补记的绑定。
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn sealed_settle_after_retire_wait() {
    use iwork_app::message_blocks::message_blocks;
    use iwork_core::turn::{RunTag, SessionLine, TurnEntry, TurnPlan, TurnPorts, TurnRunError};
    use iwork_daemon::gate::GatedStore;
    use iwork_daemon::host::ports::{HostEngine, HostEnv};
    use iwork_daemon::host::prepare::prepare;
    use iwork_daemon::time::{MonotonicIds, SystemClock, TokioTimer};
    use iwork_ports::read_model::ReadModelPort;

    // 宿主不起：这个回合由测试自己驱动
    let h = harness(Opts::default()).await;
    let t = h.send(1, "直接跑").await;
    let read = read_model(&h.raw);
    let (turn, c) = read.turn_with_conversation(t).await.unwrap().unwrap();
    let inputs = read.snapshot_inputs(c.id).await.unwrap();
    let request = read.turn_request(t).await.unwrap().expect("有请求");
    let env = HostEnv {
        platform: HostEnv::platform(),
        home: h.env.tmp.path().join("home"),
        data_dir: h.env.dir.root().into(),
        tmp_dirs: vec![std::env::temp_dir()],
    };
    let fs = iwork_host::fs::OsPathResolver::new(env.home.clone(), false);
    let prepared = prepare(&env, &fs, &inputs, &turn).unwrap();
    let plan = TurnPlan {
        turn,
        entry: TurnEntry::New,
        assistant_name: prepared.assistant_name,
        context_project_id: prepared.context_project_id,
        spec: prepared.spec,
        blocks: message_blocks(&request).unwrap(),
        changes: None,
        run: RunTag(1),
    };
    let decider = Arc::new(HostEngine::decider_for(&h.engine, prepared.keys));
    let mut line = SessionLine::new(decider, Some("rt-0".into()));
    h.agent.lose_on_resume.store(true, Ordering::SeqCst);
    *h.agent.next_id.lock().unwrap() = Some("rt-new".into());
    h.agent.fail_next_prompt.store(true, Ordering::SeqCst);
    h.agent.terminate_stall.arm();
    let store = support::hooked::HookedStore {
        inner: GatedStore::new(h.raw.clone(), h.gate.clone()),
        knobs: h.knobs.clone(),
    };
    let agent = FakeAgent(h.agent.clone());
    let run = tokio::spawn(async move {
        let (clock, ids) = (SystemClock::new(), MonotonicIds::new(SystemClock::new()));
        let ports = TurnPorts {
            store: &store,
            clock: &clock,
            ids: &ids,
            live: &NoLive,
            timer: &TokioTimer,
            history: &read,
        };
        line.run_turn(&agent, ports, plan).await
    });
    within("收尾停在等清树", h.agent.terminate_stall.wait_stuck()).await;
    h.gate.seal();
    let enqueued = count_enqueues(&h);
    h.agent.terminate_stall.release();
    let result = within("编排收场", run).await.unwrap();
    assert!(
        matches!(
            result,
            Err(TurnRunError::Storage(
                iwork_ports::store::StorageError::Busy
            ))
        ),
        "补记那一笔被闸挡下：{result:?}"
    );
    assert_eq!(enqueued.load(Ordering::SeqCst), 0, "没有入队");
    assert!(
        !events(&h.raw).await.iter().any(|(_, e)| matches!(
            e,
            DomainEvent::TurnSessionBound(b) if b.runtime_session_id == "rt-0"
        )),
        "库里没有补记的绑定"
    );
}
