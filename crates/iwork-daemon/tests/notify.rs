//! 系统通知的定夺与在看哪（T10a 方案 2.10、7.2；义务行 N1、N2、A7）：真存储 + 真提交观察者 + `notify` 任务 +
//! `outbox` 任务（系统通知去向接一个记录用的假 `Notifier`）+ 请求入口的查询层（`ReportPresence` 在这里收）。
//!
//! 「定过了」看库里有没有这一项定夺那一笔的 `command_id`；「要发」看 outbox 里有没有这一项的去向；「发了」看假
//! `Notifier` 收到的。

mod support;

use std::sync::{Arc, Mutex};
use std::time::Duration;

use iwork_app::codec::ProtoEventCodec;
use iwork_daemon::attention::Attention;
use iwork_daemon::gate::{GatedStore, SealGate};
use iwork_daemon::handler::NotWired;
use iwork_daemon::handler::query::{Queries, QueryOr};
use iwork_daemon::publish::{PublishObserver, PublishState};
use iwork_daemon::routes::{notify_route, notify_target};
use iwork_daemon::tasks::{notify, outbox};
use iwork_domain::event::{
    ApprovalRequested, AssistantCreated, ConversationCreated, DomainEvent, ProjectAdded,
    TurnQueued, TurnStateChanged,
};
use iwork_domain::id::CommandId;
use iwork_domain::turn::{InterruptReason, TurnState};
use iwork_ipc::peer::UserId;
use iwork_ipc::server::{ConnectionContext, PeerSummary, RequestHandler};
use iwork_ports::notify::{Notifier, NotifyError, PendingItemRef, SystemNotification};
use iwork_ports::read_model::PendingEntry;
use iwork_ports::store::StoragePort;
use iwork_proto::pb;
use iwork_store::SqliteReadModel;
use support::fixtures::*;
use support::turns::Nowhere;
use tokio::sync::Notify;
use tokio::task::JoinHandle;

const PROJECT: u128 = 7;
const CONV: u128 = 10;
const OTHER_CONV: u128 = 11;

#[derive(Default)]
struct FakeNotifier {
    sent: Mutex<Vec<SystemNotification>>,
    arrived: Notify,
}

impl FakeNotifier {
    fn sent(&self) -> Vec<SystemNotification> {
        self.sent.lock().unwrap().clone()
    }
}

impl Notifier for FakeNotifier {
    async fn notify(&self, n: SystemNotification) -> Result<(), NotifyError> {
        self.sent.lock().unwrap().push(n);
        self.arrived.notify_waiters();
        Ok(())
    }
}

/// 一套：库、观察者、两个任务、请求入口的查询层。
struct Rig {
    env: Env,
    raw: Arc<Store>,
    gate: Arc<SealGate>,
    read: Arc<SqliteReadModel<ProtoEventCodec>>,
    attention: Arc<Attention>,
    notifier: Arc<FakeNotifier>,
    front: QueryOr<ProtoEventCodec, NotWired>,
    tasks: Vec<JoinHandle<()>>,
}

impl Rig {
    /// 新库，带项目、两个对话、一个助手。
    async fn new() -> Rig {
        let env = Env::new();
        {
            let raw = env.open();
            raw.commit(cmd(1, seed())).await.unwrap();
            raw.close().unwrap();
        }
        Rig::start(env, Arc::default()).await
    }

    async fn start(env: Env, notifier: Arc<FakeNotifier>) -> Rig {
        let raw = Arc::new(env.open());
        let publishing = PublishState::new();
        raw.set_commit_observer(Box::new(PublishObserver::new(
            Arc::new(Nowhere),
            publishing.clone(),
        )))
        .await
        .unwrap();
        let gate = Arc::new(SealGate::default());
        let gated = Arc::new(GatedStore::new(raw.clone(), gate.clone()));
        let read = Arc::new(read_model(&raw));
        let attention = Arc::new(Attention::new("boot-test".into()));
        let routes = Arc::new(vec![notify_route(read.clone(), notifier.clone())]);
        let tasks = vec![
            tokio::spawn(notify::run(
                gated.clone(),
                read.clone(),
                attention.clone(),
                publishing.notify.clone(),
            )),
            tokio::spawn(outbox::run(
                gated,
                routes,
                publishing.outbox.clone(),
                outbox::log_unclaimed(),
            )),
        ];
        let front = QueryOr::new(Queries::new(read.clone(), attention.clone()), NotWired);
        Rig {
            env,
            raw,
            gate,
            read,
            attention,
            notifier,
            front,
            tasks,
        }
    }

