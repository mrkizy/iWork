//! 分发入口与命令受理（T10a 方案 A1、A2、A3，7.2 与 7.2a 的 K1、K2）：真存储（带封口闸）+ 真引擎。
//!
//! 这一步还没有回合宿主：回合停在 `Queued`，用例直接断言库里的事件。先后一律用屏障与通知定，不靠 `sleep`
//! （7.2a 表下）。可观测点只在测试构建里有（`test-hooks`）。

#[path = "../../iwork-store/tests/support/fixtures.rs"]
mod fixtures;

use std::collections::BTreeSet;
use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use fixtures::*;
use iwork_agent_acp::{AcpRuntime, RuntimeLaunch};
use iwork_app::codec::ProtoEventCodec;
use iwork_core::approval::engine::{ApprovalEngine, EnginePorts};
use iwork_daemon::gate::{GatedStore, SealGate};
use iwork_daemon::handler::dispatch::{
    CommandPorts, DispatchHooks, Dispatcher, HookFuture, HostState,
};
use iwork_daemon::time::{MonotonicIds, SystemClock, TokioTimer};
use iwork_domain::event::{
    ApprovalRequested, AssistantCreated, ConversationCreated, ConversationDeleting, DomainEvent,
    ProjectAdded, TurnQueued, TurnStateChanged,
};
use iwork_domain::id::{CommandId, RawUlid};
use iwork_domain::turn::{InterruptReason, TurnState};
use iwork_host::fs::OsPathResolver;
use iwork_ipc::peer::UserId;
use iwork_ipc::server::{ConnectionContext, PeerSummary, RequestHandler};
use iwork_ports::store::{EventFilter, StoragePort};
use iwork_proto::pb;
use iwork_store::SqliteReadModel;
use tokio::sync::{mpsc, watch};

type Gated = GatedStore<Store>;
type Engine = ApprovalEngine<
    SystemClock,
    MonotonicIds<SystemClock>,
    Gated,
    AcpRuntime,
    TokioTimer,
    OsPathResolver,
>;
type Disp = Dispatcher<Gated, SqliteReadModel<ProtoEventCodec>, Engine>;

const CONV: u128 = 10;
const PROJECT: u128 = 7;
const IWORK: u128 = 1;
const INTERRUPTED_TURN: u128 = 20;
const WAITING_TURN: u128 = 21;
const APPROVAL: u128 = 30;

/// 宿主开着（这一步还没有宿主）。
struct Open;
impl HostState for Open {
    fn is_closed(&self) -> bool {
        false
    }
}

/// `before_handler` 的屏障：第 n 次调用（从 1 数）报到 `reached`，等到放行数 ≥ n 才往下走。
struct Gate2 {
    calls: AtomicUsize,
    reached: mpsc::UnboundedSender<usize>,
    released: watch::Receiver<usize>,
}

impl Gate2 {
    fn hook(self: Arc<Self>) -> Box<dyn Fn(CommandId) -> HookFuture + Send + Sync> {
        Box::new(move |_id| {
            let me = self.clone();
            Box::pin(async move {
                let n = me.calls.fetch_add(1, Ordering::SeqCst) + 1;
                let _ = me.reached.send(n);
                let mut rx = me.released.clone();
                let _ = rx.wait_for(|r| *r >= n).await;
            })
        })
    }
}

struct H {
    env: Env,
    raw: Arc<Store>,
    gate: Arc<SealGate>,
    gated: Gated,
    engine: Arc<Engine>,
    d: Arc<Disp>,
    /// 屏障二报到：第几次调用
    reached: mpsc::UnboundedReceiver<usize>,
    /// 放行到第几次
    release: watch::Sender<usize>,
    /// 在途表上有人开始等同一个 id（K1 的可观测点）
    waiting: mpsc::UnboundedReceiver<CommandId>,
}

fn seed_events() -> Vec<DomainEvent> {
    let mut interrupted = turn(INTERRUPTED_TURN, CONV);
    interrupted.assistant_id = aid(IWORK);
    let mut waiting = turn(WAITING_TURN, CONV);
    waiting.assistant_id = aid(IWORK);
    let mut events: Vec<DomainEvent> = vec![
        AssistantCreated {
            assistant: assistant(IWORK, true),
        }
        .into(),
        ProjectAdded {
            project: project(PROJECT),
        }
        .into(),
        ConversationCreated {
            conversation: project_conversation(CONV, PROJECT),
        }
        .into(),
        TurnQueued { turn: interrupted }.into(),
        TurnQueued { turn: waiting }.into(),
    ];
    let step = |t: u128, from, to| -> DomainEvent {
        TurnStateChanged {
            turn_id: tid(t),
            conversation_id: Some(cid(CONV)),
            from,
            to,
            reason: None,
        }
        .into()
    };
    events.extend([
        step(INTERRUPTED_TURN, TurnState::Queued, TurnState::Running),
        step(
            INTERRUPTED_TURN,
            TurnState::Running,
            TurnState::Interrupted {
                reason: InterruptReason::Crash,
            },
        ),
        step(WAITING_TURN, TurnState::Queued, TurnState::Running),
        step(WAITING_TURN, TurnState::Running, TurnState::WaitingApproval),
        ApprovalRequested {
            approval: approval(APPROVAL, WAITING_TURN, CONV),
        }
        .into(),
    ]);
    events
}

