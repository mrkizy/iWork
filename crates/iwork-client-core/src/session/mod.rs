//! 会话主循环（T09 方案 2.3）：一个 tokio 任务独占状态库。连接来的数据、网页来的操作、定时器进同一个
//! `select!`，顺序处理，没有锁。
//!
//! - 连上 → 一次写出全部范围的 `Subscribe`（同一个 `from_seq`），在飞命令按原 `command_id` 重发；全部
//!   `Subscribed` 应用完才算 Live。
//! - 断了 → 立即重拨，之后按 `Backoff` 退避；在飞视图查询表整张清空，每种用途从状态里重新算「还要再发」。
//! - 网页取帧是一次长等待：没有可交的就挂着，状态变了或到了 16ms 再试。

pub mod dial;
pub mod reconnect;

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::sync::Arc;
use std::time::{Duration, SystemTime};

use iwork_ipc::client::{Backoff, Client, ClientError, Incoming, IncomingItem};
use iwork_proto::pb;
use tokio::sync::{mpsc, oneshot};
use tokio::task::JoinHandle;
use tokio::time::Instant;

use crate::attach;
use crate::derive::Ctx;
use crate::frame::{FrameEngine, Pull};
use crate::intent::{self, Action, Effect, Local};
use crate::store::{Connection, FollowUp, Item, QueryPurpose, Row, ScopeKind, Store, SummaryTable};
use dial::{DialError, Dialer};
use reconnect::{ReconnectState, Retry};

/// 流式缺口要检查点：每个对话每秒最多一次（方案 2.2）。
const CHECKPOINT_INTERVAL: Duration = Duration::from_secs(1);
/// `MarkRead` 每秒最多一次（方案 2.7）。
const MARK_READ_INTERVAL: Duration = Duration::from_secs(1);
/// 翻页与「全部对话」每批条数。
pub const PAGE_SIZE: u32 = 50;

/// 取帧失败。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FrameError {
    /// 带的是旧代次，或等待期间网页重新 attach 了。
    StaleEpoch,
    /// 同一时刻最多挂一个取帧：新来的顶掉了这一个。
    Superseded,
    /// 会话已结束。
    Closed,
}

/// 壳发给会话的请求。
pub enum Request {
    Attach(oneshot::Sender<u32>),
    NextFrame(
        pb::NextFrame,
        oneshot::Sender<Result<pb::FrameChunk, FrameError>>,
    ),
    Intent(pb::UiIntent, oneshot::Sender<pb::IntentAck>),
    /// 壳自己观察到的事（不经网页）。
    Shell(ShellEvent),
}

/// 壳观察到、要交给会话的事。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ShellEvent {
    /// 窗口获得或失去焦点。
    Focused(bool),
    /// 文件拖进窗口：Rust 侧拿到的绝对路径，附到当前可见的对话。
    Dropped(Vec<String>),
    /// 点了系统通知：跳到那个对话（拿不到对话 id 时为 None，跳到最近一条待处理项）。
    NotificationClicked(Option<String>),
    /// 菜单「完全退出 iWork」：先让网页弹确认框。
    QuitRequested,
}

/// 一次拨号的结果，连同拨号器一起还回来。
type Dialed<D> = (D, Result<(Client, Incoming), DialError>);

/// 会话内部任务回来的结果。
enum Internal {
    CommandDone {
        command_id: String,
        result: Result<Box<pb::Response>, ClientError>,
    },
    QueryFailed {
        epoch: u64,
        request_id: u64,
    },
    /// 订阅被后台拒了（或没写出去）：按这条连接失败处理。
    SubscribeFailed {
        epoch: u64,
        id: u32,
        error: String,
    },
    /// 拖入的附件处理完：错误说明或成功（None）。
    AttachDone(Option<String>),
}

struct InFlight {
    command: pb::Command,
    reply: Option<oneshot::Sender<pb::IntentAck>>,
}

struct Waiter {
    epoch: u32,
    applied: Option<(u64, u32)>,
    tx: oneshot::Sender<Result<pb::FrameChunk, FrameError>>,
}

/// 观测值，给测试与基准读（不参与逻辑）。
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Stats {
    pub frames_built: u64,
    pub handed_bytes: u64,
    pub held_bytes: usize,
    pub refresh_rounds: u64,
    pub dials: u64,
    pub resent_commands: u64,
    /// 主循环转了几轮（空闲时应当不转，审查 1/F-1 的守卫）。
    pub loop_turns: u64,
    /// 发出的视图查询条数。
    pub view_queries: u64,
}

pub struct Session<D: Dialer> {
    store: Store,
    engine: FrameEngine,
    reconnect: ReconnectState,
    backoff: Backoff,
    dialer: Option<D>,
    dial_task: Option<JoinHandle<Dialed<D>>>,
    retry_at: Option<Instant>,
    client: Option<Arc<Client>>,
    incoming: Option<Incoming>,
    ever_live: bool,
    queries: HashMap<u64, QueryPurpose>,
    next_sub_id: u32,
    outbox: BTreeMap<String, InFlight>,
    internal_tx: mpsc::UnboundedSender<Internal>,
    internal_rx: mpsc::UnboundedReceiver<Internal>,
    waiter: Option<Waiter>,
    frame_due: Option<Instant>,
    effects: mpsc::UnboundedSender<Effect>,
    checkpoint_last: HashMap<String, Instant>,
    mark_read_last: Option<Instant>,
    want_older: bool,
    want_tail: bool,
    list_append: bool,
    pending_known: Option<BTreeSet<String>>,
    notified: BTreeSet<String>,
    stats: Stats,
    stats_tx: Option<tokio::sync::watch::Sender<Stats>>,
    /// 每条连接一个代次：内部任务的结果带着它，断线重连后迟到的旧结果对得上号才处理（审查 1/F-9）。
    conn_epoch: u64,
    /// 视图查询失败后的退避，按用途种类（审查 1/F-5）。
    query_backoff: HashMap<&'static str, (Instant, Backoff)>,
    /// 时钟（方案 2.4 第 10 条：注入）。一轮循环只读一次，service 与 next_deadline 用同一个时刻（审查 2/F-1）。
    clock: Clock,
}

