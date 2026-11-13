//! 会话主循环（T09 方案 2.3）：一个 tokio 任务独占状态库。连接来的数据、网页来的操作、定时器进同一个
//! `select!`，顺序处理，没有锁。
//!
//! - 连上 → 一次写出全部范围的 `Subscribe`（同一个 `from_seq`），在飞命令按原 `command_id` 重发；全部
//!   `Subscribed` 应用完才算 Live。
//! - 断了 → 立即重拨，之后按 `Backoff` 退避；在飞视图查询表整张清空，每种用途从状态里重新算「还要再发」。
//! - 网页取帧是一次长等待：没有可交的就挂着，状态变了或到了 16ms 再试。

mod add_project;
mod assistant_edits;
mod attention;
pub mod dial;
mod models;
mod presence;
pub mod reconnect;
mod reminders;
mod start;

use std::collections::{BTreeMap, HashMap};
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
use crate::intent::{self, Action, Effect, Local, Pick};
use crate::store::{
    Connection, FollowUp, Item, PageCursor, QueryPurpose, Row, ScopeKind, Store, SummaryTable,
};
use dial::{DialError, Dialer};
use reconnect::{ReconnectState, Retry};

/// 流式缺口要检查点：每个对话每秒最多一次（方案 2.2）。
const CHECKPOINT_INTERVAL: Duration = Duration::from_secs(1);
/// `MarkRead` 每秒最多一次（方案 2.7）。
const MARK_READ_INTERVAL: Duration = Duration::from_secs(1);
/// 「已重新连接，回到最新」提示显示多久（Q9）。
const BACK_TO_LATEST_FOR: Duration = Duration::from_secs(4);
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
    /// 正在往窗口里拖文件（拖进来为真，拖出去为假）：对话区显示「松开以添加」。
    Dragging(bool),
    /// 文件拖进窗口松开：Rust 侧拿到的绝对路径，附到当前可见的对话。拖动随之结束。
    Dropped(Vec<String>),
    /// 系统选择框的结果（回应 `Effect::PickPaths`）：选中的绝对路径，取消或弹不出来时为空。
    Picked(Vec<String>),
    /// 窗口开着还是关了（macOS 关窗不退进程，点 Dock 图标重新开窗；方案 2.8）。
    WindowOpen(bool),
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
    /// 被叫到前台时要跳到哪（GetOpenTarget，见 reminders 模块）；没问到为 None。
    OpenTarget {
        epoch: u64,
        result: Option<pb::GetOpenTargetResult>,
    },
    /// 添加项目的检查回来了（见 add_project 模块）。错误为 None 表示连接断了，不算失败。
    Inspected {
        epoch: u64,
        token: u64,
        result: Result<pb::InspectProjectPathResult, Option<String>>,
    },
    /// 模型清单的附带查询回来了（见 models 模块）。错误为 None 表示连接断了，不算失败。
    ModelsLoaded {
        epoch: u64,
        runtime_id: String,
        result: Result<pb::ListModelsResult, Option<String>>,
    },
}