type Ports = CommandPorts<Gated, SqliteReadModel<ProtoEventCodec>, Engine>;

fn ports_on(raw: &Arc<Store>, gated: &Gated, engine: &Arc<Engine>) -> Ports {
    CommandPorts {
        store: Arc::new(gated.clone()),
        read: Arc::new(read_model(raw)),
        engine: engine.clone(),
        ids: Arc::new(MonotonicIds::new(SystemClock::new())),
        clock: Arc::new(SystemClock::new()),
        host: Arc::new(Open),
    }
}

impl H {
    /// 同一套端口（另起一个分发入口装别的屏障用）。
    fn d_ports(&self) -> Ports {
        ports_on(&self.raw, &self.gated, &self.engine)
    }
}

async fn harness() -> H {
    let env = Env::new();
    let raw = Arc::new(env.open());
    raw.commit(cmd(1, seed_events())).await.expect("夹具");
    let gate = Arc::new(SealGate::default());
    let gated = GatedStore::new(raw.clone(), gate.clone());
    let reviewer = AcpRuntime::new(RuntimeLaunch {
        program: PathBuf::from("/nonexistent/iwork-test-reviewer"),
        args: vec![],
        env: vec![],
    });
    let (engine, plan) = ApprovalEngine::start(
        EnginePorts {
            clock: SystemClock::new(),
            ids: MonotonicIds::new(SystemClock::new()),
            store: gated.clone(),
            reviewer,
            timer: TokioTimer,
            fs: OsPathResolver::new(env.tmp.path().join("home"), false),
        },
        BTreeSet::new(),
        |_, _| None,
    )
    .await
    .expect("引擎起来");
    assert!(plan.is_empty(), "夹具里没有孤儿：{plan:?}");
    let (reached_tx, reached) = mpsc::unbounded_channel();
    let (release, released) = watch::channel(usize::MAX);
    let (waiting_tx, waiting) = mpsc::unbounded_channel();
    let gate2 = Arc::new(Gate2 {
        calls: AtomicUsize::new(0),
        reached: reached_tx,
        released,
    });
    let d = Dispatcher::new(ports_on(&raw, &gated, &engine)).with_hooks(DispatchHooks {
        waiting_for_same_id: Some(Box::new(move |id| {
            let _ = waiting_tx.send(id);
        })),
        before_handler: Some(gate2.hook()),
        before_commit: None,
    });
    H {
        env,
        raw,
        gate,
        gated,
        engine,
        d: Arc::new(d),
        reached,
        release,
        waiting,
    }
}

fn ctx() -> ConnectionContext {
    ConnectionContext {
        connection_id: 1,
        session_id: "s".into(),
        peer: PeerSummary {
            pid: 1,
            user: UserId::Uid(1),
        },
        client_kind: pb::ClientKind::Desktop,
        signature_checked: true,
    }
}

fn fresh_id(n: u128) -> CommandId {
    CommandId::from_raw(RawUlid::from_parts(1 << 40, n))
}

fn request(id: CommandId, kind: pb::command::Kind) -> pb::Request {
    pb::Request {
        request_id: 7,
        body: Some(pb::request::Body::Command(pb::Command {
            command_id: id.to_string(),
            kind: Some(kind),
        })),
    }
}

fn send_message(text: &str) -> pb::command::Kind {
    pb::command::Kind::SendMessage(pb::SendMessage {
        conversation_id: cid(CONV).to_string(),
        text: text.into(),
        attachment_ids: vec![],
    })
}

fn abandon() -> pb::command::Kind {
    pb::command::Kind::AbandonTurn(pb::AbandonTurn {
        turn_id: tid(INTERRUPTED_TURN).to_string(),
    })
}

fn delete() -> pb::command::Kind {
    pb::command::Kind::DeleteConversation(pb::DeleteConversation {
        conversation_id: cid(CONV).to_string(),
    })
}

#[derive(Debug, Clone, PartialEq)]
enum Answer {
    Accepted(pb::CommandAccepted),
    Error(i32, bool, String),
}

fn answer(r: pb::Response) -> Answer {
    match r.body {
        Some(pb::response::Body::Accepted(a)) => Answer::Accepted(a),
        Some(pb::response::Body::Error(e)) => Answer::Error(e.code, e.retryable, e.message),
        other => panic!("{other:?}"),
    }
}

fn code(a: &Answer) -> Option<pb::ErrorCode> {
    match a {
        Answer::Error(c, ..) => pb::ErrorCode::try_from(*c).ok(),
        Answer::Accepted(_) => None,
    }
}

async fn ask(d: &Disp, id: CommandId, kind: pb::command::Kind) -> Answer {
    answer(d.handle(&ctx(), request(id, kind)).await)
}

