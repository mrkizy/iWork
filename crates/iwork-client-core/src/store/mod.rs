//! 客户端状态库（T09 方案 2.2）。由会话循环一个任务独占，没有锁。
//!
//! 连接来的数据只经 [`Store::apply`] 写入，吃的是 `iwork_ipc` `Incoming` 给出的全序；网页声明的视图状态经
//! `set_*` 写入。两者都只往 [`DirtySet`] 记「哪块脏了」，帧在网页来取时才由 `derive` 从当前状态算（方案 2.4）。

pub mod created;
pub mod home;
pub mod models;
pub mod summary;
pub mod timeline;

use std::collections::{BTreeMap, BTreeSet};

use iwork_proto::pb;

pub use summary::{Refresh, Summary, SummaryTable};
pub use timeline::{PageMode, Row, RowKey, Timeline, TimelineChange};

use timeline::{Delta, DeltaOutcome, DeltaTarget};

/// 订阅范围的种类（与 `pb::SubscriptionScope` 一一对应，会话层登记）。
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub enum ScopeKind {
    Sidebar,
    Conversation(String),
    Pending,
    Quota,
    RuntimeStatus,
    Settings,
}

impl ScopeKind {
    pub fn to_proto(&self) -> pb::SubscriptionScope {
        use pb::subscription_scope::Kind as K;
        let kind = match self {
            ScopeKind::Sidebar => K::Sidebar(pb::SidebarScope {}),
            ScopeKind::Conversation(id) => K::Conversation(pb::ConversationScope {
                conversation_id: id.clone(),
            }),
            ScopeKind::Pending => K::Pending(pb::PendingScope {}),
            ScopeKind::Quota => K::Quota(pb::QuotaScope {}),
            ScopeKind::RuntimeStatus => K::RuntimeStatus(pb::RuntimeStatusScope {}),
            ScopeKind::Settings => K::Settings(pb::SettingsScope {}),
        };
        pb::SubscriptionScope { kind: Some(kind) }
    }

    /// 命中它的持久事件让摘要失效（方案 2.2 第 1 条）。
    fn feeds_summary(&self) -> bool {
        matches!(self, ScopeKind::Sidebar | ScopeKind::Pending)
    }
}

/// 视图的快照有没有到（方案 R2）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ViewState {
    Loading,
    Ready,
}

#[derive(Debug, Clone)]
struct Sub {
    scope: ScopeKind,
    state: ViewState,
}

/// 一条会写视图的查询是为什么发的（在飞视图查询登记表的值，方案 2.3）。结果按它进状态库。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum QueryPurpose {
    Summary {
        round: u64,
        table: SummaryTable,
    },
    /// 最新页：回到最新、流式缺口的检查点。
    LatestPage {
        conversation_id: String,
    },
    /// 往前翻页；`before` 是发出时最旧一条消息的 id。结果回来时它已不是最旧的一行（中间重建过），
    /// 结果就不接上去，免得时间线中间留一段空洞。
    OlderPage {
        conversation_id: String,
        before: String,
    },
    Rules,
    LocalConfig {
        runtime_id: String,
    },
    /// 「全部对话」列表页的一批；`append` 为假时整份替换。`round` 有值表示它是那一轮摘要重查的一部分
    /// （方案 2.2 第 2 条：列表页开着时摘要轮多带一条）。
    ConversationList {
        project_id: String,
        filter: String,
        append: bool,
        round: Option<u64>,
    },
}

/// 从 `Incoming` 来、会写状态的一项（`Closed` 由会话层处理）。
#[derive(Debug, Clone)]
pub enum Item {
    Event(Box<pb::Event>),
    Subscribed(pb::Subscribed),
    ViewResult {
        purpose: QueryPurpose,
        result: Box<pb::QueryResult>,
    },
}