struct InFlight {
    command: pb::Command,
    reply: Option<oneshot::Sender<pb::IntentAck>>,
    /// 首页「新建并发出」的一步（session::start）。
    start_step: Option<start::StartStep>,
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
    /// 发出的 ReportPresence 条数。
    pub presence_reports: u64,
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
    /// Dock 角标与跳动（方案 2.8）。
    attention: attention::Attention,
    /// 窗口开着（macOS 关窗不退进程；方案 2.8）。
    window_open: bool,
    /// 这条连接上最后报过的在看哪；连上后为 None（先发一次当前值）。
    presence_sent: Option<presence::Presence>,
    /// 延后要报的在看哪与到期时刻（到来延后报）。
    presence_due: Option<(Instant, presence::Presence)>,
    /// 该问一次 GetOpenTarget 了（窗口获得焦点、启动后第一次连上）。
    open_target_wanted: bool,
    open_target_in_flight: bool,
    /// 处理过的最大 token（只记在内存里）。
    open_target_seen: u64,
    /// 开着的系统选择框：选好以后做什么，以及网页那次操作还没回的受理结果。同一时刻最多一个。
    pick: Option<(Pick, oneshot::Sender<pb::IntentAck>)>,
    stats: Stats,
    stats_tx: Option<tokio::sync::watch::Sender<Stats>>,
    /// 每条连接一个代次：内部任务的结果带着它，断线重连后迟到的旧结果对得上号才处理（审查 1/F-9）。
    conn_epoch: u64,
    /// 视图查询失败后的退避，按用途种类（审查 1/F-5）。
    query_backoff: HashMap<&'static str, (Instant, Backoff)>,
    /// 首页「新建并发出」新对话配上后还没发的步骤（session::start）；None 表示还没排。
    start_steps: Option<std::collections::VecDeque<pb::command::Kind>>,
    /// 在飞的目录检查：(连接代次, token)。换了连接就算不在飞（断线丢了的检查重连后重发）。
    inspect_in_flight: Option<(u64, u64)>,
    /// 添加项目草稿发出的命令（command_id → 草稿操作编号）：回包按编号交回草稿（store::add_project）。
    add_project_ops: HashMap<String, u64>,
    /// 「已重新连接，回到最新」提示到时撤掉的时刻（Q9）。
    back_to_latest_until: Option<Instant>,
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
            attention: attention::Attention::default(),
            window_open: true,
            presence_sent: None,
            presence_due: None,
            open_target_wanted: false,
            open_target_in_flight: false,
            open_target_seen: 0,
            pick: None,
            stats: Stats::default(),
            stats_tx: None,
            conn_epoch: 0,
            query_backoff: HashMap::new(),
            start_steps: None,
            inspect_in_flight: None,
            add_project_ops: HashMap::new(),
            back_to_latest_until: None,
            clock,
        }
    }

    /// 只给录像与预览：按助手 id 换形象（M1 没有选形象的入口）。
    pub fn with_characters(mut self, c: crate::derive::avatar::CharacterOverrides) -> Self {
        self.store.character_overrides = c;
        self
    }

    /// 家目录：界面上的路径把它写成 `~`。
    pub fn with_home_dir(mut self, home: Option<String>) -> Self {
        self.store.home_dir = home;
        self
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
        take(self.back_to_latest_until);
        if self.live() {
            let refresh = self.store.refresh.next_due_at().map(Instant::from_std);
            let blocked = self.backoff_until("summary");
            take(match (refresh, blocked) {
                (Some(r), Some(b)) => Some(r.max(b)),
                (r, None) => r,
                (None, Some(_)) => None,
            });
            take(self.mark_read_due());
            take(self.presence_due.as_ref().map(|d| d.0).filter(|t| *t > now));
            // 退避到期只表示「可以再发」，要不要发由 service 用同一个 now 判过了；没发就是用不着，
            // 只登记还没到期的，免得过期的退避项让主循环空转。
            for (until, _) in self.query_backoff.values() {
                take(Some(*until).filter(|t| *t > now));
            }
        }
        d
    }

    /// 某种用途的查询失败：退避后再发，界面挂「查询重试中」（视图查询与附带查询共用）。
    fn query_failed(&mut self, kind: &'static str) {
        let now = self.now();
        let entry = self
            .query_backoff
            .entry(kind)
            .or_insert_with(|| (now, Backoff::new()));
        entry.0 = now + entry.1.next_delay();
        self.store.set_query_failing(true);
    }

    /// 某种用途的查询成功：清掉它的退避；没有别的用途还在退避，提示一并清掉。
    fn query_succeeded(&mut self, kind: &str) {
        if self.query_backoff.remove(kind).is_some() && self.query_backoff.is_empty() {
            self.store.set_query_failing(false);
        }
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
            // 打开的对话已经有锚点窗口：这一轮按它的中心一起重取（方案 2.9）
            let anchors = self
                .store
                .open
                .as_ref()
                .filter(|t| t.anchors.window.is_some())
                .map(|t| (t.conversation_id.clone(), t.anchors.center));
            if !self.backed_off("summary", now)
                && let Some((round, tables)) =
                    self.store
                        .refresh
                        .due_with(now.into_std(), with_list, anchors.is_some())
            {
                self.stats.refresh_rounds += 1;
                for table in tables {
                    match (table.query(), table, &anchors) {
                        (Some(q), _, _) => {
                            self.view_query(q, QueryPurpose::Summary { round, table });
                        }
                        (None, SummaryTable::Anchors, Some((cid, center))) => {
                            let (q, p) = anchors_query(cid, *center, Some(round));
                            self.view_query(q, p);
                        }
                        (None, _, _) => self.send_list_query(Some(round)),
                    }
                }
            }
            self.wanted_queries();
            self.serve_presence(now);
            self.ask_open_target();
            self.want_models(now);
            self.advance_start();
            self.drive_assistant_edits();
            self.want_inspect();
            self.mark_read(now);
        }
        if self.store.take_back_to_latest_started() {
            self.back_to_latest_until = Some(now + BACK_TO_LATEST_FOR);
        }
        if self.back_to_latest_until.is_some_and(|t| t <= now) {
            self.back_to_latest_until = None;
            self.store.clear_back_to_latest();
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
            ShellEvent::Focused(f) => {
                self.store.set_focused(f);
                // 被系统通知叫到前台：问一次要跳到哪（方案 2.8）
                if f {
                    self.open_target_wanted = true;
                }
            }
            ShellEvent::WindowOpen(open) => {
                self.window_open = open;
                if !open {
                    self.store.set_focused(false);
                }
            }
            ShellEvent::Dragging(on) => self.store.set_dragging(on),
            ShellEvent::Picked(paths) => self.on_picked(paths),
            ShellEvent::Dropped(paths) => {
                self.store.set_dragging(false);
                // 恰好一个文件夹：添加项目（方案 15.2）；别的照旧当附件
                if self.dropped_folder(&paths) {
                    return;
                }
                let Some(cid) = self.store.visible.conversation_id.clone() else {
                    return;
                };
                let (tx, rx) = oneshot::channel();
                self.on_attach(
                    attach::Source::Paths {
                        conversation_id: cid,
                        paths,
                    },
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
            ShellEvent::QuitRequested => self.store.set_quit_requested(true),
        }
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
            Action::AddProject(act) => self.add_project_act(act, tx),
            Action::AssistantField {
                assistant_id,
                field,
            } => {
                let error = self.assistant_field(&assistant_id, field);
                let _ = tx.send(pb::IntentAck { error });
            }
            Action::Start { project_id, text } => {
                let error = self.start(project_id, text);
                let _ = tx.send(pb::IntentAck { error });
            }
            Action::Local(l) => {
                self.on_local(l);
                let _ = tx.send(ok);
            }
            Action::Command(kind) => {
                self.send_command(kind, Some(tx));
            }
            Action::CreateAssistant(c) => {
                if self.store.creating_assistant() {
                    let _ = tx.send(pb::IntentAck {
                        error: Some(pb::IntentError {
                            code: "not_ready".into(),
                            message: "正在创建，等上一次有结果".into(),
                            retryable: false,
                        }),
                    });
                } else if self
                    .send_command(pb::command::Kind::CreateAssistant(c), Some(tx))
                    .is_some()
                {
                    self.store.set_creating_assistant(true);
                }
            }
            Action::AttachPasted(x) => self.on_attach(attach::Source::Pasted(x), tx),
            Action::Pick(p) => self.open_picker(p, tx),
        }
    }

    /// 请壳弹系统选择框。网页那次操作的受理结果留到选完、命令受理后再回（[`Self::on_picked`]）。
    fn open_picker(&mut self, pick: Pick, tx: oneshot::Sender<pb::IntentAck>) {
        let refuse = |code: &str, message: &str| pb::IntentError {
            code: code.into(),
            message: message.into(),
            retryable: false,
        };
        let error = if self.pick.is_some() {
            refuse("picker_open", "已经开着一个选择框")
        } else if !self.live() {
            // 选完了也发不出去：先别弹框
            offline_error()
        } else if self.effects.send(Effect::PickPaths(pick.kind())).is_err() {
            refuse("no_shell", "弹不出选择框")
        } else {
            self.pick = Some((pick, tx));
            return;
        };
        let _ = tx.send(pb::IntentAck { error: Some(error) });
    }

    /// 选择框的结果：按打开它时的用途发附件或重新定位；取消就只回受理结果。
    fn on_picked(&mut self, paths: Vec<String>) {
        let Some((pick, tx)) = self.pick.take() else {
            tracing::warn!(target: "iwork_client", "收到选择结果，但没有开着的选择框，丢掉");
            return;
        };
        let Some(first) = paths.first().cloned() else {
            let _ = tx.send(pb::IntentAck { error: None });
            return;
        };
        match pick {
            Pick::Attachments { conversation_id } => self.on_attach(
                attach::Source::Paths {
                    conversation_id,
                    paths,
                },
                tx,
            ),
            // 选文件夹的框只给一个路径
            Pick::ProjectLocation { project_id } => {
                self.send_command(
                    pb::command::Kind::RelocateProject(pb::RelocateProject {
                        project_id,
                        new_path: first,
                    }),
                    Some(tx),
                );
            }
            Pick::ProjectFolder => {
                self.add_project_picked(add_project::AddProjectPick::Candidate, first);
                let _ = tx.send(pb::IntentAck { error: None });
            }
            Pick::ProjectsParent => {
                self.add_project_picked(add_project::AddProjectPick::Parent, first);
                let _ = tx.send(pb::IntentAck { error: None });
            }
            Pick::ProjectsRoot => {
                self.send_command(
                    pb::command::Kind::SetProjectsRoot(pb::SetProjectsRoot { path: first }),
                    Some(tx),
                );
            }
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
                by_user,
            } => {
                // 开发构建里把每次可见行上报打到 stderr（`pnpm -C client dev:fake` 时看「用户滚动」有没有报出 by_user；
                // 组 2b 第 6 轮 UI 审查）。发布构建没有这一行
                #[cfg(feature = "dev")]
                eprintln!(
                    "[dev] SetVisibleRows conversation={conversation_id} first={first} last={last} by_user={by_user}"
                );
                self.store
                    .set_window(&conversation_id, first, last, by_user);
            }
            Local::LoadOlder(_) => self.want_older = true,
            Local::JumpToLatest(cid) => {
                // 跳到最新：等着的锚点跳转作废（方案 2.9 第 3 条）
                if let Some(t) = self
                    .store
                    .open
                    .as_mut()
                    .filter(|t| t.conversation_id == cid)
                {
                    t.anchors.pending_jump = None;
                }
                self.want_tail = true;
            }
            Local::AnchorBrowse {
                conversation_id,
                key,
            } => {
                self.store.anchor_browse(&conversation_id, &key);
            }
            Local::JumpToRow {
                conversation_id,
                row_key,
            } => {
                self.store.jump_to_row(&conversation_id, &row_key);
            }
            Local::AnchorListInput(conversation_id) => {
                self.store.anchor_list_input(&conversation_id);
            }
            Local::AnchorBrowseEdge {
                conversation_id,
                latest,
            } => {
                self.store.anchor_browse_edge(&conversation_id, latest);
            }
            Local::JumpToAnchor {
                conversation_id,
                key,
            } => {
                self.store.jump_to_anchor(&conversation_id, &key);
            }
            Local::JumpRelative {
                conversation_id,
                delta,
            } => self.store.jump_relative(&conversation_id, delta),
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
            Local::HomeModel(m) => self.store.set_home_choice(|h| h.model = Some(m)),
            Local::HomeMode(m) => self.store.set_home_choice(|h| h.mode = Some(m)),
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

    /// 发一条命令，返回它的 command_id（断线时不发，回 None）。
    fn send_command(
        &mut self,
        kind: pb::command::Kind,
        reply: Option<oneshot::Sender<pb::IntentAck>>,
    ) -> Option<String> {
        if !self.live() {
            // 断线期间不接新命令（D8：发送禁用，草稿留在网页本地）。
            if let Some(tx) = reply {
                let _ = tx.send(pb::IntentAck {
                    error: Some(offline_error()),
                });
            }
            return None;
        }
        let command_id = ulid::Ulid::generate().to_string();
        let command = pb::Command {
            command_id: command_id.clone(),
            kind: Some(kind),
        };
        self.outbox.insert(
            command_id.clone(),
            InFlight {
                command: command.clone(),
                reply,
                start_step: None,
            },
        );
        self.dispatch(command);
        Some(command_id)
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

    fn on_attach(&mut self, source: attach::Source, tx: oneshot::Sender<pb::IntentAck>) {
        let Some(client) = self.client.clone().filter(|_| self.live()) else {
            let _ = tx.send(pb::IntentAck {
                error: Some(offline_error()),
            });
            return;
        };
        // 附件按批发：不进在飞命令表（断线就整批作废，用户重新添加）。
        let attach::Batch { chunks, commands } = match attach::batch(source) {
            Ok(b) => b,
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
                let accepted = match &result {
                    Ok(r) => match &r.body {
                        Some(pb::response::Body::Accepted(a)) => Some((a.first_seq, a.last_seq)),
                        _ => None,
                    },
                    Err(_) => None,
                };
                let ack = match result {
                    Err(ClientError::Closed) => {
                        // 连接断了：留在在飞表里，重连后按原 command_id 重发（幂等）。
                        return;
                    }
                    Err(e) => Some(client_error(&e)),
                    Ok(r) => response_error(&r),
                };
                if let Some(f) = self.outbox.remove(&command_id) {
                    if let Some(step) = f.start_step {
                        self.on_start_step(step, accepted, ack.clone());
                    } else if let (
                        Some(pb::command::Kind::CreateConversation(_)),
                        Some((first, last)),
                    ) = (&f.command.kind, accepted)
                    {
                        // 新建对话：事件里带对话 id，配上受理的 seq 范围就打开它（store::created）。
                        self.store.create_accepted(
                            first,
                            last,
                            crate::store::created::Purpose::Open,
                        );
                    }
                    if let Some(pb::command::Kind::UpdateAssistant(u)) = &f.command.kind {
                        self.assistant_update_done(&u.assistant_id, ack.as_ref());
                    }
                    if matches!(f.command.kind, Some(pb::command::Kind::CreateAssistant(_))) {
                        self.store.set_creating_assistant(false);
                    }
                    if let Some(op) = self.add_project_ops.remove(&command_id) {
                        self.add_project_done(op, &f.command.kind, accepted, ack.as_ref());
                    }
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
            Internal::ModelsLoaded {
                epoch,
                runtime_id,
                result,
            } => self.on_models(epoch, &runtime_id, result),
            Internal::Inspected {
                epoch,
                token,
                result,
            } => self.on_inspected(epoch, token, result),
            Internal::OpenTarget { epoch, result } => self.on_open_target(epoch, result),
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
                            | QueryPurpose::Anchors { round: Some(_), .. }
                    ) {
                        self.store.refresh.abandon_round();
                    }
                    if matches!(p, QueryPurpose::OlderPage { .. }) {
                        self.want_older = false;
                    }
                    // 发出时清掉的过期标记放回去：退避过后再查（审查 g3 1/F-1）
                    if p == QueryPurpose::Rules {
                        self.store.rules_stale = true;
                    }
                    self.query_failed(purpose_kind(&p));
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
            // 和 store 判断「翻页结果合不合并」用同一个条件（方案 2.2 第 4 条、2.3）
            let window_at_top = self.store.window_touches_oldest();
            if ready
                && tl.has_older
                && (self.want_older || window_at_top)
                && let Some(before) = tl.older_cursor()
                && !self.backed_off("older", now)
                && !self.in_flight(|p| matches!(p, QueryPurpose::OlderPage { .. }))
            {
                want.push((
                    page_query(&cid, Side::Older, &before),
                    QueryPurpose::OlderPage {
                        conversation_id: cid.clone(),
                        before,
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
                        conversation_id: cid.clone(),
                    },
                ));
            }
            if tl.at_tail {
                self.want_tail = false;
            }
            // 往下翻到这一段的末尾、而后面还有（跳到旧处之后）：往后翻一页，接到最新就回到 at_tail（G11）
            let window_at_bottom = self.store.window_touches_newest();
            if ready
                && !tl.at_tail
                && window_at_bottom
                && !self.want_tail
                && let Some(after) = tl.newer_cursor()
                && !self.backed_off("newer", now)
                && !self.in_flight(|p| matches!(p, QueryPurpose::NewerPage { .. }))
            {
                want.push((
                    page_query(&cid, Side::Newer, &after),
                    QueryPurpose::NewerPage {
                        conversation_id: cid.clone(),
                        after,
                    },
                ));
            }
            // 跳到没加载的锚点：取以它为中心的一段（有了底才取：首份快照没到时先等底，见锚点模块头的表）
            if ready
                && tl.anchors.base_loaded
                && let Some(j) = &tl.anchors.pending_jump
                && !self.backed_off("around", now)
                && !self.in_flight(|p| matches!(p, QueryPurpose::AroundPage { seq, .. } if *seq == j.anchor.created_seq))
            {
                let seq = j.anchor.created_seq;
                want.push((
                    pb::Query {
                        kind: Some(Q::GetConversation(pb::GetConversation {
                            after_message_id: None,
                            around_seq: Some(seq),
                            before_seq: None,
                            after_seq: None,
                            conversation_id: cid.clone(),
                            before_message_id: None,
                            limit: crate::store::anchors::JUMP_PAGE,
                        })),
                    },
                    QueryPurpose::AroundPage {
                        conversation_id: cid.clone(),
                        seq,
                    },
                ));
            }
            // 锚点窗口：还没取过、或要换中心时单独取；过时的重取进摘要重查轮（service 里）
            if ready
                && let Some(center) = tl.anchors.wanted_center()
                && !self.backed_off("anchors", now)
                && !self.in_flight(
                    |p| matches!(p, QueryPurpose::Anchors { center: c, .. } if *c == center),
                )
            {
                want.push(anchors_query(&cid, center, None));
            }
        }
        if self.store.visible.settings
            && (self.store.rules.is_none() || self.store.rules_stale)
            && !self.backed_off("rules", now)
            && !self.in_flight(|p| *p == QueryPurpose::Rules)
        {
            // 发出即清掉过期：在飞期间又来了 rule 事件会再标过期，结果回来后再查一次
            self.store.rules_stale = false;
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
        // 在看哪是连接级状态：断了就算没人在看，重新连上后发一次当前值；不重发旧值
        self.presence_sent = None;
        self.presence_due = None;
        self.open_target_in_flight = false;
        self.incoming = None;
        // 断线丢掉的规则重查：过期标记放回去，重连后再查（重连没走快照时不会有别的东西替它标）
        if self.queries.values().any(|p| *p == QueryPurpose::Rules) {
            self.store.rules_stale = true;
        }
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
        let applied = match item {
            IncomingItem::Event(e) => self.store.apply(Item::Event(e)),
            IncomingItem::ViewResult { request_id, result } => {
                let Some(purpose) = self.queries.remove(&request_id) else {
                    tracing::warn!(target: "iwork_client", request_id, "查询结果没有登记的用途，丢掉");
                    return;
                };
                if matches!(purpose, QueryPurpose::OlderPage { .. }) {
                    self.want_older = false;
                }
                self.query_succeeded(purpose_kind(&purpose));
                self.store.apply(Item::ViewResult { purpose, result })
            }
            IncomingItem::Subscribed { subscribed, .. } => {
                self.store.apply(Item::Subscribed(subscribed))
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
        // 待处理表整份换过（摘要重查的那一条、快照）：更新 Dock 角标与跳动
        if self.store.take_pending_replaced() {
            self.pending_replaced();
        }
        if applied.had_seq {
            self.backoff = Backoff::new();
            self.reconnect.on_applied_seq();
            self.store.set_overloaded(self.reconnect.overloaded());
        }
        for f in applied.follow_ups {
            match f {
                FollowUp::AllReady => {
                    // 启动后第一次连上：问一次是不是被系统通知叫起来的（方案 2.8）
                    if !self.ever_live {
                        self.open_target_wanted = true;
                    }
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

    // ---------- 测试与基准用的只读口 ----------

    pub fn store(&self) -> &Store {
        &self.store
    }

    pub fn card_row(&self, id: &str) -> Option<&Row> {
        self.store.open.as_ref().and_then(|t| t.row(id))
    }
}

#[derive(Clone, Copy)]
enum Side {
    Older,
    Newer,
}

/// 往前 / 往后翻一页的查询（方案 2.2 第 4 条）：游标是消息 id 时按消息数取一页（[`PAGE_SIZE`] 条）；是边界 seq 时
/// 用行级游标按行数取（[`crate::store::ROW_PAGE`] 行，合并时挑的也是这么多行）。
fn page_query(cid: &str, side: Side, cursor: &PageCursor) -> pb::Query {
    let mut g = pb::GetConversation {
        conversation_id: cid.to_owned(),
        ..Default::default()
    };
    match (side, cursor) {
        (Side::Older, PageCursor::Message(id)) => g.before_message_id = Some(id.clone()),
        (Side::Newer, PageCursor::Message(id)) => g.after_message_id = Some(id.clone()),
        (Side::Older, PageCursor::Seq(seq)) => g.before_seq = Some(*seq),
        (Side::Newer, PageCursor::Seq(seq)) => g.after_seq = Some(*seq),
    }
    g.limit = match cursor {
        PageCursor::Message(_) => PAGE_SIZE,
        PageCursor::Seq(_) => crate::store::ROW_PAGE,
    };
    pb::Query {
        kind: Some(pb::query::Kind::GetConversation(g)),
    }
}

fn latest_page(cid: &str) -> pb::Query {
    pb::Query {
        kind: Some(pb::query::Kind::GetConversation(pb::GetConversation {
            after_message_id: None,
            around_seq: None,
            before_seq: None,
            after_seq: None,
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
/// 断线期间不接新命令（D8：发送禁用，草稿留在网页本地）。
fn offline_error() -> pb::IntentError {
    pb::IntentError {
        code: "offline".into(),
        message: crate::derive::text::OFFLINE_TRY_LATER.into(),
        retryable: true,
    }
}

/// 锚点窗口查询：以 `center` 为中心前后各取 `ANCHOR_SPAN` 条。
fn anchors_query(cid: &str, center: u64, round: Option<u64>) -> (pb::Query, QueryPurpose) {
    use crate::store::anchors::ANCHOR_SPAN;
    (
        pb::Query {
            kind: Some(pb::query::Kind::ListAnchors(pb::ListAnchors {
                conversation_id: cid.to_owned(),
                around_seq: center,
                before: ANCHOR_SPAN,
                after: ANCHOR_SPAN,
            })),
        },
        QueryPurpose::Anchors {
            conversation_id: cid.to_owned(),
            center,
            round,
        },
    )
}

fn purpose_kind(p: &QueryPurpose) -> &'static str {
    match p {
        QueryPurpose::Summary { .. } => "summary",
        QueryPurpose::LatestPage { .. } => "latest",
        QueryPurpose::OlderPage { .. } => "older",
        QueryPurpose::Rules => "rules",
        QueryPurpose::LocalConfig { .. } => "local_config",
        QueryPurpose::ConversationList { .. } => "list",
        QueryPurpose::Anchors { .. } => "anchors",
        QueryPurpose::NewerPage { .. } => "newer",
        QueryPurpose::AroundPage { .. } => "around",
    }
}