/// 库里全部事件（解码后）。
async fn events(raw: &Store) -> Vec<(u64, DomainEvent)> {
    raw.read_events(EventFilter::All, iwork_domain::id::Seq(0), 100_000)
        .await
        .unwrap()
        .into_iter()
        .map(|e| (e.seq.0, e.event))
        .collect()
}

async fn count(raw: &Store, f: impl Fn(&DomainEvent) -> bool) -> usize {
    events(raw).await.iter().filter(|(_, e)| f(e)).count()
}

fn is_queued(e: &DomainEvent) -> bool {
    matches!(e, DomainEvent::TurnQueued(_))
}

/// 等待一个「应该发生」的结果：套时限，回归表现为红而不是挂住。
async fn within<T>(what: &str, f: impl std::future::Future<Output = T>) -> T {
    tokio::time::timeout(Duration::from_secs(20), f)
        .await
        .unwrap_or_else(|_| panic!("20 秒内没等到：{what}"))
}

/// 等库的 head 前进到超过 `from`（等第一份那笔确实落盘）。
async fn head_past(raw: &Store, from: u64) {
    within("head 前进", async {
        while raw.head_seq().await.unwrap().0 <= from {
            tokio::task::yield_now().await;
        }
    })
    .await;
}

// ---------- A1：受理在三个位置被中止 ----------

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn handler_abort_three_points() {
    let mut h = harness().await;
    let before = count(&h.raw, is_queued).await;

    // ① 提交之前被中止（停在受理函数开头）：库里没有这条命令的任何事件
    let id1 = fresh_id(1);
    h.release.send_replace(0);
    let task = {
        let d = h.d.clone();
        tokio::spawn(async move { ask(&d, id1, send_message("hi")).await })
    };
    within("受理函数开头", h.reached.recv()).await;
    task.abort();
    let _ = task.await;
    assert_eq!(h.raw.command_receipt(id1).await.unwrap(), None);
    assert_eq!(count(&h.raw, is_queued).await, before, "没落盘就等于没发过");
    h.release.send_replace(usize::MAX);
    // 重发同一个 id：当新命令受理，回合排上
    let first = ask(&h.d, id1, send_message("hi")).await;
    assert!(
        matches!(&first, Answer::Accepted(a) if !a.replayed),
        "{first:?}"
    );
    assert_eq!(h.reached.recv().await, Some(2), "重发那一份过了屏障二");
    assert_eq!(count(&h.raw, is_queued).await, before + 1);

    // ② 提交在途时被中止：那一笔照样落盘，重发拿到同一份回执、事件数不变。
    // 查回执也排在写队列里，所以等它停在受理函数开头（查完回执）再压住写线程，然后放它去提交
    let id2 = fresh_id(2);
    let (enq_tx, mut enq) = mpsc::unbounded_channel();
    h.gate.on_enqueued(move |c| {
        let _ = enq_tx.send(c);
    });
    let head = h.raw.head_seq().await.unwrap().0;
    h.release.send_replace(2);
    let task = {
        let d = h.d.clone();
        tokio::spawn(async move { ask(&d, id2, send_message("again")).await })
    };
    assert_eq!(
        within("第二条到受理函数开头", h.reached.recv()).await,
        Some(3)
    );
    let park = h.raw.park_writer().unwrap();
    park.wait_parked(Duration::from_secs(10));
    h.release.send_replace(usize::MAX);
    loop {
        if within("第二条入队", enq.recv()).await == Some(id2) {
            break;
        }
    }
    task.abort();
    let _ = task.await;
    park.release();
    head_past(&h.raw, head).await;
    let landed = h
        .raw
        .command_receipt(id2)
        .await
        .unwrap()
        .expect("在途的那笔落了盘");
    let again = ask(&h.d, id2, send_message("again")).await;
    assert_eq!(
        again,
        Answer::Accepted(pb::CommandAccepted {
            first_seq: landed.first_seq.0,
            last_seq: landed.last_seq.0,
            replayed: true,
        })
    );
    assert_eq!(count(&h.raw, is_queued).await, before + 2);

    // ③ 提交返回之后（客户端没收到回执）：重发同一份
    let id3 = fresh_id(3);
    let Answer::Accepted(r) = ask(&h.d, id3, send_message("third")).await else {
        panic!()
    };
    let again = ask(&h.d, id3, send_message("third")).await;
    assert_eq!(
        again,
        Answer::Accepted(pb::CommandAccepted {
            replayed: true,
            ..r
        })
    );
    assert_eq!(count(&h.raw, is_queued).await, before + 3);
}