/// 会话用的时钟。
pub type Clock = Arc<dyn Fn() -> Instant + Send + Sync>;

/// 默认时钟：tokio 的时间（测试里可暂停）。
pub fn tokio_clock() -> Clock {
    Arc::new(Instant::now)
}

/// 常驻范围（方案 2.3）。
const RESIDENT: [ScopeKind; 5] = [
    ScopeKind::Sidebar,
    ScopeKind::Pending,
    ScopeKind::Quota,
    ScopeKind::RuntimeStatus,
    ScopeKind::Settings,
];

impl<D: Dialer> Session<D> {
    pub fn new(
        dialer: D,
        chunk_max: usize,
        timeline_cap: usize,
        clock: Clock,
        effects: mpsc::UnboundedSender<Effect>,
    ) -> Self {
        let (internal_tx, internal_rx) = mpsc::unbounded_channel();
        let mut store = Store::with_timeline_cap(timeline_cap);
        for (i, scope) in RESIDENT.iter().enumerate() {
            store.register_subscription(u32::try_from(i).unwrap_or(0) + 1, scope.clone());
        }
        Self {
            store,
            engine: FrameEngine::new(chunk_max),
            reconnect: ReconnectState::default(),
            backoff: Backoff::new(),
            dialer: Some(dialer),
            dial_task: None,
            retry_at: Some(Instant::now()),
            client: None,
            incoming: None,
            ever_live: false,
            queries: HashMap::new(),
            next_sub_id: u32::try_from(RESIDENT.len()).unwrap_or(0) + 1,
            outbox: BTreeMap::new(),
            internal_tx,
            internal_rx,
            waiter: None,
            frame_due: None,
            effects,
            checkpoint_last: HashMap::new(),
            mark_read_last: None,
            want_older: false,
            want_tail: false,
            list_append: false,
            pending_known: None,
            notified: BTreeSet::new(),
            stats: Stats::default(),
            stats_tx: None,
            conn_epoch: 0,
            query_backoff: HashMap::new(),
            clock,
        }
    }

    pub fn with_stats(mut self, tx: tokio::sync::watch::Sender<Stats>) -> Self {
        self.stats_tx = Some(tx);
        self
    }

    /// 跑到请求通道关闭。
    pub async fn run(mut self, mut rx: mpsc::UnboundedReceiver<Request>) {
        loop {
            self.stats.loop_turns += 1;
            // 一轮只读一次时钟：service 判「还没到」与 next_deadline 登记截止时间用的必须是同一个时刻，
            // 否则两次读钟之间过去的那个截止时间会被漏登，主循环睡到别的事件来（审查 2/F-1）。
            let now = self.now();
            self.service(now);
            let deadline = self.next_deadline(now);
            tokio::select! {
                req = rx.recv() => match req {
                    Some(r) => self.on_request(r),
                    None => break,
                },
                item = next_incoming(&mut self.incoming) => self.on_incoming(item),
                res = join_dial(&mut self.dial_task) => self.on_dialed(res),
                Some(msg) = self.internal_rx.recv() => self.on_internal(msg),
                () = sleep_until(deadline) => {}
            }
        }
        if let Some(w) = self.waiter.take() {
            let _ = w.tx.send(Err(FrameError::Closed));
        }
    }

    fn now(&self) -> Instant {
        (self.clock)()
    }

    fn ctx() -> Ctx {
        Ctx {
            now: SystemTime::now(),
        }
    }

    fn live(&self) -> bool {
        self.client.is_some() && self.store.connection == Connection::Live
    }

    // ---------- 定时 ----------

    /// 下一次要醒的时刻。只登记「到时真有事要做」的截止时间（审查 1/F-1）；登记了的不论过没过都收
    /// （过了的下一轮立即处理，审查 2/F-1）。`now` 与这一轮 service 用的是同一个时刻。
    fn next_deadline(&self, now: Instant) -> Option<Instant> {
        let mut d: Option<Instant> = None;
        let mut take = |t: Option<Instant>| {
            if let Some(t) = t {
                d = Some(d.map_or(t, |x| x.min(t)));
            }
        };
        if self.dial_task.is_none() && self.client.is_none() && self.dialer.is_some() {
            take(self.retry_at);
        }
        if self.waiter.is_some() {
            take(self.frame_due);
        }
        if self.live() {
            let refresh = self.store.refresh.next_due_at().map(Instant::from_std);
            let blocked = self.backoff_until("summary");
            take(match (refresh, blocked) {
                (Some(r), Some(b)) => Some(r.max(b)),
                (r, None) => r,
                (None, Some(_)) => None,
            });
            take(self.mark_read_due());
            // 退避到期只表示「可以再发」，要不要发由 service 用同一个 now 判过了；没发就是用不着，
            // 只登记还没到期的，免得过期的退避项让主循环空转。
            for (until, _) in self.query_backoff.values() {
                take(Some(*until).filter(|t| *t > now));
            }
        }
        d
    }

    fn backoff_until(&self, kind: &str) -> Option<Instant> {
        self.query_backoff.get(kind).map(|(t, _)| *t)
    }

