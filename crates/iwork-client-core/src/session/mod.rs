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
        request_id: u64,
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
    pub fn new(dialer: D, chunk_max: usize, effects: mpsc::UnboundedSender<Effect>) -> Self {
        let (internal_tx, internal_rx) = mpsc::unbounded_channel();
        let mut store = Store::new();
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
        }
    }

    pub fn with_stats(mut self, tx: tokio::sync::watch::Sender<Stats>) -> Self {
        self.stats_tx = Some(tx);
        self
    }

    /// 跑到请求通道关闭。
    pub async fn run(mut self, mut rx: mpsc::UnboundedReceiver<Request>) {
        loop {
            self.service();
            let deadline = self.next_deadline();
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

    fn now() -> Instant {
        Instant::now()
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

    fn next_deadline(&self) -> Option<Instant> {
        let mut d: Option<Instant> = None;
        let mut take = |t: Option<Instant>| {
            if let Some(t) = t {
                d = Some(d.map_or(t, |x| x.min(t)));
            }
        };
        if self.dial_task.is_none() && self.client.is_none() {
            take(self.retry_at);
        }
        if self.waiter.is_some() {
            take(self.frame_due);
        }
        if self.live() {
            take(self.store.refresh.next_due_at().map(Instant::from_std));
            if let Some(t) = self.mark_read_last {
                take(Some(t + MARK_READ_INTERVAL));
            }
        }
        d
    }

    /// 每轮循环之后：该拨就拨、该查就查、取帧的等到了就交。
    fn service(&mut self) {
        let now = Self::now();
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
            if let Some((round, tables)) = self.store.refresh.due(now.into_std()) {
                self.stats.refresh_rounds += 1;
                for table in tables {
                    self.view_query(table.query(), QueryPurpose::Summary { round, table });
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
                if self.client.is_none() {
                    self.retry_at = Some(Self::now());
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
            if let Some(c) = &self.client
                && let Err(e) = c.subscribe_nowait(id, scope.to_proto(), 0)
            {
                tracing::debug!(target: "iwork_client", id, error = %e, "订阅时连接已关，重连时一起订");
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
            Internal::QueryFailed { request_id } => {
                if let Some(p) = self.queries.remove(&request_id) {
                    tracing::warn!(target: "iwork_client", request_id, purpose = ?p, "视图查询失败");
                    if matches!(p, QueryPurpose::Summary { .. }) {
                        self.store.refresh.abandon_round();
                    }
                    if matches!(p, QueryPurpose::OlderPage { .. }) {
                        self.want_older = false;
                    }
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
                self.queries.insert(id, purpose);
                let tx = self.internal_tx.clone();
                tokio::spawn(async move {
                    if !matches!(rx.await, Ok(Ok(()))) {
                        let _ = tx.send(Internal::QueryFailed { request_id: id });
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
                    },
                ));
            }
            if ready
                && self.want_tail
                && !tl.at_tail
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
            && !self.in_flight(|p| *p == QueryPurpose::Rules)
        {
            want.push((
                pb::Query {
                    kind: Some(Q::ListRules(pb::ListRules { project_id: None })),
                },
                QueryPurpose::Rules,
            ));
        }
        if self.store.visible.settings {
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
        if let Some(list) = &self.store.conversation_list
            && list.loading
            && !self.in_flight(|p| matches!(p, QueryPurpose::ConversationList { .. }))
        {
            let before = if self.list_append {
                list.items.last().map(|c| c.id.clone())
            } else {
                None
            };
            let append = self.list_append && before.is_some();
            want.push((
                pb::Query {
                    kind: Some(Q::ListConversations(pb::ListConversations {
                        project_id: Some(list.query.project_id.clone()),
                        title_contains: (!list.query.filter.is_empty())
                            .then(|| list.query.filter.clone()),
                        before,
                        limit: PAGE_SIZE,
                    })),
                },
                QueryPurpose::ConversationList {
                    project_id: list.query.project_id.clone(),
                    filter: list.query.filter.clone(),
                    append,
                },
            ));
        }
        for (q, p) in want {
            self.view_query(q, p);
        }
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
                    Retry::Backoff => Some(Self::now() + self.backoff.next_delay()),
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
        let client = Arc::new(client);
        let subs: Vec<(u32, pb::SubscriptionScope)> = self
            .store
            .subscriptions()
            .map(|(id, s)| (id, s.to_proto()))
            .collect();
        for (id, scope) in subs {
            // 一次写出全部范围，同一个 from_seq（T03 客户端契约）。
            if let Err(e) = client.subscribe_nowait(id, scope, from_seq) {
                tracing::debug!(target: "iwork_client", id, error = %e, "订阅没写出去，等连接关闭后重来");
            }
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
        tracing::info!(target: "iwork_client", why, "与后台服务的连接断了");
        self.client = None;
        self.incoming = None;
        self.queries.clear();
        self.reconnect.on_closed(self.store.cursor());
        self.store.connection_closed();
        self.store.set_overloaded(self.reconnect.overloaded());
        self.store.set_connection(Connection::Reconnecting);
        // 立即重拨；之后失败才退避。
        self.retry_at = Some(Self::now());
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
            self.reconnect.on_applied_seq();
            self.store.set_overloaded(self.reconnect.overloaded());
        }
        for f in applied.follow_ups {
            match f {
                FollowUp::AllReady => {
                    self.ever_live = true;
                    self.backoff = Backoff::new();
                    self.store.set_connection(Connection::Live);
                }
                FollowUp::Checkpoint { conversation_id } => {
                    let now = Self::now();
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