/// 应用一项之后会话层要做的事。
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Applied {
    /// 应用了带 seq 的项（事件、快照、查询结果）：会话层据此清零 `same_cursor_closes`。
    pub had_seq: bool,
    pub follow_ups: Vec<FollowUp>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FollowUp {
    /// 流式缺口：要一次最新页查询（会话层按每个对话每秒最多一次节流）。
    Checkpoint { conversation_id: String },
    /// 全部订阅都 Ready 了：连接算 Live。
    AllReady,
}

/// 视图的种类（帧引擎按它算 patch）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum ViewKey {
    Sidebar,
    Pending,
    Settings,
    Assistant,
    AddProject,
    Connection,
    ConversationList,
    Conversation,
    Navigate,
    Home,
}

impl ViewKey {
    /// 「全部视图」不含跳转：跳转只在请求时发一次，attach、SetVisible 不重发（审查 1/F-10）。
    pub const ALL: [ViewKey; 9] = [
        ViewKey::Sidebar,
        ViewKey::Pending,
        ViewKey::Settings,
        ViewKey::Assistant,
        ViewKey::AddProject,
        ViewKey::Connection,
        ViewKey::ConversationList,
        ViewKey::Conversation,
        ViewKey::Home,
    ];
}

/// 脏集合：视图种类 + 对话里具体哪些行。
#[derive(Debug, Clone, Default, PartialEq)]
pub struct DirtySet {
    pub views: BTreeSet<ViewKey>,
    pub conversation: TimelineChange,
}

impl DirtySet {
    pub fn is_empty(&self) -> bool {
        self.views.is_empty() && self.conversation.is_empty()
    }
    pub fn mark(&mut self, v: ViewKey) {
        self.views.insert(v);
    }
    pub fn mark_all(&mut self) {
        self.views.extend(ViewKey::ALL);
        self.conversation.reset = true;
    }
    fn conversation(&mut self, c: TimelineChange) {
        if !c.is_empty() {
            self.views.insert(ViewKey::Conversation);
            self.conversation.merge(c);
        }
    }
}