/// A2：重发时状态已经变了（对话在删）仍然是 `replayed`，不是校验错误。
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn replay_returns_same_receipt_after_state_change() {
    let h = harness().await;
    let id = fresh_id(1);
    let Answer::Accepted(r) = ask(&h.d, id, send_message("hi")).await else {
        panic!()
    };
    // 回合停在 Queued（没有宿主），删除只落意图
    let del = ask(&h.d, fresh_id(2), delete()).await;
    assert!(matches!(del, Answer::Accepted(_)), "{del:?}");
    // 对照：一条新的 SendMessage 这时被拒
    let fresh = ask(&h.d, fresh_id(3), send_message("late")).await;
    assert_eq!(code(&fresh), Some(pb::ErrorCode::ConversationDeleting));
    let again = ask(&h.d, id, send_message("hi")).await;
    assert_eq!(
        again,
        Answer::Accepted(pb::CommandAccepted {
            replayed: true,
            ..r
        })
    );
}

/// K1：两份同一个 `command_id` 同时到。第二份卡在在途表上，等第一份回完再查回执，拿到同一份。
/// 坏实现（去掉在途表）：两份都查空、都到屏障二；第一份提交后对话进入 `Deleting`，第二份回 `CONVERSATION_DELETING`。
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn concurrent_same_id_one_receipt() {
    let mut h = harness().await;
    h.release.send_replace(0);
    let id = fresh_id(1);
    let spawn = |d: Arc<Disp>| tokio::spawn(async move { ask(&d, id, send_message("hi")).await });
    let a = spawn(h.d.clone());
    assert_eq!(within("第一份到屏障", h.reached.recv()).await, Some(1));
    let b = spawn(h.d.clone());
    // 两样见到一样：第二份在在途表上等（正确实现），或第二份也到了屏障（坏实现）
    tokio::select! {
        w = h.waiting.recv() => assert_eq!(w, Some(id)),
        r = h.reached.recv() => assert_eq!(r, Some(2)),
    }
    h.release.send_replace(1);
    let first = within("第一份回执", a).await.unwrap();
    let Answer::Accepted(r) = &first else {
        panic!("{first:?}")
    };
    assert!(!r.replayed);
    // 第一份落盘之后对话进入 Deleting
    h.raw
        .commit(cmd(
            900,
            vec![
                ConversationDeleting {
                    conversation_id: cid(CONV),
                }
                .into(),
            ],
        ))
        .await
        .unwrap();
    h.release.send_replace(usize::MAX);
    let second = within("第二份回执", b).await.unwrap();
    assert_eq!(
        second,
        Answer::Accepted(pb::CommandAccepted {
            replayed: true,
            ..*r
        }),
        "两份拿到同一份回执，第二份没有进受理函数"
    );
    assert_eq!(count(&h.raw, is_queued).await, 3, "夹具两个 + 这一条");
}

/// K2：第一份 `AbandonTurn` 的那笔已入队、受理函数被中止；第二份紧跟着到。查回执排在写队列里、在那一笔后面，
/// 查到回执，回 `replayed`，到不了屏障二。坏实现（查回执走读连接）：查空、停在屏障二；那笔落盘后放它去读，
/// 回合已不在已中断，回 `TURN_NOT_RESUMABLE`。放开条件照 7.2a 的 K2。
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn aborted_first_enqueued_commit_is_seen() {
    let mut h = harness().await;
    let id = fresh_id(1);
    let (enq_tx, mut enq) = mpsc::unbounded_channel();
    h.gate.on_enqueued(move |c| {
        let _ = enq_tx.send(c);
    });
    let head = h.raw.head_seq().await.unwrap().0;
    // 第一份先停在屏障二（它的查回执要走写线程，所以先让它查完），这时压住写线程（屏障一），再放它去提交；
    // 第二份之后停在屏障二
    h.release.send_replace(0);
    let first = {
        let d = h.d.clone();
        tokio::spawn(async move { ask(&d, id, abandon()).await })
    };
    assert_eq!(within("第一份到屏障二", h.reached.recv()).await, Some(1));
    let park = h.raw.park_writer().unwrap();
    park.wait_parked(Duration::from_secs(10));
    h.release.send_replace(1);
    loop {
        if within("第一份那笔入队", enq.recv()).await == Some(id) {
            break;
        }
    }
    first.abort();
    let _ = first.await;
    let (rcpt_tx, mut rcpt) = mpsc::unbounded_channel();
    h.raw.on_receipt_enqueued(move |c| {
        let _ = rcpt_tx.send(c);
    });
    let second = {
        let d = h.d.clone();
        tokio::spawn(async move { ask(&d, id, abandon()).await })
    };
    // 「写队列里出现了查回执这一项」或「第二份停在屏障二上」，见到一样就放开屏障一
    tokio::select! {
        c = rcpt.recv() => assert_eq!(c, Some(id)),
        n = h.reached.recv() => assert!(n >= Some(2), "{n:?}"),
    }
    park.release();
    head_past(&h.raw, head).await;
    h.release.send_replace(usize::MAX); // 放开屏障二
    let got = within("第二份回执", second).await.unwrap();
    let landed = h
        .raw
        .command_receipt(id)
        .await
        .unwrap()
        .expect("第一份那笔落了盘");
    assert_eq!(
        got,
        Answer::Accepted(pb::CommandAccepted {
            first_seq: landed.first_seq.0,
            last_seq: landed.last_seq.0,
            replayed: true,
        })
    );
}