    fn backed_off(&self, kind: &str, now: Instant) -> bool {
        self.backoff_until(kind).is_some_and(|t| now < t)
    }

    /// 每轮循环之后：该拨就拨、该查就查、取帧的等到了就交。
    fn service(&mut self, now: Instant) {
        if self.client.is_none()
            && self.dial_task.is_none()
            && self.retry_at.is_some_and(|t| t <= now)
            && let Some(mut d) = self.dialer.take()
        {
            self.retry_at = None;
            self.stats.dials += 1;
            self.dial_task = Some(tokio::spawn(async move {
                let r = d.dial().await;
                (d, r)
            }));
        }
        if self.live() {
            let with_list = self.store.conversation_list.is_some();
            if !self.backed_off("summary", now)
                && let Some((round, tables)) = self.store.refresh.due(now.into_std(), with_list)
            {
                self.stats.refresh_rounds += 1;
                for table in tables {
                    match table.query() {
                        Some(q) => self.view_query(q, QueryPurpose::Summary { round, table }),
                        None => self.send_list_query(Some(round)),
                    }
                }
            }
            self.wanted_queries();
            self.mark_read(now);
        }
        self.serve_frame(now);
        self.publish_stats();
    }

    fn publish_stats(&mut self) {
        self.stats.frames_built = self.engine.frames_built();
        self.stats.handed_bytes = self.engine.handed_bytes();
        self.stats.held_bytes = self.engine.held_bytes();
        if let Some(tx) = &self.stats_tx {
            tx.send_if_modified(|s| {
                if *s == self.stats {
                    false
                } else {
                    *s = self.stats.clone();
                    true
                }
            });
        }
    }

    fn serve_frame(&mut self, now: Instant) {
        let Some(w) = self.waiter.take() else {
            return;
        };
        if w.tx.is_closed() {
            return;
        }
        match self.engine.pull(
            &mut self.store,
            w.epoch,
            w.applied,
            now.into_std(),
            Self::ctx(),
        ) {
            Pull::Chunk(c) => {
                self.frame_due = None;
                let _ = w.tx.send(Ok(c));
            }
            Pull::StaleEpoch => {
                let _ = w.tx.send(Err(FrameError::StaleEpoch));
            }
            Pull::Wait(due) => {
                self.frame_due = due.map(Instant::from_std);
                self.waiter = Some(w);
            }
        }
    }

    // ---------- 网页请求 ----------

    fn on_request(&mut self, r: Request) {
        match r {
            Request::Attach(tx) => {
                if let Some(w) = self.waiter.take() {
                    let _ = w.tx.send(Err(FrameError::StaleEpoch));
                }
                let epoch = self.engine.attach(&mut self.store);
                let _ = tx.send(epoch);
            }
            Request::NextFrame(req, tx) => {
                if let Some(w) = self.waiter.take() {
                    let _ = w.tx.send(Err(FrameError::Superseded));
                }
                self.waiter = Some(Waiter {
                    epoch: req.epoch,
                    applied: req.applied.map(|p| (p.frame, p.index)),
                    tx,
                });
            }
            Request::Intent(intent, tx) => self.on_intent(intent, tx),
            Request::Shell(e) => self.on_shell(e),
        }
    }

    fn on_shell(&mut self, e: ShellEvent) {
        match e {
            ShellEvent::Focused(f) => self.store.set_focused(f),
            ShellEvent::Dropped(paths) => {
                let Some(cid) = self.store.visible.conversation_id.clone() else {
                    return;
                };
                let (tx, rx) = oneshot::channel();
                self.on_attach(
                    pb::ui_intent::Kind::AttachPaths(pb::AttachPaths {
                        conversation_id: cid,
                        paths,
                    }),
                    tx,
                );
                // 拖入没有回包可带错误：结果回来后放进输入框上方的提示。
                let internal = self.internal_tx.clone();
                tokio::spawn(async move {
                    if let Ok(ack) = rx.await {
                        let _ = internal.send(Internal::AttachDone(ack.error.map(|e| e.message)));
                    }
                });
            }
            ShellEvent::NotificationClicked(conv) => {
                let target = conv
                    .map(|c| (c, String::new()))
                    .or_else(|| self.latest_pending_target());
                if let Some((c, row)) = target {
                    self.store.request_navigate(c, row);
                }
            }
            ShellEvent::QuitRequested => self.store.set_quit_requested(true),
        }
    }

    /// 最近一条有对话的待处理项：(对话, 行)。
    fn latest_pending_target(&self) -> Option<(String, String)> {
        self.store.summary.pending.iter().rev().find_map(|p| {
            let c = crate::derive::pending_conversation(p)?.to_owned();
            let row = crate::derive::pending::item_id(p)?.to_owned();
            Some((c, row))
        })
    }

    fn on_intent(&mut self, intent: pb::UiIntent, tx: oneshot::Sender<pb::IntentAck>) {
        let ok = pb::IntentAck { error: None };
        match intent::map(&self.store, intent) {
            Action::Nothing => {
                let _ = tx.send(ok);
            }
            Action::Reject(e) => {
                let _ = tx.send(pb::IntentAck { error: Some(e) });
            }
            Action::Effect(e) => {
                let _ = self.effects.send(e);
                let _ = tx.send(ok);
            }
            Action::Local(l) => {
                self.on_local(l);
                let _ = tx.send(ok);
            }
            Action::Command(kind) => self.send_command(kind, Some(tx)),
            Action::Attach(kind) => self.on_attach(kind, tx),
        }
    }