/// 网页声明的可见视图（`SetVisible` 整份替换）。
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Visible {
    pub sidebar: bool,
    pub conversation_id: Option<String>,
    pub pending: bool,
    pub settings: bool,
    pub assistant_id: Option<String>,
    pub add_project: bool,
    pub conversation_list: Option<ListQuery>,
    /// 首页：路由里带的项目（空串表示由 Rust 挑）。
    pub home: Option<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ListQuery {
    pub project_id: String,
    pub filter: String,
}

/// 「全部对话」列表页的数据。
#[derive(Debug, Clone, Default, PartialEq)]
pub struct ConversationList {
    pub query: ListQuery,
    pub items: Vec<pb::Conversation>,
    pub has_more: bool,
    pub loading: bool,
}

/// 连接状态（会话层写，derive::connection 画）。
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub enum Connection {
    #[default]
    Connecting,
    Live,
    Reconnecting,
    /// 升级窗口的中性提示：文字与出口取 `WaitPrompt`。
    Waiting {
        title: &'static str,
        exits: Vec<&'static str>,
    },
    Diagnostics(Diagnostics),
    Environment(String),
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Diagnostics {
    pub reason: String,
    pub lines: Vec<(String, String)>,
    pub newer_schema: bool,
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct CloneJob {
    pub progress: pb::CloneProgress,
    pub error: Option<String>,
}

#[derive(Debug, Default)]
pub struct Store {
    cursor: u64,
    pub summary: Summary,
    pub refresh: Refresh,
    pub runtimes: BTreeMap<String, pb::RuntimeStatus>,
    pub quota: BTreeMap<String, pb::RuntimeQuota>,
    pub settings: Option<pb::GlobalSettings>,
    pub rules: Option<Vec<pb::Rule>>,
    pub local_config: BTreeMap<String, Vec<pb::LocalConfigItem>>,
    pub clone_job: Option<CloneJob>,
    pub open: Option<Timeline>,
    pub conversation_list: Option<ConversationList>,
    pub visible: Visible,
    /// 网页声明的行窗口（按 ordinal，闭区间）。
    pub window: Option<(i64, i64)>,
    pub focused: bool,
    pub connection: Connection,
    pub overloaded: bool,
    /// 菜单点了「完全退出 iWork」，等网页的确认框。
    pub quit_requested: bool,
    /// 上一次添加附件被拒的原因（拖入时没有回包可带，放在输入框上方）。
    pub attach_error: Option<String>,
    /// 最近一次跳转请求（点击系统通知、新建对话后）。
    pub navigate: Option<pb::NavigatePatch>,
    /// 首页：选过的模型与档位、正在进行的「新建并发出」（见 home 模块）。
    pub home: home::Home,
    /// 本客户端新建的对话：建好就打开（见 created 模块）。
    pub created: created::CreatedWatch,
    /// 各运行时的模型清单（ListModels 附带查询的结果，见 models 模块）。
    pub models: models::ModelCatalog,
    /// 「已重新连接，回到最新」提示挂在哪个对话上（Q9）；会话层到时撤掉。
    pub back_to_latest: Option<String>,
    /// 这一轮刚挂上提示：会话层取走后开始计时。
    back_to_latest_started: bool,
    subs: BTreeMap<u32, Sub>,
    /// 这次重连以来由快照换过的摘要表（审查 1/F-8）。
    snapshot_tables: BTreeSet<SummaryTable>,
    /// 时间线行数上限。
    timeline_cap: usize,
    /// 有视图查询失败、在退避中：连接条上显示一行（审查 1/F-5）。
    pub query_failing: bool,
    pub dirty: DirtySet,
}

impl Store {
    pub fn new() -> Self {
        Self::with_timeline_cap(timeline::TIMELINE_CAP)
    }

    pub fn with_timeline_cap(cap: usize) -> Self {
        Self {
            timeline_cap: cap,
            ..Self::default()
        }
    }

    pub fn set_query_failing(&mut self, on: bool) {
        if self.query_failing != on {
            self.query_failing = on;
            self.dirty.mark(ViewKey::Connection);
        }
    }

    pub fn cursor(&self) -> u64 {
        self.cursor
    }

    // ---------- 订阅登记（会话层） ----------

    pub fn register_subscription(&mut self, id: u32, scope: ScopeKind) {
        self.subs.insert(
            id,
            Sub {
                scope,
                state: ViewState::Loading,
            },
        );
    }

    pub fn unregister_subscription(&mut self, id: u32) {
        self.subs.remove(&id);
    }

    pub fn subscriptions(&self) -> impl Iterator<Item = (u32, &ScopeKind)> {
        self.subs.iter().map(|(id, s)| (*id, &s.scope))
    }

    pub fn subscription_of(&self, scope: &ScopeKind) -> Option<u32> {
        self.subs
            .iter()
            .find(|(_, s)| &s.scope == scope)
            .map(|(id, _)| *id)
    }

    /// 有视图还在 `Loading`：重连必须拿快照（方案 R2）。
    pub fn any_loading(&self) -> bool {
        self.subs.values().any(|s| s.state == ViewState::Loading)
    }

    pub fn view_state(&self, id: u32) -> Option<ViewState> {
        self.subs.get(&id).map(|s| s.state)
    }

    /// 重连：全部订阅重新写出，回到 `Loading`，直到各自的 `Subscribed` 应用完。
    pub fn resubscribing(&mut self) {
        self.snapshot_tables.clear();
        for s in self.subs.values_mut() {
            s.state = ViewState::Loading;
        }
    }

    /// 连接关闭：在飞的摘要重查作废（标记不动，方案 2.3 在飞查询表）。
    pub fn connection_closed(&mut self) {
        self.refresh.abandon_round();
        // 列表页的 loading 留着：它就是「还要再发」的记号，重新 Live 后照样发（审查 1/F-2）。
        self.snapshot_tables.clear();
        self.models.connection_closed();
    }

    /// 模型清单取到了：输入框的菜单要重算。
    pub fn set_models(&mut self, runtime_id: &str, result: pb::ListModelsResult) {
        self.models.loaded(runtime_id, result);
        self.mark_header();
    }

    // ---------- 本地视图状态 ----------

    pub fn set_visible(&mut self, v: Visible) {
        if v.conversation_id != self.visible.conversation_id {
            let cap = self.timeline_cap;
            self.open = v
                .conversation_id
                .as_deref()
                .map(|id| Timeline::with_cap(id, cap));
            self.window = None;
            self.dirty.conversation(TimelineChange {
                reset: true,
                ..Default::default()
            });
        }
        if v.conversation_list != self.visible.conversation_list {
            self.conversation_list = v.conversation_list.clone().map(|query| ConversationList {
                query,
                loading: true,
                ..Default::default()
            });
        }
        self.visible = v;
        // 新可见的视图要整份发：简单起见全部记脏，不可见的发帧时丢掉。
        self.dirty.views.extend(ViewKey::ALL);
    }

    pub fn set_window(&mut self, conversation_id: &str, first: i64, last: i64) {
        if self
            .open
            .as_ref()
            .is_some_and(|t| t.conversation_id == conversation_id)
        {
            self.window = Some((first.min(last), first.max(last)));
            self.dirty.mark(ViewKey::Conversation);
        }
    }

    pub fn set_focused(&mut self, focused: bool) {
        self.focused = focused;
    }

    pub fn set_connection(&mut self, c: Connection) {
        if self.connection != c {
            self.connection = c;
            self.dirty.mark(ViewKey::Connection);
            // 「会发命令的按钮」随连接状态变：输入框在标题栏一带，卡片按钮在行里（derive::row 的 live）。
            self.mark_header();
            self.mark_rows(|r| matches!(r, Row::Approval(_) | Row::UserAction(_) | Row::Resume(_)));
        }
    }

    pub fn set_overloaded(&mut self, on: bool) {
        if self.overloaded != on {
            self.overloaded = on;
            self.dirty.mark(ViewKey::Connection);
        }
    }

    pub fn set_rules_loading(&mut self) {
        self.dirty.mark(ViewKey::Settings);
    }

    pub fn set_list_loading(&mut self) {
        if let Some(l) = self.conversation_list.as_mut() {
            l.loading = true;
            self.dirty.mark(ViewKey::ConversationList);
        }
    }

    pub fn set_quit_requested(&mut self, on: bool) {
        self.quit_requested = on;
        self.dirty.mark(ViewKey::Connection);
    }

    pub fn set_attach_error(&mut self, e: Option<String>) {
        if self.attach_error != e {
            self.attach_error = e;
            self.mark_header();
        }
    }

    pub fn request_navigate(&mut self, conversation_id: String, row_key: String) {
        self.navigate_to(conversation_id, row_key, None);
    }

    fn navigate_to(&mut self, conversation_id: String, row_key: String, started: Option<String>) {
        let seq = self.navigate.as_ref().map_or(1, |n| n.seq + 1);
        self.navigate = Some(pb::NavigatePatch {
            seq,
            conversation_id,
            row_key,
            started_project_id: started,
        });
        self.dirty.mark(ViewKey::Navigate);
    }

    /// 首页的选择（模型、档位）改了。
    pub fn set_home_choice(&mut self, f: impl FnOnce(&mut home::Home)) {
        f(&mut self.home);
        self.dirty.mark(ViewKey::Home);
    }

    /// 首页开始「新建并发出」。
    pub fn start_begin(&mut self, project_id: String, text: String) {
        self.home.begin(project_id, text);
        self.dirty.mark(ViewKey::Home);
    }

    /// 首条消息受理了：跳过去，网页据 started_project_id 清掉首页草稿。
    pub fn start_finished(&mut self) {
        if let Some(s) = self.home.finish()
            && let Some(id) = s.conversation_id
        {
            self.navigate_to(id, String::new(), Some(s.project_id));
        }
        self.dirty.mark(ViewKey::Home);
    }

    pub fn start_failed(&mut self, why: home::StartError) {
        self.home.fail(why);
        self.dirty.mark(ViewKey::Home);
    }

    /// 新建的对话配上了受理范围：侧栏「新对话」直接打开；首页的记下对话，等会话层发首条消息。
    fn created_matched(&mut self, conversation_id: String, purpose: created::Purpose) {
        match purpose {
            created::Purpose::Open => self.request_navigate(conversation_id, String::new()),
            created::Purpose::Start => {
                if let Some(s) = self.home.start.as_mut() {
                    s.conversation_id = Some(conversation_id);
                }
            }
        }
    }

    /// 刚挂上「回到最新」提示：返回真一次，会话层据此开始计时。
    pub fn take_back_to_latest_started(&mut self) -> bool {
        std::mem::take(&mut self.back_to_latest_started)
    }

    pub fn clear_back_to_latest(&mut self) {
        if self.back_to_latest.take().is_some() {
            self.mark_header();
        }
    }

    /// 本客户端的 CreateConversation 受理了（seq 范围来自 CommandAccepted）。
    pub fn create_accepted(&mut self, first: u64, last: u64, purpose: created::Purpose) {
        if let Some((id, purpose)) = self.created.accepted(first, last, purpose) {
            self.created_matched(id, purpose);
        }
    }

    pub fn take_dirty(&mut self) -> DirtySet {
        std::mem::take(&mut self.dirty)
    }

    // ---------- 唯一写入口 ----------

    pub fn apply(&mut self, item: Item) -> Applied {
        let mut applied = Applied::default();
        match item {
            Item::Event(e) => self.apply_event(*e, &mut applied),
            Item::Subscribed(s) => self.apply_subscribed(s, &mut applied),
            Item::ViewResult { purpose, result } => {
                applied.had_seq = true;
                self.cursor = self.cursor.max(result.seq);
                self.apply_result(Some(&purpose), *result);
            }
        }
        applied
    }

    fn apply_event(&mut self, e: pb::Event, applied: &mut Applied) {
        let hit: Vec<&Sub> = e
            .subscription_ids
            .iter()
            .filter_map(|id| self.subs.get(id))
            .collect();
        if hit.is_empty() {
            // 退订后排着的事件：没有活动范围对应，丢掉（T03 客户端契约）。
            return;
        }
        let feeds_summary = hit.iter().any(|s| s.scope.feeds_summary());
        match e.body {
            Some(pb::event::Body::Persisted(p)) => {
                applied.had_seq = true;
                self.cursor = self.cursor.max(p.seq);
                if feeds_summary {
                    self.refresh.invalidate();
                    // 快照收齐四张表之前来了命中摘要的事件：已收的快照不再代表最新，不能凭它清标记。
                    self.snapshot_tables.clear();
                }
                let Some(event) = p.event.and_then(|d| d.event) else {
                    tracing::warn!(target: "iwork_client", seq = p.seq, "持久事件没有载荷");
                    return;
                };
                self.apply_persisted(p.seq, p.at.as_ref(), &event);
            }
            Some(pb::event::Body::Transient(t)) => {
                if let Some(t) = t.event {
                    self.apply_transient(t, applied);
                }
            }
            None => {}
        }
    }

    /// 直接写的三种 + 时间线。其余持久事件只靠摘要失效（方案 2.2）。
    fn apply_persisted(
        &mut self,
        seq: u64,
        at: Option<&prost_types::Timestamp>,
        event: &pb::domain_event::Event,
    ) {
        use pb::domain_event::Event as E;
        match event {
            E::RuntimeUpdated(e) => {
                if let Some(r) = &e.runtime {
                    self.runtimes.insert(r.runtime_id.clone(), r.clone());
                    self.dirty.mark(ViewKey::Settings);
                    self.mark_header();
                }
            }
            E::QuotaObserved(e) => {
                self.quota.insert(
                    e.runtime_id.clone(),
                    pb::RuntimeQuota {
                        runtime_id: e.runtime_id.clone(),
                        windows: e.windows.clone(),
                        observed_at: e.observed_at,
                    },
                );
                self.dirty.mark(ViewKey::Sidebar);
            }
            E::GlobalSettingsChanged(e) => {
                let s = self.settings.get_or_insert_with(Default::default);
                if let Some(v) = e.playful_ui {
                    s.playful_ui = v;
                }
                if let Some(v) = e.autostart {
                    s.autostart = v;
                }
                if let Some(v) = &e.projects_root {
                    s.projects_root.clone_from(v);
                }
                self.dirty.mark(ViewKey::Settings);
                self.mark_rows(|r| matches!(r, Row::TurnEnd(_)));
            }
            E::CloneFinished(e) => {
                if let Some(job) = self.clone_job.as_mut()
                    && job.progress.job_id == e.job_id
                {
                    job.error = (!e.ok).then(|| {
                        e.error
                            .as_ref()
                            .map_or_else(String::new, |x| x.message.clone())
                    });
                    self.dirty.mark(ViewKey::AddProject);
                }
            }
            E::ConversationCreated(e) => {
                if let Some(id) = e
                    .conversation
                    .as_ref()
                    .and_then(|c| self.created.created(seq, &c.id))
                {
                    self.created_matched(id.0, id.1);
                }
            }
            E::ConversationDeleted(e)
                if self
                    .open
                    .as_ref()
                    .is_some_and(|t| t.conversation_id == e.conversation_id) =>
            {
                self.open = None;
                self.dirty.conversation(TimelineChange {
                    reset: true,
                    ..Default::default()
                });
            }
            _ => {}
        }
        if let Some(t) = self.open.as_mut() {
            match t.apply_event(seq, at, event) {
                Some(c) => self.dirty.conversation(c),
                None => {
                    if matches!(
                        event,
                        E::MessageCompleted(_)
                            | E::TurnStateChanged(_)
                            | E::TurnFinished(_)
                            | E::ToolCallFinished(_)
                            | E::ApprovalDecided(_)
                            | E::ApprovalVoided(_)
                            | E::UserActionStateChanged(_)
                    ) {
                        tracing::debug!(target: "iwork_client", seq, "变化事件的目标不在时间线里，丢掉");
                    }
                }
            }
        }
    }

    /// 把打开的时间线里满足条件的行标脏（行外状态变了，见 `Timeline::identities_where`）。
    fn mark_rows(&mut self, pred: impl Fn(&Row) -> bool) {
        let Some(t) = self.open.as_ref() else {
            return;
        };
        let rows = t.identities_where(pred);
        if !rows.is_empty() {
            self.dirty.mark(ViewKey::Conversation);
            self.dirty.conversation.rows.extend(rows);
        }
    }

    fn apply_transient(&mut self, t: pb::transient_event::Event, applied: &mut Applied) {
        use pb::transient_event::Event as T;
        let (target, offset, text) = match &t {
            T::MessageDelta(d) => (
                DeltaTarget::Message(&d.message_id),
                d.offset,
                d.text.as_str(),
            ),
            T::ToolOutputDelta(d) => (
                DeltaTarget::ToolOutput(&d.tool_call_id),
                d.offset,
                d.text.as_str(),
            ),
            T::CloneProgress(p) => {
                let job = self.clone_job.get_or_insert_with(Default::default);
                job.progress = p.clone();
                job.error = None;
                self.dirty.mark(ViewKey::AddProject);
                return;
            }
            T::SubscriptionLagging(_) => return,
        };
        let Some(tl) = self.open.as_mut() else {
            return;
        };
        let was_pending = tl.gap_pending;
        match tl.apply_delta(Delta {
            target,
            offset,
            text,
        }) {
            DeltaOutcome::Appended(c) => self.dirty.conversation(c),
            DeltaOutcome::Ignored => {}
            DeltaOutcome::Gap => {
                if !was_pending {
                    applied.follow_ups.push(FollowUp::Checkpoint {
                        conversation_id: tl.conversation_id.clone(),
                    });
                }
            }
        }
    }

    fn apply_subscribed(&mut self, s: pb::Subscribed, applied: &mut Applied) {
        let Some(sub) = self.subs.get_mut(&s.subscription_id) else {
            return;
        };
        sub.state = ViewState::Ready;
        if let Some(snapshot) = s.snapshot {
            applied.had_seq = true;
            self.cursor = self.cursor.max(s.snapshot_seq);
            // 快照盖过的新建：建出它的事件不会再逐条来了，配不上。侧栏「新对话」就不跳（对话在侧栏里）；
            // 首页那次停下来报原因，不猜哪个是新对话（猜错会把首条消息发进别的对话）。
            for purpose in self.created.covered_by_snapshot(s.snapshot_seq) {
                if purpose == created::Purpose::Start && self.home.start.is_some() {
                    self.start_failed(home::StartError::Lost);
                }
            }
            self.snapshot_tables.extend(
                snapshot
                    .results
                    .iter()
                    .filter_map(|r| summary_table_of(r.kind.as_ref()?)),
            );
            for r in snapshot.results {
                self.apply_result(None, r);
            }
            // 一次重连的各个快照分在各自的 Subscribed 里：四张表都由快照换过、期间没有命中摘要的事件，才算对齐。
            if SummaryTable::ALL
                .iter()
                .all(|t| self.snapshot_tables.contains(t))
            {
                self.refresh.replaced_by_snapshot();
                self.snapshot_tables.clear();
            }
        }
        if !self.any_loading() {
            applied.follow_ups.push(FollowUp::AllReady);
        }
    }

    /// 查询结果进状态库。`purpose` 为 `None` 表示来自订阅快照。
    fn apply_result(&mut self, purpose: Option<&QueryPurpose>, r: pb::QueryResult) {
        use pb::query_result::Kind as K;
        let Some(kind) = r.kind else {
            return;
        };
        let table = summary_table_of(&kind);
        match (purpose, kind) {
            (
                Some(QueryPurpose::ConversationList {
                    project_id,
                    filter,
                    append,
                    round,
                }),
                K::ListConversations(l),
            ) => {
                if let Some(list) = self.conversation_list.as_mut()
                    && list.query.project_id == *project_id
                    && list.query.filter == *filter
                {
                    if *append {
                        list.items.extend(l.conversations);
                    } else {
                        list.items = l.conversations;
                    }
                    list.has_more = l.has_more;
                    list.loading = false;
                    self.dirty.mark(ViewKey::ConversationList);
                }
                if let Some(round) = round {
                    self.refresh.result_applied(*round, SummaryTable::List);
                }
            }
            (_, K::ListProjects(l)) => {
                // 项目名进了分隔线与「已切到项目」说明的文字（derive::row::project_name）。
                if self.summary.projects != l.projects {
                    self.mark_rows(|r| matches!(r, Row::Divider { .. } | Row::Note { .. }));
                }
                self.summary.projects = l.projects;
                self.mark_summary_views();
            }
            (_, K::ListConversations(l)) => {
                self.summary.conversations = l.conversations;
                self.mark_summary_views();
                self.mark_header();
            }
            (_, K::ListAssistants(l)) => {
                // 助手头像进了消息行（derive::avatar）。
                if self.summary.assistants != l.assistants {
                    self.mark_rows(|r| matches!(r, Row::Message(_)));
                }
                self.summary.assistants = l.assistants;
                self.summary.activities = l.activities;
                self.mark_summary_views();
            }
            (_, K::ListPendingItems(l)) => {
                self.summary.pending = l.items;
                self.mark_summary_views();
                if let Some(t) = self.open.as_mut() {
                    let c = t.sync_resume_cards(self.summary.resume_cards());
                    self.dirty.conversation(c);
                }
            }
            (p, K::GetConversation(page)) => {
                let mode = match p {
                    None => PageMode::Snapshot,
                    Some(QueryPurpose::OlderPage { .. }) => PageMode::Older,
                    Some(_) => PageMode::LiveLatest,
                };
                let cid = page.conversation.as_ref().map(|c| c.id.as_str());
                if let Some(QueryPurpose::OlderPage { before, .. }) = p
                    && self.open.as_ref().and_then(|t| t.oldest_message_id())
                        != Some(before.as_str())
                {
                    tracing::debug!(target: "iwork_client", before, "翻页结果到达时已不接着最旧一行，丢掉");
                    return;
                }
                if let Some(t) = self.open.as_mut()
                    && (cid == Some(t.conversation_id.as_str())
                        || matches!(p, Some(QueryPurpose::OlderPage { conversation_id, .. } | QueryPurpose::LatestPage { conversation_id }) if *conversation_id == t.conversation_id))
                {
                    // Q9（产品负责人 2026-11-11）：快照重建时用户正在看旧消息 → 回到最新并提示。
                    // 「在看旧消息」= 网页声明的窗口没顶到最新一行，或时间线已不接着最新。
                    let reading_old = mode == PageMode::Snapshot
                        && t.identities().last().is_some_and(|(newest, _)| {
                            !t.at_tail
                                || self.window.is_some_and(|(_, last)| last < newest.ordinal())
                        });
                    let mut c = t.apply_page(&page, mode);
                    c.merge(t.sync_resume_cards(self.summary.resume_cards()));
                    if reading_old {
                        // 旧窗口指着重建前的位置；清掉它，下一帧从最新的一页开始。
                        self.window = None;
                        self.back_to_latest = Some(t.conversation_id.clone());
                        self.back_to_latest_started = true;
                        c.header = true;
                    }
                    self.dirty.conversation(c);
                }
            }
            (_, K::GetQuota(q)) => {
                self.quota = q
                    .runtimes
                    .into_iter()
                    .map(|r| (r.runtime_id.clone(), r))
                    .collect();
                self.dirty.mark(ViewKey::Sidebar);
            }
            (_, K::GetRuntimeStatus(s)) => {
                self.runtimes = s
                    .runtimes
                    .into_iter()
                    .map(|r| (r.runtime_id.clone(), r))
                    .collect();
                self.dirty.mark(ViewKey::Settings);
                self.mark_header();
            }
            (_, K::GetSettings(s)) => {
                self.settings = s.global;
                self.dirty.mark(ViewKey::Settings);
            }
            (_, K::ListRules(l)) => {
                self.rules = Some(l.rules);
                self.dirty.mark(ViewKey::Settings);
            }
            (Some(QueryPurpose::LocalConfig { runtime_id }), K::ListLocalConfig(l)) => {
                self.local_config.insert(runtime_id.clone(), l.items);
                self.dirty.mark(ViewKey::Settings);
            }
            (_, K::ListLocalConfig(_)) => {}
            // 安全清单页 M1 不实现（ui-m1.md 第 2 节第 16 条）；制品与模型列表不走视图查询。
            (_, K::ListSecurityLists(_) | K::ReadArtifact(_) | K::ListModels(_)) => {}
        }
        if let (Some(QueryPurpose::Summary { round, table: want }), Some(got)) = (purpose, table)
            && *want == got
        {
            self.refresh.result_applied(*round, got);
        }
    }

    /// 标题栏与输入框一带要重算。首页的输入框用的是同一组输入（运行时、模型清单、连接、项目、助手），一起标。
    fn mark_header(&mut self) {
        self.dirty.mark(ViewKey::Conversation);
        self.dirty.conversation.header = true;
        self.dirty.mark(ViewKey::Home);
    }

    fn mark_summary_views(&mut self) {
        self.dirty.mark(ViewKey::Home);
        self.dirty.mark(ViewKey::Sidebar);
        self.dirty.mark(ViewKey::Pending);
        self.dirty.mark(ViewKey::Assistant);
        self.dirty.mark(ViewKey::AddProject);
    }
}

fn summary_table_of(kind: &pb::query_result::Kind) -> Option<SummaryTable> {
    use pb::query_result::Kind as K;
    match kind {
        K::ListProjects(_) => Some(SummaryTable::Projects),
        K::ListConversations(_) => Some(SummaryTable::Conversations),
        K::ListAssistants(_) => Some(SummaryTable::Assistants),
        K::ListPendingItems(_) => Some(SummaryTable::Pending),
        _ => None,
    }
}