/// 第 10 节第 1 条：`DecideApproval` 用客户端的 `command_id` 提交，重发得到同一份回执、不再进引擎。
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn decide_replay_returns_same_receipt() {
    let h = harness().await;
    let id = fresh_id(1);
    let deny = || {
        pb::command::Kind::DecideApproval(pb::DecideApproval {
            approval_id: apid(APPROVAL).to_string(),
            decision: pb::ApprovalDecision::Deny.into(),
            remember: 0,
        })
    };
    let Answer::Accepted(r) = ask(&h.d, id, deny()).await else {
        panic!()
    };
    assert!(!r.replayed);
    let decided = |e: &DomainEvent| matches!(e, DomainEvent::ApprovalDecided(_));
    assert_eq!(count(&h.raw, decided).await, 1);
    let again = ask(&h.d, id, deny()).await;
    assert_eq!(
        again,
        Answer::Accepted(pb::CommandAccepted {
            replayed: true,
            ..r
        })
    );
    assert_eq!(count(&h.raw, decided).await, 1);
    // 对照：换一个 id 再决定，审批已不在待审批
    let other = ask(&h.d, fresh_id(2), deny()).await;
    assert_eq!(code(&other), Some(pb::ErrorCode::ApprovalNotPending));
}

/// 没接的命令回「未接入」，什么都不提交。
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn unwired_commands_answer_unavailable() {
    let h = harness().await;
    let head = h.raw.head_seq().await.unwrap();
    let kinds = [
        pb::command::Kind::CreateAssistant(pb::CreateAssistant { config: None }),
        pb::command::Kind::UploadAttachment(Default::default()),
        pb::command::Kind::ExecuteUserAction(Default::default()),
        pb::command::Kind::SendMessage(pb::SendMessage {
            conversation_id: cid(CONV).to_string(),
            text: "with file".into(),
            attachment_ids: vec!["x".into()],
        }),
    ];
    for (n, kind) in kinds.into_iter().enumerate() {
        let a = ask(&h.d, fresh_id(n as u128 + 1), kind).await;
        assert!(
            matches!(&a, Answer::Error(c, false, m) if *c == pb::ErrorCode::Unavailable as i32 && m.contains("未接入")),
            "{a:?}"
        );
    }
    let q = answer(
        h.d.handle(
            &ctx(),
            pb::Request {
                request_id: 1,
                body: Some(pb::request::Body::Query(pb::Query { kind: None })),
            },
        )
        .await,
    );
    assert_eq!(code(&q), Some(pb::ErrorCode::Unavailable));
    assert_eq!(h.raw.head_seq().await.unwrap(), head, "没有提交");
    // 对照：接了的命令照常受理
    assert!(matches!(
        ask(&h.d, fresh_id(9), send_message("ok")).await,
        Answer::Accepted(_)
    ));
}

/// A5 的表经分发入口走一遍：各状态下 `CancelTurn` / `ResumeTurn` / `CancelAll` 提交了什么。
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn turn_commands_commit_one_batch_each() {
    let h = harness().await;
    let cancel = |t: u128| {
        pb::command::Kind::CancelTurn(pb::CancelTurn {
            turn_id: tid(t).to_string(),
        })
    };
    let resume = |t: u128| {
        pb::command::Kind::ResumeTurn(pb::ResumeTurn {
            turn_id: tid(t).to_string(),
        })
    };
    // 已中断的不能取消
    let a = ask(&h.d, fresh_id(1), cancel(INTERRUPTED_TURN)).await;
    assert_eq!(code(&a), Some(pb::ErrorCode::TurnNotCancellable));
    // 等审批的不能继续
    let a = ask(&h.d, fresh_id(2), resume(WAITING_TURN)).await;
    assert_eq!(code(&a), Some(pb::ErrorCode::TurnNotResumable));
    // 已中断的继续：一条继续意图
    let intents = |e: &DomainEvent| matches!(e, DomainEvent::TurnIntentRecorded(_));
    assert!(matches!(
        ask(&h.d, fresh_id(3), resume(INTERRUPTED_TURN)).await,
        Answer::Accepted(_)
    ));
    assert_eq!(count(&h.raw, intents).await, 1);
    // 排一条新消息，再 CancelAll：等审批的落取消意图、排着的撤回，同一笔；已中断的不动
    assert!(matches!(
        ask(&h.d, fresh_id(4), send_message("hi")).await,
        Answer::Accepted(_)
    ));
    let Answer::Accepted(r) = ask(
        &h.d,
        fresh_id(5),
        pb::command::Kind::CancelAll(pb::CancelAll {
            conversation_id: cid(CONV).to_string(),
        }),
    )
    .await
    else {
        panic!()
    };
    let batch: Vec<DomainEvent> = events(&h.raw)
        .await
        .into_iter()
        .filter(|(s, _)| *s >= r.first_seq && *s <= r.last_seq)
        .map(|(_, e)| e)
        .collect();
    let kinds: Vec<&str> = batch
        .iter()
        .map(|e| match e {
            DomainEvent::TurnIntentRecorded(i) if i.turn_id == tid(WAITING_TURN) => "intent",
            DomainEvent::TurnStateChanged(s) if s.to == TurnState::Cancelled => "withdrawn",
            DomainEvent::TurnFinished(_) => "finished",
            other => panic!("不该在这一笔里：{other:?}"),
        })
        .collect();
    assert_eq!(kinds, ["intent", "withdrawn", "finished"]);
    // G5：受理函数提交的回合事件都带所属对话（步 1b）
    let owners: Vec<_> = batch
        .iter()
        .map(|e| match e {
            DomainEvent::TurnIntentRecorded(i) => i.conversation_id,
            DomainEvent::TurnStateChanged(s) => s.conversation_id,
            DomainEvent::TurnFinished(f) => f.conversation_id,
            other => panic!("不该在这一笔里：{other:?}"),
        })
        .collect();
    assert_eq!(owners, [Some(cid(CONV)); 3]);
    // 已经在取消中、已结束的回合：空的一笔，照样有回执（A1）
    let a = ask(&h.d, fresh_id(6), cancel(WAITING_TURN)).await;
    let Answer::Accepted(empty) = a else {
        panic!("{a:?}")
    };
    assert!(!empty.replayed);
    assert!(h.raw.command_receipt(fresh_id(6)).await.unwrap().is_some());
}