    fn on_local(&mut self, l: Local) {
        match l {
            Local::SetVisible(v) => {
                let old = self.store.visible.conversation_id.clone();
                if v.conversation_list != self.store.visible.conversation_list {
                    self.list_append = false;
                }
                if v.conversation_id != old {
                    self.switch_conversation(old.as_deref(), v.conversation_id.as_deref());
                }
                self.store.set_visible(v);
            }
            Local::SetWindow {
                conversation_id,
                first,
                last,
            } => {
                self.store.set_window(&conversation_id, first, last);
            }
            Local::LoadOlder(_) => self.want_older = true,
            Local::JumpToLatest(_) => self.want_tail = true,
            Local::SetFocused(f) => self.store.set_focused(f),
            Local::LoadMoreConversations => {
                self.list_append = true;
                self.store.set_list_loading();
            }
            Local::Quit { confirm } => {
                self.store.set_quit_requested(false);
                if confirm {
                    // 关后台服务（连着时）再退出；连不上也照样退出客户端。
                    let effects = self.effects.clone();
                    match self.client.clone() {
                        Some(client) => {
                            tokio::spawn(async move {
                                let cmd = pb::Command {
                                    command_id: ulid::Ulid::generate().to_string(),
                                    kind: Some(pb::command::Kind::ShutdownDaemon(
                                        pb::ShutdownDaemon {
                                            cancel_running: true,
                                        },
                                    )),
                                };
                                let _ = tokio::time::timeout(
                                    Duration::from_secs(3),
                                    client.request(pb::request::Body::Command(cmd)),
                                )
                                .await;
                                let _ = effects.send(Effect::Exit);
                            });
                        }
                        None => {
                            let _ = effects.send(Effect::Exit);
                        }
                    }
                }
            }
            Local::Retry => {
                // 拨号器不在（拨号任务异常结束带走了它）时点重试没有用，也不登记截止时间（审查 1/F-1）。
                if self.client.is_none() && self.dialer.is_some() {
                    self.retry_at = Some(self.now());
                }
            }
        }
    }

    /// 打开对话 = `Subscribe{Conversation, from_seq: 0}`（一次全范围快照）；离开 = `Unsubscribe`。
    fn switch_conversation(&mut self, old: Option<&str>, new: Option<&str>) {
        self.want_older = false;
        self.want_tail = false;
        if let Some(old) = old
            && let Some(id) = self
                .store
                .subscription_of(&ScopeKind::Conversation(old.to_owned()))
        {
            self.store.unregister_subscription(id);
            if let Some(c) = &self.client
                && c.unsubscribe_nowait(id).is_err()
            {
                tracing::debug!(target: "iwork_client", id, "退订时连接已关");
            }
        }
        if let Some(new) = new {
            let id = self.next_sub_id;
            self.next_sub_id += 1;
            let scope = ScopeKind::Conversation(new.to_owned());
            self.store.register_subscription(id, scope.clone());
            if let Some(c) = self.client.clone() {
                self.subscribe(&c, id, scope.to_proto(), 0);
            }
        }
    }

    /// 写出一个订阅，并盯着它的结果：后台拒了就按这条连接失败处理（审查 1/F-4），不默默留一个永远 Loading 的视图。
    fn subscribe(&self, client: &Client, id: u32, scope: pb::SubscriptionScope, from_seq: u64) {
        match client.subscribe_nowait(id, scope, from_seq) {
            Ok(rx) => {
                let tx = self.internal_tx.clone();
                let epoch = self.conn_epoch;
                tokio::spawn(async move {
                    let error = match rx.await {
                        Ok(Ok(())) | Ok(Err(ClientError::Closed)) | Err(_) => return,
                        Ok(Err(e)) => e.to_string(),
                    };
                    let _ = tx.send(Internal::SubscribeFailed { epoch, id, error });
                });
            }
            Err(e) => {
                tracing::debug!(target: "iwork_client", id, error = %e, "订阅没写出去，连接已关，重连时一起订");
            }
        }
    }

    // ---------- 命令 ----------

    fn send_command(
        &mut self,
        kind: pb::command::Kind,
        reply: Option<oneshot::Sender<pb::IntentAck>>,
    ) {
        if !self.live() {
            // 断线期间不接新命令（D8：发送禁用，草稿留在网页本地）。
            if let Some(tx) = reply {
                let _ = tx.send(pb::IntentAck {
                    error: Some(pb::IntentError {
                        code: "offline".into(),
                        message: "正在重新连接，连上后再试".into(),
                        retryable: true,
                    }),
                });
            }
            return;
        }
        let command_id = ulid::Ulid::generate().to_string();
        let command = pb::Command {
            command_id: command_id.clone(),
            kind: Some(kind),
        };
        self.outbox.insert(
            command_id,
            InFlight {
                command: command.clone(),
                reply,
            },
        );
        self.dispatch(command);
    }

    fn dispatch(&self, command: pb::Command) {
        let Some(client) = self.client.clone() else {
            return;
        };
        let tx = self.internal_tx.clone();
        tokio::spawn(async move {
            let command_id = command.command_id.clone();
            let result = client
                .request(pb::request::Body::Command(command))
                .await
                .map(Box::new);
            let _ = tx.send(Internal::CommandDone { command_id, result });
        });
    }