    /// 进程没了以后再起来：任务停掉、库关掉，同一个数据目录再装一套（`Attention` 是新的，没人报过在看哪）。
    async fn restart(self) -> Rig {
        for t in self.tasks {
            t.abort();
            let _ = t.await;
        }
        drop(self.front);
        drop(self.read);
        drop(self.attention);
        let notifier = self.notifier;
        drop(self.raw);
        Rig::start(self.env, notifier).await
    }

    async fn commit(&self, n: u128, events: Vec<DomainEvent>) {
        self.raw.commit(cmd(n, events)).await.unwrap();
    }

    async fn pending(&self) -> Vec<PendingEntry> {
        self.read.view(|tx| tx.pending()).await.unwrap()
    }

    /// 待处理里这一项（必须在）。
    async fn entry(&self, item: PendingItemRef) -> PendingEntry {
        self.pending()
            .await
            .into_iter()
            .find(|e| e.item.item_ref() == item)
            .unwrap_or_else(|| panic!("{} 不在待处理里", item.key()))
    }

    fn decided(&self, id: CommandId) -> bool {
        scalar(
            &self.env.db(),
            &format!("SELECT count(*) FROM commands WHERE command_id = '{id}'"),
        ) == 1
    }

    /// 等这一项定夺完（那一笔落盘）。
    async fn wait_decided(&self, entry: &PendingEntry) {
        let id = notify::command_id_of(entry);
        within(&format!("{} 定夺完", entry.item.item_ref().key()), async {
            while !self.decided(id) {
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        })
        .await;
    }

    /// outbox 里给这一项发通知的条目数。
    fn queued_for(&self, item: PendingItemRef) -> i64 {
        scalar(
            &self.env.db(),
            &format!(
                "SELECT count(*) FROM outbox WHERE target = '{}'",
                notify_target(item).0
            ),
        )
    }

    /// 等假 `Notifier` 一共收到 `n` 条。
    async fn wait_sent(&self, n: usize) -> Vec<SystemNotification> {
        within(&format!("收到 {n} 条通知"), async {
            loop {
                let arrived = self.notifier.arrived.notified();
                let sent = self.notifier.sent();
                if sent.len() >= n {
                    return sent;
                }
                arrived.await;
            }
        })
        .await
    }

    /// 经请求入口报一次在看哪。
    async fn report(
        &self,
        ctx: &ConnectionContext,
        focused: bool,
        conversation: Option<u128>,
        pending_list: bool,
    ) -> pb::Response {
        self.front
            .handle(
                ctx,
                pb::Request {
                    request_id: 9,
                    body: Some(pb::request::Body::Command(pb::Command {
                        command_id: cmd_id(9000 + ctx.connection_id as u128).to_string(),
                        kind: Some(pb::command::Kind::ReportPresence(pb::ReportPresence {
                            window_focused: focused,
                            viewing_conversation_id: conversation.map(|c| cid(c).to_string()),
                            viewing_pending_list: pending_list,
                        })),
                    })),
                },
            )
            .await
    }
}

fn seed() -> Vec<DomainEvent> {
    vec![
        ProjectAdded {
            project: project(PROJECT),
        }
        .into(),
        ConversationCreated {
            conversation: project_conversation(CONV, PROJECT),
        }
        .into(),
        ConversationCreated {
            conversation: project_conversation(OTHER_CONV, PROJECT),
        }
        .into(),
        AssistantCreated {
            assistant: assistant(1, false),
        }
        .into(),
    ]
}

fn step(turn: u128, conv: u128, from: TurnState, to: TurnState) -> DomainEvent {
    TurnStateChanged {
        turn_id: tid(turn),
        conversation_id: Some(cid(conv)),
        from,
        to,
        reason: None,
    }
    .into()
}

const CRASH: TurnState = TurnState::Interrupted {
    reason: InterruptReason::Crash,
};

/// 对话 `conv` 里回合 `turn` 跑起来、停在等审批 `approval` 上。
fn asks(approval: u128, turn: u128, conv: u128) -> Vec<DomainEvent> {
    vec![
        TurnQueued {
            turn: support::fixtures::turn(turn, conv),
        }
        .into(),
        step(turn, conv, TurnState::Queued, TurnState::Running),
        step(turn, conv, TurnState::Running, TurnState::WaitingApproval),
        ApprovalRequested {
            approval: support::fixtures::approval(approval, turn, conv),
        }
        .into(),
    ]
}

fn approval_ref(n: u128) -> PendingItemRef {
    PendingItemRef::Approval(apid(n))
}

fn desktop(connection_id: u64) -> ConnectionContext {
    ConnectionContext {
        connection_id,
        session_id: format!("s{connection_id}"),
        peer: PeerSummary {
            pid: 1,
            user: UserId::Uid(1),
        },
        client_kind: pb::ClientKind::Desktop,
        signature_checked: true,
    }
}

fn dev_tool(connection_id: u64) -> ConnectionContext {
    ConnectionContext {
        client_kind: pb::ClientKind::DevTool,
        ..desktop(connection_id)
    }
}

async fn within<T>(what: &str, f: impl std::future::Future<Output = T>) -> T {
    tokio::time::timeout(Duration::from_secs(20), f)
        .await
        .unwrap_or_else(|_| panic!("20 秒内没等到：{what}"))
}

fn assert_accepted(r: &pb::Response) {
    assert!(
        matches!(r.body, Some(pb::response::Body::Accepted(_))),
        "没受理：{r:?}"
    );
}

/// 没有桌面客户端连着：新的审批照发，正文是待处理页上的文字。
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn notify_without_desktop_client_calls_notifier() {
    let rig = Rig::new().await;
    rig.commit(2, asks(30, 20, CONV)).await;
    let sent = rig.wait_sent(1).await;
    assert_eq!(sent.len(), 1);
    assert_eq!(sent[0].title, "审批");
    assert_eq!(sent[0].body, "审批：npm test");
    assert_eq!(rig.queued_for(approval_ref(30)), 1);
}

/// 窗口在前台、正看着这一项所在的对话或待你处理页：不发（定过、outbox 里没有）。
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn notify_held_back_when_viewing_it() {
    let rig = Rig::new().await;
    // 看着对话 CONV
    assert_accepted(&rig.report(&desktop(1), true, Some(CONV), false).await);
    rig.commit(2, asks(30, 20, CONV)).await;
    rig.wait_decided(&rig.entry(approval_ref(30)).await).await;
    assert_eq!(rig.queued_for(approval_ref(30)), 0, "看着这个对话时不该发");
    // 改看待你处理页：另一个对话里的也不发
    assert_accepted(&rig.report(&desktop(1), true, None, true).await);
    rig.commit(3, asks(31, 21, OTHER_CONV)).await;
    rig.wait_decided(&rig.entry(approval_ref(31)).await).await;
    assert_eq!(
        rig.queued_for(approval_ref(31)),
        0,
        "看着待你处理页时不该发"
    );
    assert!(rig.notifier.sent().is_empty());
}

/// 看着别的对话、或窗口不在前台：照发。
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn notify_sent_when_viewing_other_conversation() {
    let rig = Rig::new().await;
    assert_accepted(&rig.report(&desktop(1), true, Some(OTHER_CONV), false).await);
    rig.commit(2, asks(30, 20, CONV)).await;
    rig.wait_sent(1).await;
    assert_eq!(rig.queued_for(approval_ref(30)), 1);
    // 看着这个对话但窗口在后台
    assert_accepted(
        &rig.report(&desktop(1), false, Some(OTHER_CONV), false)
            .await,
    );
    rig.commit(3, asks(31, 21, OTHER_CONV)).await;
    rig.wait_sent(2).await;
    assert_eq!(rig.queued_for(approval_ref(31)), 1);
}

/// 定过的不再定：断开重连（连接断开清掉在看哪）、后台重启（内存里的记账没了）之后都不再发第二次。
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn notify_not_repeated_after_reconnect_or_restart() {
    let rig = Rig::new().await;
    rig.commit(2, asks(30, 20, CONV)).await;
    rig.wait_sent(1).await;
    // 断开、重连、报一次在看哪，再来一项待处理叫醒任务
    rig.front.connection_closed(&desktop(1)).await;
    assert_accepted(&rig.report(&desktop(2), true, None, false).await);
    rig.commit(3, asks(31, 21, OTHER_CONV)).await;
    rig.wait_sent(2).await;
    // 重启：起来先对一次账
    let rig = rig.restart().await;
    rig.commit(4, asks(32, 22, CONV)).await;
    let sent = rig.wait_sent(3).await;
    assert_eq!(sent.len(), 3);
    for n in [30, 31, 32] {
        assert_eq!(rig.queued_for(approval_ref(n)), 1, "审批 {n} 只排一次");
    }
    // 留一点时间给错误的重发
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert_eq!(rig.notifier.sent().len(), 3);
}

/// 定夺之前已经处理掉的不发：封口时没定成的一项在重启前被处理掉，重启后对账只看还在待处理的。
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn notify_skipped_when_already_decided() {
    use iwork_domain::approval::ApprovalState;
    use iwork_domain::event::ApprovalDecided;
    let rig = Rig::new().await;
    rig.gate.seal();
    rig.commit(2, asks(30, 20, CONV)).await;
    // 封口时定不了；处理掉之后再放开
    rig.commit(
        3,
        vec![
            ApprovalDecided {
                approval_id: apid(30),
                conversation_id: Some(cid(CONV)),
                state: ApprovalState::Denied,
                remember: None,
            }
            .into(),
        ],
    )
    .await;
    let rig = rig.restart().await;
    rig.commit(4, asks(31, 21, OTHER_CONV)).await;
    let sent = rig.wait_sent(1).await;
    assert_eq!(sent.len(), 1);
    assert_eq!(rig.queued_for(approval_ref(30)), 0, "处理掉的不该再排");
    assert_eq!(rig.queued_for(approval_ref(31)), 1);
}

/// 同一回合中断、续接、再中断：两张继续卡片，两次都发。
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn resume_card_notified_on_each_interrupt() {
    let rig = Rig::new().await;
    let resume = PendingItemRef::Resume(tid(20));
    rig.commit(
        2,
        vec![
            TurnQueued {
                turn: support::fixtures::turn(20, CONV),
            }
            .into(),
            step(20, CONV, TurnState::Queued, TurnState::Running),
            step(20, CONV, TurnState::Running, CRASH),
        ],
    )
    .await;
    let first = rig.entry(resume).await;
    rig.wait_decided(&first).await;
    rig.wait_sent(1).await;
    rig.commit(3, vec![step(20, CONV, CRASH, TurnState::Running)])
        .await;
    rig.commit(4, vec![step(20, CONV, TurnState::Running, CRASH)])
        .await;
    let second = rig.entry(resume).await;
    assert_ne!(first.created_seq, second.created_seq);
    rig.wait_decided(&second).await;
    let sent = rig.wait_sent(2).await;
    assert_eq!(sent.len(), 2);
    assert!(sent.iter().all(|n| n.title == "继续或放弃"));
}

/// 在看哪按连接记：只收桌面客户端（别的种类回 PERMISSION_DENIED、不记）；参数不合法回 INVALID_ARGUMENT；
/// 连接断开就删掉它报的那一份。
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn report_presence_routed_by_connection() {
    let rig = Rig::new().await;
    // 开发工具报：拒绝，什么都不记
    let r = rig.report(&dev_tool(5), true, Some(CONV), false).await;
    let Some(pb::response::Body::Error(e)) = &r.body else {
        panic!("开发工具报在看哪该被拒：{r:?}")
    };
    assert_eq!(e.code, pb::ErrorCode::PermissionDenied as i32);
    assert!(!e.message.is_empty(), "拒绝要带原因");
    assert!(rig.attention.presences().is_empty());
    // 两个都设：不合法
    let r = rig.report(&desktop(1), true, Some(CONV), true).await;
    let Some(pb::response::Body::Error(e)) = &r.body else {
        panic!("两个都设该被拒：{r:?}")
    };
    assert_eq!(e.code, pb::ErrorCode::InvalidArgument as i32);
    assert!(rig.attention.presences().is_empty());
    // 两条桌面连接各记一份；后报的覆盖先报的
    assert_accepted(&rig.report(&desktop(1), true, Some(CONV), false).await);
    assert_accepted(&rig.report(&desktop(2), false, None, true).await);
    assert_accepted(&rig.report(&desktop(1), true, Some(OTHER_CONV), false).await);
    let mut seen = rig.attention.presences();
    seen.sort_by_key(|p| p.window_focused);
    assert_eq!(seen.len(), 2);
    assert_eq!(
        seen[1].viewing,
        iwork_ports::notify::Viewing::Conversation(cid(OTHER_CONV))
    );
    // 连接 1 断开：它那份没了，连接 2 的还在
    rig.front.connection_closed(&desktop(1)).await;
    let left = rig.attention.presences();
    assert_eq!(left.len(), 1);
    assert_eq!(left[0].viewing, iwork_ports::notify::Viewing::PendingList);
    // 断开的那个对话里来了审批：照发（没人看着它）
    rig.commit(2, asks(30, 20, OTHER_CONV)).await;
    rig.wait_sent(1).await;
}

/// 定夺的提交经带封口闸的存储：封口之后不再定（不排通知、不记 `command_id`）。
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn notify_task_respects_seal() {
    let rig = Rig::new().await;
    rig.gate.seal();
    rig.commit(2, asks(30, 20, CONV)).await;
    let entry = rig.entry(approval_ref(30)).await;
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert!(!rig.decided(notify::command_id_of(&entry)), "封口后不该定");
    assert_eq!(rig.queued_for(approval_ref(30)), 0);
    assert!(rig.notifier.sent().is_empty());
}