// ---------- A3、G2：发消息与删除任意先后 ----------

/// 库里每个对话：`ConversationDeleting` 之后没有新的 `TurnQueued`；已删的对话名下没有回合与消息。
async fn assert_no_turn_after_delete(raw: &Store, db: &std::path::Path) {
    let evs = events(raw).await;
    let deleting = evs.iter().find_map(|(s, e)| match e {
        DomainEvent::ConversationDeleting(d) if d.conversation_id == cid(CONV) => Some(*s),
        _ => None,
    });
    if let Some(at) = deleting {
        for (s, e) in &evs {
            if let DomainEvent::TurnQueued(q) = e {
                assert!(
                    q.turn.conversation_id != cid(CONV) || *s < at,
                    "seq {s} 的 TurnQueued 排在删除（seq {at}）之后"
                );
            }
        }
    }
    let deleted = evs
        .iter()
        .any(|(_, e)| matches!(e, DomainEvent::ConversationDeleted(_)));
    if deleted {
        let key = cid(CONV).to_string();
        for table in ["turns", "messages"] {
            assert_eq!(
                scalar(
                    db,
                    &format!("SELECT COUNT(*) FROM {table} WHERE conversation_id = '{key}'")
                ),
                0,
                "已删对话名下还有 {table}"
            );
        }
    }
}

#[derive(Debug, Clone, Copy)]
enum Op {
    Send,
    Delete,
    /// 宿主的最后一笔（X4），这里直接提交；回合没到终态时被 `check_cascade` 拒
    Finish,
}

proptest::proptest! {
    #![proptest_config(proptest::prelude::ProptestConfig::with_cases(24))]
    /// 性质：发消息、删除、删完三种提交任意先后（相邻两步并发），库里没有属于在删或已删对话的新回合。
    #[test]
    fn send_vs_delete_orders(ops in proptest::collection::vec(
        proptest::prop_oneof![
            proptest::strategy::Just(Op::Send),
            proptest::strategy::Just(Op::Delete),
            proptest::strategy::Just(Op::Finish),
        ],
        1..6,
    )) {
        let rt = tokio::runtime::Builder::new_multi_thread().worker_threads(2).enable_all().build().unwrap();
        rt.block_on(async {
            let h = harness().await;
            let finish_n = Mutex::new(1000u128);
            let run = |op: Op, n: u128| {
                let d = h.d.clone();
                let raw = h.raw.clone();
                let k = { let mut g = finish_n.lock().unwrap(); *g += 1; *g };
                async move {
                    match op {
                        Op::Send => { ask(&d, fresh_id(n), send_message("x")).await; }
                        Op::Delete => { ask(&d, fresh_id(n), delete()).await; }
                        Op::Finish => {
                            let _ = raw.commit(cmd(k, vec![
                                iwork_domain::event::ConversationDeleted { conversation_id: cid(CONV) }.into(),
                            ])).await;
                        }
                    }
                }
            };
            for (i, pair) in ops.chunks(2).enumerate() {
                let n = i as u128 * 2 + 1;
                match pair {
                    [a, b] => { tokio::join!(run(*a, n), run(*b, n + 1)); }
                    [a] => run(*a, n).await,
                    _ => unreachable!(),
                }
            }
            assert_no_turn_after_delete(&h.raw, &h.env.db()).await;
        });
    }
}