    fn on_attach(&mut self, kind: pb::ui_intent::Kind, tx: oneshot::Sender<pb::IntentAck>) {
        let Some(client) = self.client.clone().filter(|_| self.live()) else {
            let _ = tx.send(pb::IntentAck {
                error: Some(pb::IntentError {
                    code: "offline".into(),
                    message: "正在重新连接，连上后再试".into(),
                    retryable: true,
                }),
            });
            return;
        };
        // 附件按批发：不进在飞命令表（断线就整批作废，用户重新添加）。
        let uploads: Result<(Vec<pb::Chunk>, Vec<pb::UploadAttachment>), pb::IntentError> =
            match kind {
                pb::ui_intent::Kind::AttachPaths(x) => {
                    attach::path_commands(&x.conversation_id, &x.paths).map(|c| (Vec::new(), c))
                }
                pb::ui_intent::Kind::AttachPasted(x) => {
                    attach::check_size(&x.name, x.data.len() as u64).map(|()| {
                        let upload_id = ulid::Ulid::generate().to_string();
                        let chunks = attach::chunks(&upload_id, &x.data);
                        let cmd = pb::UploadAttachment {
                            conversation_id: x.conversation_id,
                            name: x.name,
                            mime: x.mime,
                            source: Some(pb::upload_attachment::Source::UploadId(upload_id)),
                        };
                        (chunks, vec![cmd])
                    })
                }
                _ => return,
            };
        let (chunks, commands) = match uploads {
            Ok(x) => x,
            Err(e) => {
                let _ = tx.send(pb::IntentAck { error: Some(e) });
                return;
            }
        };
        tokio::spawn(async move {
            let mut error = None;
            for c in chunks {
                if let Err(e) = client.send_chunk(c).await {
                    error = Some(client_error(&e));
                    break;
                }
            }
            if error.is_none() {
                for up in commands {
                    let cmd = pb::Command {
                        command_id: ulid::Ulid::generate().to_string(),
                        kind: Some(pb::command::Kind::UploadAttachment(up)),
                    };
                    match client.request(pb::request::Body::Command(cmd)).await {
                        Ok(r) => {
                            if let Some(e) = response_error(&r) {
                                error = Some(e);
                                break;
                            }
                        }
                        Err(e) => {
                            error = Some(client_error(&e));
                            break;
                        }
                    }
                }
            }
            let _ = tx.send(pb::IntentAck { error });
        });
    }

    fn on_internal(&mut self, msg: Internal) {
        match msg {
            Internal::CommandDone { command_id, result } => {
                let ack = match result {
                    Err(ClientError::Closed) => {
                        // 连接断了：留在在飞表里，重连后按原 command_id 重发（幂等）。
                        return;
                    }
                    Err(e) => Some(client_error(&e)),
                    Ok(r) => response_error(&r),
                };
                if let Some(f) = self.outbox.remove(&command_id) {
                    if ack.is_none()
                        && matches!(f.command.kind, Some(pb::command::Kind::RevokeRule(_)))
                    {
                        // rule 流不在任何范围里：撤销受理后重查一次（T10 给设置范围加上 rule 流后删掉）。
                        self.store.rules = None;
                        self.store.set_rules_loading();
                    }
                    if let Some(tx) = f.reply {
                        let _ = tx.send(pb::IntentAck { error: ack });
                    }
                }
            }
            Internal::AttachDone(e) => self.store.set_attach_error(e),
            Internal::QueryFailed { epoch, request_id } => {
                if epoch != self.conn_epoch {
                    // 旧连接上的查询：那条连接关闭时在飞表已整张清空，这里对不上号，不能删新连接的同号项。
                    return;
                }
                if let Some(p) = self.queries.remove(&request_id) {
                    tracing::warn!(target: "iwork_client", request_id, purpose = ?p, "视图查询失败，退避后重发");
                    // 属于某一轮摘要重查的查询（四张表与列表页那一条）失败：这一轮作废，标记还在，整轮重发
                    // （审查 2/F-2：列表那一条原来不走这一支，这一轮永远走不完、摘要从此停更）。
                    if matches!(
                        p,
                        QueryPurpose::Summary { .. }
                            | QueryPurpose::ConversationList { round: Some(_), .. }
                    ) {
                        self.store.refresh.abandon_round();
                    }
                    if matches!(p, QueryPurpose::OlderPage { .. }) {
                        self.want_older = false;
                    }
                    let now = self.now();
                    let entry = self
                        .query_backoff
                        .entry(purpose_kind(&p))
                        .or_insert_with(|| (now, Backoff::new()));
                    entry.0 = now + entry.1.next_delay();
                    self.store.set_query_failing(true);
                }
            }
            Internal::SubscribeFailed { epoch, id, error } => {
                if epoch == self.conn_epoch && self.client.is_some() {
                    tracing::warn!(target: "iwork_client", id, error, "订阅被拒，这条连接按失败处理，退避后重连");
                    // 不按「同游标被关」计数：那会在第三次显示「后台负载过高」，和真实原因（订阅被拒）对不上。
                    self.drop_connection("订阅被拒");
                    self.store.connection_closed();
                    self.store.set_connection(Connection::Reconnecting);
                    // TODO(T10)：订阅持续被拒时退避可能长不起来——别的范围的快照应用后会复位 Backoff（审查 3/F-2，
                    // 目前没有已知触发途径）。等 T10 定下拒绝订阅的语义后再补。
                    self.retry_at = Some(self.now() + self.backoff.next_delay());
                }
            }
        }
    }

    // ---------- 视图查询 ----------

    fn view_query(&mut self, query: pb::Query, purpose: QueryPurpose) {
        let Some(client) = &self.client else {
            return;
        };
        match client.view_query_nowait(query) {
            Ok((id, rx)) => {
                self.stats.view_queries += 1;
                self.queries.insert(id, purpose);
                let tx = self.internal_tx.clone();
                let epoch = self.conn_epoch;
                tokio::spawn(async move {
                    match rx.await {
                        // 断线造成的失败不算查询失败：在飞表随连接关闭整张清空，不进退避与界面提示（审查 2/F-3）。
                        Ok(Ok(())) | Ok(Err(ClientError::Closed)) | Err(_) => {}
                        Ok(Err(_)) => {
                            let _ = tx.send(Internal::QueryFailed {
                                epoch,
                                request_id: id,
                            });
                        }
                    }
                });
            }
            Err(e) => tracing::debug!(target: "iwork_client", error = %e, "视图查询没发出去"),
        }
    }

    fn in_flight(&self, f: impl Fn(&QueryPurpose) -> bool) -> bool {
        self.queries.values().any(f)
    }

    /// 在飞视图查询登记表（方案 2.3）：每种用途的「还要再发」都从状态里算，不留「已经发过」的记号。
    fn wanted_queries(&mut self) {
        let now = self.now();
        use pb::query::Kind as Q;
        let mut want: Vec<(pb::Query, QueryPurpose)> = Vec::new();
        if let Some(tl) = &self.store.open {
            let cid = tl.conversation_id.clone();
            let ready = self
                .store
                .subscription_of(&ScopeKind::Conversation(cid.clone()))
                .and_then(|id| self.store.view_state(id))
                == Some(crate::store::ViewState::Ready);
            let window_at_top = match (self.store.window, tl.identities().first()) {
                (Some((first, _)), Some((k, _))) => first <= k.ordinal(),
                _ => false,
            };
            if ready
                && tl.has_older
                && (self.want_older || window_at_top)
                && let Some(oldest) = tl.oldest_message_id()
                && !self.backed_off("older", now)
                && !self.in_flight(|p| matches!(p, QueryPurpose::OlderPage { .. }))
            {
                want.push((
                    pb::Query {
                        kind: Some(Q::GetConversation(pb::GetConversation {
                            conversation_id: cid.clone(),
                            before_message_id: Some(oldest.to_owned()),
                            limit: PAGE_SIZE,
                        })),
                    },
                    QueryPurpose::OlderPage {
                        conversation_id: cid.clone(),
                        before: oldest.to_owned(),
                    },
                ));
            }
            if ready
                && self.want_tail
                && !tl.at_tail
                && !self.backed_off("latest", now)
                && !self.in_flight(|p| matches!(p, QueryPurpose::LatestPage { .. }))
            {
                want.push((
                    latest_page(&cid),
                    QueryPurpose::LatestPage {
                        conversation_id: cid,
                    },
                ));
            }
            if tl.at_tail {
                self.want_tail = false;
            }
        }
        if self.store.visible.settings
            && self.store.rules.is_none()
            && !self.backed_off("rules", now)
            && !self.in_flight(|p| *p == QueryPurpose::Rules)
        {
            want.push((
                pb::Query {
                    kind: Some(Q::ListRules(pb::ListRules { project_id: None })),
                },
                QueryPurpose::Rules,
            ));
        }
        if self.store.visible.settings && !self.backed_off("local_config", now) {
            for rt in self.store.runtimes.keys() {
                if !self.store.local_config.contains_key(rt)
                    && !self.in_flight(|p| matches!(p, QueryPurpose::LocalConfig { runtime_id } if runtime_id == rt))
                {
                    want.push((
                        pb::Query { kind: Some(Q::ListLocalConfig(pb::ListLocalConfig { runtime_id: rt.clone() })) },
                        QueryPurpose::LocalConfig { runtime_id: rt.clone() },
                    ));
                }
            }
        }
        if self
            .store
            .conversation_list
            .as_ref()
            .is_some_and(|l| l.loading)
            && !self.backed_off("list", now)
            && !self.in_flight(|p| matches!(p, QueryPurpose::ConversationList { .. }))
        {
            self.send_list_query(None);
        }
        for (q, p) in want {
            self.view_query(q, p);
        }
    }

    /// 「全部对话」列表页的一批。`round` 有值时它是摘要重查轮的一条：按已加载条数整份重查（列表随事件刷新，
    /// 审查 1/F-3）；没有时是首批或「加载下一批」。
    fn send_list_query(&mut self, round: Option<u64>) {
        use pb::query::Kind as Q;
        let Some(list) = &self.store.conversation_list else {
            return;
        };
        let (before, append, limit) = match round {
            Some(_) => (
                None,
                false,
                u32::try_from(list.items.len())
                    .unwrap_or(u32::MAX)
                    .max(PAGE_SIZE),
            ),
            None => {
                let before = if self.list_append {
                    list.items.last().map(|c| c.id.clone())
                } else {
                    None
                };
                let append = self.list_append && before.is_some();
                (before, append, PAGE_SIZE)
            }
        };
        let query = pb::Query {
            kind: Some(Q::ListConversations(pb::ListConversations {
                project_id: Some(list.query.project_id.clone()),
                title_contains: (!list.query.filter.is_empty()).then(|| list.query.filter.clone()),
                before,
                limit,
            })),
        };
        let purpose = QueryPurpose::ConversationList {
            project_id: list.query.project_id.clone(),
            filter: list.query.filter.clone(),
            append,
            round,
        };
        self.view_query(query, purpose);
    }

    /// MarkRead 的下一次时刻：只有现在就该发（可见、前台、Ready、有未读）而被节流挡住时才有。
    fn mark_read_due(&self) -> Option<Instant> {
        let last = self.mark_read_last?;
        self.mark_read_wanted().then_some(last + MARK_READ_INTERVAL)
    }