/// A3（G2）的确定性一格：受理函数读到对话还在、算好了用户消息与 `TurnQueued`，提交之前对话进了删除。写线程上的
/// 归属校验拒掉这一笔，受理函数重读，回 `CONVERSATION_DELETING`；库里在删除之后没有新回合。对照（去掉校验）：
/// 回合排在删除之后进来，红。
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn send_racing_delete_is_refused_at_commit() {
    let h = harness().await;
    let (reached_tx, mut reached) = mpsc::unbounded_channel();
    let release = Arc::new(tokio::sync::Notify::new());
    let gate_release = release.clone();
    let calls = Arc::new(AtomicUsize::new(0));
    let first_only = calls.clone();
    let d = Arc::new(Dispatcher::new(h.d_ports()).with_hooks(DispatchHooks {
        waiting_for_same_id: None,
        before_handler: None,
        before_commit: Some(Box::new(move |_| {
            let (tx, r, n) = (reached_tx.clone(), gate_release.clone(), first_only.clone());
            Box::pin(async move {
                // 只压第一次提交（被拒后重算的那次放过去）
                if n.fetch_add(1, Ordering::SeqCst) == 0 {
                    let go = r.notified();
                    let _ = tx.send(());
                    go.await;
                }
            })
        })),
    }));
    let task = {
        let d = d.clone();
        tokio::spawn(async move { ask(&d, fresh_id(1), send_message("hi")).await })
    };
    within("受理函数算好、停在提交之前", reached.recv()).await;
    h.raw
        .commit(cmd(
            900,
            vec![
                ConversationDeleting {
                    conversation_id: cid(CONV),
                }
                .into(),
            ],
        ))
        .await
        .unwrap();
    release.notify_waiters();
    let got = within("受理回来", task).await.unwrap();
    assert_eq!(
        code(&got),
        Some(pb::ErrorCode::ConversationDeleting),
        "{got:?}"
    );
    assert_no_turn_after_delete(&h.raw, &h.env.db()).await;
}

/// 只压 `paused` 那条命令的**第一次**提交（读完、算好、提交之前），每一次提交前都报到 `passes`。
/// 被写线程拒了重算的那一次照常过去，所以 `passes` 收到两次 = 第一笔被拒、重读重算过。
fn pause_first_commit(
    h: &H,
    paused: CommandId,
) -> (
    Arc<Disp>,
    mpsc::UnboundedReceiver<()>,
    Arc<tokio::sync::Notify>,
) {
    let (tx, passes) = mpsc::unbounded_channel();
    let release = Arc::new(tokio::sync::Notify::new());
    let r = release.clone();
    let first = Arc::new(AtomicUsize::new(0));
    let d = Dispatcher::new(h.d_ports()).with_hooks(DispatchHooks {
        before_commit: Some(Box::new(move |id| {
            let (tx, r, first) = (tx.clone(), r.clone(), first.clone());
            Box::pin(async move {
                if id != paused {
                    return;
                }
                let go = r.notified();
                let _ = tx.send(());
                if first.fetch_add(1, Ordering::SeqCst) == 0 {
                    go.await;
                }
            })
        })),
        ..Default::default()
    });
    (Arc::new(d), passes, release)
}

fn patch_model(model: &str) -> pb::command::Kind {
    pb::command::Kind::UpdateProjectSettings(pb::UpdateProjectSettings {
        project_id: pid(PROJECT).to_string(),
        default_model: Some(pb::ModelSelection {
            model_id: model.into(),
            effort: None,
        }),
        ..Default::default()
    })
}

fn patch_mode(mode: pb::ApprovalMode) -> pb::command::Kind {
    pb::command::Kind::UpdateProjectSettings(pb::UpdateProjectSettings {
        project_id: pid(PROJECT).to_string(),
        default_approval_mode: Some(mode.into()),
        ..Default::default()
    })
}

async fn project_settings(raw: &Arc<Store>) -> iwork_domain::project::ProjectSettings {
    use iwork_ports::read_model::ReadModelPort;
    let projects = read_model(raw).projects().await.unwrap();
    projects
        .iter()
        .find(|p| p.project.id == pid(PROJECT))
        .unwrap()
        .project
        .settings
        .clone()
}

/// 项目设置补丁 `late` 读完旧设置、停在提交之前，另一条连接的 `newer` 先落盘；放开 `late` 之后两边的改动都在。
/// `late` 只改模型，`newer` 把项目默认档位从自动改回手动（经 `UpdateProjectSettings` 或 `SetApprovalMode`）。
async fn settings_patch_races(newer: pb::command::Kind) {
    use iwork_domain::approval_mode::ProjectDefaultMode;
    let h = harness().await;
    let auto = ask(&h.d, fresh_id(99), patch_mode(pb::ApprovalMode::Auto)).await;
    assert!(matches!(auto, Answer::Accepted(_)), "{auto:?}");
    let late_id = fresh_id(100);
    let (d, mut passes, release) = pause_first_commit(&h, late_id);
    let late = {
        let d = d.clone();
        tokio::spawn(async move { ask(&d, late_id, patch_model("new-model")).await })
    };
    within("模型补丁读完旧设置、停在提交之前", passes.recv()).await;
    let newer = ask(&d, fresh_id(101), newer).await;
    assert!(matches!(newer, Answer::Accepted(_)), "{newer:?}");
    assert_eq!(
        project_settings(&h.raw).await.default_approval_mode,
        ProjectDefaultMode::Manual
    );
    release.notify_waiters();
    let late = within("模型补丁回来", late).await.unwrap();
    assert!(matches!(late, Answer::Accepted(_)), "{late:?}");
    let settings = project_settings(&h.raw).await;
    assert_eq!(
        settings.default_model.map(|m| m.model_id.0).as_deref(),
        Some("new-model")
    );
    assert_eq!(
        settings.default_approval_mode,
        ProjectDefaultMode::Manual,
        "只改模型的补丁不能把别人刚改的档位盖回去"
    );
    assert!(
        passes.try_recv().is_ok(),
        "第一笔应被写线程拒掉、重读重算过"
    );
}