    fn mark_read_wanted(&self) -> bool {
        let Some(cid) = self.store.visible.conversation_id.as_deref() else {
            return false;
        };
        self.store.focused
            && self
                .store
                .subscription_of(&ScopeKind::Conversation(cid.to_owned()))
                .and_then(|id| self.store.view_state(id))
                == Some(crate::store::ViewState::Ready)
            && self
                .store
                .summary
                .conversation(cid)
                .is_some_and(|c| c.unread > 0)
    }

    /// 对话可见、窗口在前台、视图 Ready、摘要里有未读时发 `MarkRead`（每秒最多一次）。
    fn mark_read(&mut self, now: Instant) {
        let Some(cid) = self.store.visible.conversation_id.clone() else {
            return;
        };
        if !self.store.focused
            || self
                .mark_read_last
                .is_some_and(|t| now < t + MARK_READ_INTERVAL)
        {
            return;
        }
        let ready = self
            .store
            .subscription_of(&ScopeKind::Conversation(cid.clone()))
            .and_then(|id| self.store.view_state(id))
            == Some(crate::store::ViewState::Ready);
        let unread = self
            .store
            .summary
            .conversation(&cid)
            .is_some_and(|c| c.unread > 0);
        if ready && unread {
            self.mark_read_last = Some(now);
            let up_to_seq = self.store.cursor();
            self.send_command(
                pb::command::Kind::MarkRead(pb::MarkRead {
                    conversation_id: cid,
                    up_to_seq,
                }),
                None,
            );
        }
    }

    // ---------- 连接 ----------

    fn on_dialed(&mut self, res: Result<Dialed<D>, tokio::task::JoinError>) {
        self.dial_task = None;
        let (dialer, result) = match res {
            Ok(x) => x,
            Err(e) => {
                tracing::error!(target: "iwork_client", error = %e, "拨号任务异常结束");
                self.store
                    .set_connection(Connection::Environment(format!("拨号任务异常结束：{e}")));
                return;
            }
        };
        self.dialer = Some(dialer);
        match result {
            Ok((client, incoming)) => self.on_connected(client, incoming),
            Err(e) => {
                let (view, retry) = reconnect::on_dial_error(&e);
                tracing::info!(target: "iwork_client", error = ?e, "连不上后台服务");
                self.store.set_connection(view);
                self.retry_at = match retry {
                    Retry::Backoff => Some(self.now() + self.backoff.next_delay()),
                    Retry::Manual => None,
                };
            }
        }
    }

    fn on_connected(&mut self, client: Client, incoming: Incoming) {
        let from_seq = self
            .reconnect
            .from_seq(self.store.cursor(), self.store.any_loading());
        self.store.resubscribing();
        self.conn_epoch += 1;
        let client = Arc::new(client);
        let subs: Vec<(u32, pb::SubscriptionScope)> = self
            .store
            .subscriptions()
            .map(|(id, s)| (id, s.to_proto()))
            .collect();
        for (id, scope) in subs {
            // 一次写出全部范围，同一个 from_seq（T03 客户端契约）。
            self.subscribe(&client, id, scope, from_seq);
        }
        self.client = Some(client);
        self.incoming = Some(incoming);
        self.store.set_connection(if self.ever_live {
            Connection::Reconnecting
        } else {
            Connection::Connecting
        });
        // 在飞命令按原 command_id 重发（幂等）。
        let resend: Vec<pb::Command> = self.outbox.values().map(|f| f.command.clone()).collect();
        self.stats.resent_commands += resend.len() as u64;
        for c in resend {
            self.dispatch(c);
        }
    }

    fn on_closed(&mut self, why: &str) {
        self.drop_connection(why);
        self.reconnect.on_closed(self.store.cursor());
        self.store.connection_closed();
        self.store.set_overloaded(self.reconnect.overloaded());
        self.store.set_connection(Connection::Reconnecting);
        // 第一次掉线立即重拨；同一游标又被关（续传没送出任何东西就断，多半是后台过载）就按退避来，
        // 退避只在应用了带 seq 的项（真有进展）时复位（审查 1/F-4）。
        self.retry_at = Some(
            self.now()
                + reconnect::close_retry_delay(
                    self.reconnect.same_cursor_closes(),
                    &mut self.backoff,
                ),
        );
    }

    /// 丢掉这条连接与它的在飞状态（断线、订阅被拒共用）。
    fn drop_connection(&mut self, why: &str) {
        tracing::info!(target: "iwork_client", why, "与后台服务的连接断了");
        self.client = None;
        self.incoming = None;
        self.queries.clear();
        // 查询退避属于出错的那条连接：新连接从头来（审查 2/F-3）。
        self.query_backoff.clear();
        self.store.set_query_failing(false);
    }

    fn on_incoming(&mut self, item: Option<IncomingItem>) {
        let item = match item {
            Some(i) => i,
            None => {
                self.on_closed("Incoming 结束");
                return;
            }
        };
        let (applied, from_refresh) = match item {
            IncomingItem::Event(e) => (self.store.apply(Item::Event(e)), false),
            IncomingItem::ViewResult { request_id, result } => {
                let Some(purpose) = self.queries.remove(&request_id) else {
                    tracing::warn!(target: "iwork_client", request_id, "查询结果没有登记的用途，丢掉");
                    return;
                };
                if matches!(purpose, QueryPurpose::OlderPage { .. }) {
                    self.want_older = false;
                }
                if self.query_backoff.remove(purpose_kind(&purpose)).is_some()
                    && self.query_backoff.is_empty()
                {
                    self.store.set_query_failing(false);
                }
                let refresh = matches!(
                    purpose,
                    QueryPurpose::Summary {
                        table: SummaryTable::Pending,
                        ..
                    }
                );
                (
                    self.store.apply(Item::ViewResult { purpose, result }),
                    refresh,
                )
            }
            IncomingItem::Subscribed { subscribed, .. } => {
                let a = self.store.apply(Item::Subscribed(subscribed));
                // 快照带来的待处理项不发通知：只记为已知。
                self.pending_known = Some(self.pending_ids());
                (a, false)
            }
            IncomingItem::ServerError(e) => {
                tracing::warn!(target: "iwork_client", code = e.code, message = %e.message, "后台服务主动报错");
                return;
            }
            IncomingItem::Closed(reason) => {
                self.on_closed(&format!("{reason:?}"));
                return;
            }
        };
        if from_refresh {
            self.notify_new_pending();
        }
        if applied.had_seq {
            self.backoff = Backoff::new();
            self.reconnect.on_applied_seq();
            self.store.set_overloaded(self.reconnect.overloaded());
        }
        for f in applied.follow_ups {
            match f {
                FollowUp::AllReady => {
                    self.ever_live = true;
                    self.store.set_connection(Connection::Live);
                }
                FollowUp::Checkpoint { conversation_id } => {
                    let now = self.now();
                    if self
                        .checkpoint_last
                        .get(&conversation_id)
                        .is_some_and(|t| now < *t + CHECKPOINT_INTERVAL)
                    {
                        continue;
                    }
                    self.checkpoint_last.insert(conversation_id.clone(), now);
                    self.view_query(
                        latest_page(&conversation_id),
                        QueryPurpose::LatestPage { conversation_id },
                    );
                }
            }
        }
    }

    fn pending_ids(&self) -> BTreeSet<String> {
        self.store
            .summary
            .pending
            .iter()
            .filter_map(|p| crate::derive::pending::item_id(p).map(str::to_owned))
            .collect()
    }

    /// 系统通知（D6）：摘要重查后待处理表比上一次多出的项，窗口不在前台或那个对话不可见时发；每个 id 一次。
    fn notify_new_pending(&mut self) {
        let now_ids = self.pending_ids();
        let Some(known) = self.pending_known.replace(now_ids.clone()) else {
            return;
        };
        for item in &self.store.summary.pending {
            let Some(id) = crate::derive::pending::item_id(item) else {
                continue;
            };
            if known.contains(id) || self.notified.contains(id) {
                continue;
            }
            let conv = crate::derive::pending_conversation(item).map(str::to_owned);
            let looking =
                self.store.focused && conv.is_some() && conv == self.store.visible.conversation_id;
            if looking {
                continue;
            }
            self.notified.insert(id.to_owned());
            let _ = self.effects.send(Effect::Notify {
                title: "iWork 需要你处理".into(),
                body: crate::derive::pending::item_title(&self.store, item),
                conversation_id: conv,
            });
        }
    }

    // ---------- 测试与基准用的只读口 ----------

    pub fn store(&self) -> &Store {
        &self.store
    }

    pub fn card_row(&self, id: &str) -> Option<&Row> {
        self.store.open.as_ref().and_then(|t| t.row(id))
    }
}

fn latest_page(cid: &str) -> pb::Query {
    pb::Query {
        kind: Some(pb::query::Kind::GetConversation(pb::GetConversation {
            conversation_id: cid.to_owned(),
            before_message_id: None,
            limit: PAGE_SIZE,
        })),
    }
}

fn response_error(r: &pb::Response) -> Option<pb::IntentError> {
    match &r.body {
        Some(pb::response::Body::Error(e)) => Some(server_error(e)),
        _ => None,
    }
}

/// 后台的错误原样给网页（R6：后台给的文字原样用）；错误码转成小写名。
pub fn server_error(e: &pb::Error) -> pb::IntentError {
    let code = pb::ErrorCode::try_from(e.code)
        .map(|c| {
            c.as_str_name()
                .trim_start_matches("ERROR_CODE_")
                .to_ascii_lowercase()
        })
        .unwrap_or_else(|_| format!("code_{}", e.code));
    pb::IntentError {
        code,
        message: e.message.clone(),
        retryable: e.retryable,
    }
}

fn client_error(e: &ClientError) -> pb::IntentError {
    match e {
        ClientError::Server(s) => server_error(s),
        other => pb::IntentError {
            code: "client".into(),
            message: other.to_string(),
            retryable: matches!(other, ClientError::Closed),
        },
    }
}

async fn next_incoming(incoming: &mut Option<Incoming>) -> Option<IncomingItem> {
    match incoming {
        Some(i) => i.next().await,
        None => std::future::pending().await,
    }
}

async fn join_dial<D>(
    task: &mut Option<JoinHandle<Dialed<D>>>,
) -> Result<Dialed<D>, tokio::task::JoinError> {
    match task {
        Some(t) => t.await,
        None => std::future::pending().await,
    }
}

async fn sleep_until(deadline: Option<Instant>) {
    match deadline {
        Some(d) => tokio::time::sleep_until(d).await,
        None => std::future::pending().await,
    }
}

/// 视图查询用途的种类：退避按种类记（同一种用途的不同参数共用一个退避）。
fn purpose_kind(p: &QueryPurpose) -> &'static str {
    match p {
        QueryPurpose::Summary { .. } => "summary",
        QueryPurpose::LatestPage { .. } => "latest",
        QueryPurpose::OlderPage { .. } => "older",
        QueryPurpose::Rules => "rules",
        QueryPurpose::LocalConfig { .. } => "local_config",
        QueryPurpose::ConversationList { .. } => "list",
    }
}