/// 1/F-1：两条连接各改项目设置的一项。后到的那笔是在旧设置上合并的，写线程拒掉它（`previous` 对不上），
/// 重读重算后两项都在。修之前：后到的整份写回，档位退回自动。
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn settings_patch_racing_another_patch_keeps_both() {
    settings_patch_races(patch_mode(pb::ApprovalMode::Manual)).await;
}

/// 1/F-1 的同类：项目默认档位也能经 `SetApprovalMode`（引擎切档）改，它和设置补丁写的是同一份设置。
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn settings_patch_racing_project_mode_switch_keeps_both() {
    settings_patch_races(pb::command::Kind::SetApprovalMode(pb::SetApprovalMode {
        target: Some(pb::ApprovalModeTarget {
            kind: Some(pb::approval_mode_target::Kind::ProjectId(
                pid(PROJECT).to_string(),
            )),
        }),
        mode: pb::ApprovalMode::Manual.into(),
        follow_supervisor: false,
    }))
    .await;
}

/// 1/F-2：已读位置只往前走。读得早的 `MarkRead(1)` 停在提交之前，另一条连接把位置推到 head；放开之后
/// 写线程拒掉小的那笔，重读后是空的一笔，位置仍是 head。修之前：位置被拨回 1。
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn mark_read_racing_a_further_mark_never_moves_back() {
    use iwork_ports::read_model::{MessagePage, ReadModelPort};
    let h = harness().await;
    let head = h.raw.head_seq().await.unwrap().0;
    assert!(head > 1, "夹具要有不止一条事件，才分得出前后");
    let mark = |up_to_seq| {
        pb::command::Kind::MarkRead(pb::MarkRead {
            conversation_id: cid(CONV).to_string(),
            up_to_seq,
        })
    };
    let read = read_model(&h.raw);
    let last_read = || async {
        read.conversation(
            cid(CONV),
            MessagePage {
                before: None,
                limit: 0,
            },
        )
        .await
        .unwrap()
        .conversation
        .last_read_seq
        .0
    };
    let late_id = fresh_id(100);
    let (d, mut passes, release) = pause_first_commit(&h, late_id);
    let late = {
        let d = d.clone();
        let earlier = mark(1);
        tokio::spawn(async move { ask(&d, late_id, earlier).await })
    };
    within("小的已读位置算好、停在提交之前", passes.recv()).await;
    let newer = ask(&d, fresh_id(101), mark(head)).await;
    assert!(matches!(newer, Answer::Accepted(_)), "{newer:?}");
    assert_eq!(last_read().await, head);
    release.notify_waiters();
    let late = within("小的那笔回来", late).await.unwrap();
    assert!(matches!(late, Answer::Accepted(_)), "{late:?}");
    assert_eq!(last_read().await, head, "已读位置被拨回去了");
    assert!(
        passes.try_recv().is_ok(),
        "第一笔应被写线程拒掉、重读重算过"
    );
}

/// A2 ①：等同一个 `command_id` 的那一份到时退出，不放掉原来占着的那一份；占着的丢弃后才能再进。
#[tokio::test(start_paused = true)]
async fn same_id_wait_timeout_keeps_the_holder() {
    use futures::FutureExt;
    use iwork_daemon::handler::dispatch::{InFlight, SAME_ID_WAIT};
    let table = Arc::new(InFlight::default());
    let id = fresh_id(200);
    let held = table.enter(id, || {}).await;
    let waiting = Arc::new(tokio::sync::Notify::new());
    let task = {
        let table = table.clone();
        let waiting = waiting.clone();
        tokio::spawn(async move {
            tokio::time::timeout(SAME_ID_WAIT, table.enter(id, || waiting.notify_one()))
                .await
                .is_err()
        })
    };
    waiting.notified().await;
    tokio::time::advance(SAME_ID_WAIT).await;
    assert!(task.await.unwrap());
    assert_eq!(table.waiting_len(), 0);
    assert!(
        table.enter(id, || {}).now_or_never().is_none(),
        "到时退出的等待者不能放掉原来占着的那一份"
    );
    drop(held);
    assert!(
        table.enter(id, || {}).now_or_never().is_some(),
        "占着的丢弃后才能再进"
    );
}
