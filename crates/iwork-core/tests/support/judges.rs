//! 审批裁决屏障测试（T06 方案 15.3）的公共部件：判定方两种实现（参照判定方、真实引擎）放在同一个接口后面，
//! 同一组断言对两者各跑一遍；可控的时钟与定时器；在判定方返回路上的闸门；布景与回合计划。
//! `iwork-fake-agent` 的连着测也经 `#[path]` 用这一份。

use std::collections::BTreeMap;
use std::future::Future;
use std::sync::atomic::{AtomicBool, AtomicI64, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use iwork_core::approval::engine::{
    ApprovalEngine, EnginePorts, SessionDecider, SessionKeys, UserDecision,
};
use iwork_core::approval::session::tool_set_for;
use iwork_core::turn::{Decided, LIMIT_CHECK_INTERVAL, PermissionDecider, VoidPlanError};
use iwork_domain::approval::VoidReason;
use iwork_domain::approval_mode::{ApprovalMode, RememberScope};
use iwork_domain::common::{Actor, ModelId, ModelSelection, Timestamp, ToolGroups};
use iwork_domain::event::{DomainEvent, TurnQueued};
use iwork_domain::id::{ApprovalId, RawUlid, RuntimeId, Seq, TurnId};
use iwork_domain::project::AiReviewSettings;
use iwork_domain::turn::{Turn, TurnKind, TurnState};
use iwork_ports::agent::{PermissionRequest, SessionSpec};
use iwork_ports::clock::{Clock, MonotonicInstant, Timer};
use tokio::sync::{Notify, watch};

use super::engine_fakes::{FakeReviewer, FakeTimer, Ids as EngineIds, MemStore, TickClock};
use super::fake_fs::FakeFs;
use super::ordered_decider::OrderedDecider;
use super::{WS, assistant, conversation, cp, env, fold_fx, fs, project, snapshot_on};

// ---------- 判定方 ----------

/// 屏障测试里的判定方：提供 `PermissionDecider`，并替用户做决定、回答「还留没留授权」。
pub trait Judge: Send + Sync + 'static {
    type D: PermissionDecider;
    const NAME: &'static str;
    /// 有没有拒绝记忆（AC05：被拒的动作在用户下一条消息前自动拒绝）。参照判定方只管 R1–R3，没有
    const DENIAL_MEMORY: bool;
    /// 布景（`seed_world`）之后建。
    fn make(store: &MemStore) -> impl Future<Output = Self> + Send;
    fn decider(&self) -> Arc<Self::D>;
    /// 用户对这条审批做决定（提交 `ApprovalDecided`）。
    fn user(&self, id: ApprovalId, allow: bool) -> impl Future<Output = ()> + Send;
    /// 追平之后，这条审批有没有留下一次性授权。
    fn has_grant(&self, id: ApprovalId) -> impl Future<Output = bool> + Send;
}

/// 参照判定方（`ordered_decider.rs`）。
pub struct Reference(Arc<OrderedDecider>);

impl Reference {
    pub fn new(store: &MemStore) -> Self {
        Self(Arc::new(OrderedDecider::new(store.clone(), conversation())))
    }
}

impl Judge for Reference {
    type D = OrderedDecider;
    const NAME: &'static str = "reference";
    const DENIAL_MEMORY: bool = false;
    async fn make(store: &MemStore) -> Self {
        Self::new(store)
    }
    fn decider(&self) -> Arc<OrderedDecider> {
        self.0.clone()
    }
    async fn user(&self, id: ApprovalId, allow: bool) {
        self.0.decide_user(id, allow).await;
    }
    async fn has_grant(&self, id: ApprovalId) -> bool {
        self.0.has_grant(id)
    }
}

pub type Engine = ApprovalEngine<TickClock, EngineIds, MemStore, FakeReviewer, FakeTimer, FakeFs>;
pub type EngineDecider =
    SessionDecider<TickClock, EngineIds, MemStore, FakeReviewer, FakeTimer, FakeFs>;

/// 真实审批引擎（T07），手动档，会话快照与 `delivery_gate` 用的一样。
pub struct RealEngine {
    pub engine: Arc<Engine>,
    decider: Arc<EngineDecider>,
}

pub fn engine_keys() -> SessionKeys {
    SessionKeys {
        conversation: conversation(),
        assistant: assistant(),
        runtime_id: RuntimeId::from_raw(RawUlid::from_u128(0x888)),
        assistant_version: 1,
        project: Some(project()),
        turn_kind: TurnKind::Assistant,
        snapshot: Arc::new(snapshot_on(
            ApprovalMode::Manual,
            &[],
            &env(),
            Seq(1),
            &fs(),
        )),
        tool_set: tool_set_for(TurnKind::Assistant, &ToolGroups::new()),
        disabled_groups: ToolGroups::new(),
        cwd: cp(WS),
        paths: env(),
        ai_review: AiReviewSettings {
            enabled: false,
            runtime_id: None,
            model: None,
            timeout_secs: 60,
        },
        review_model: model(),
    }
}

impl RealEngine {
    /// 布景（`seed_world`）之后起引擎。
    pub async fn start(store: &MemStore) -> Self {
        let (engine, recovery) = Engine::start(
            EnginePorts {
                clock: TickClock::default(),
                ids: EngineIds::default(),
                store: store.clone(),
                reviewer: FakeReviewer::default(),
                timer: FakeTimer::default(),
                fs: fs(),
            },
            Default::default(),
            |_, _| None,
        )
        .await
        .expect("引擎起得来");
        assert!(recovery.is_empty());
        let decider = Arc::new(engine.decider_for(engine_keys()));
        Self { engine, decider }
    }
}

impl Judge for RealEngine {
    type D = EngineDecider;
    const NAME: &'static str = "engine";
    const DENIAL_MEMORY: bool = true;
    async fn make(store: &MemStore) -> Self {
        Self::start(store).await
    }
    fn decider(&self) -> Arc<EngineDecider> {
        self.decider.clone()
    }
    async fn user(&self, id: ApprovalId, allow: bool) {
        let decision = if allow {
            UserDecision::Allow {
                remember: RememberScope::Once,
            }
        } else {
            UserDecision::Deny
        };
        self.engine
            .decide(id, decision, Actor("user".into()), super::command())
            .await
            .expect("用户决定被接受");
    }
    async fn has_grant(&self, id: ApprovalId) -> bool {
        self.engine.sync().await.expect("追平");
        self.engine.grant(id).is_some()
    }
}

// ---------- 判定方返回路上的闸门 ----------

/// 一道闸门：判定方的结论出来之后、交回 `PermissionGate` 之前停在这里。
#[derive(Default)]
pub struct Gate {
    open: Notify,
    /// 结论已经出来、正停在闸门前
    pub reached: AtomicBool,
}

impl Gate {
    pub fn open(&self) {
        self.open.notify_one();
    }

    /// 停在闸门前，直到 `open`。
    pub async fn wait(&self) {
        self.open.notified().await;
    }
}

/// 包一层判定方：工具输入含某个标记的请求，结论出来后停在对应的闸门前。
pub struct GateDecider<D> {
    inner: Arc<D>,
    gates: Mutex<BTreeMap<String, Arc<Gate>>>,
    /// 要过几次作废计划
    pub void_calls: AtomicU64,
}

impl<D> GateDecider<D> {
    pub fn new(inner: Arc<D>) -> Self {
        Self {
            inner,
            gates: Mutex::new(BTreeMap::new()),
            void_calls: AtomicU64::new(0),
        }
    }

    /// 工具输入含 `marker` 的请求在闸门前停下。
    pub fn gate(&self, marker: &str) -> Arc<Gate> {
        let gate = Arc::new(Gate::default());
        self.gates
            .lock()
            .unwrap()
            .insert(marker.to_owned(), gate.clone());
        gate
    }
}

impl<D: PermissionDecider> PermissionDecider for GateDecider<D> {
    type VoidHold = D::VoidHold;

    async fn decide(&self, turn_id: TurnId, req: &PermissionRequest) -> Decided {
        let decided = self.inner.decide(turn_id, req).await;
        let gate = self
            .gates
            .lock()
            .unwrap()
            .iter()
            .find(|(marker, _)| req.raw_input.0.contains(marker.as_str()))
            .map(|(_, g)| g.clone());
        if let Some(gate) = gate {
            gate.reached.store(true, Ordering::SeqCst);
            gate.wait().await;
        }
        decided
    }

    async fn void_pending(
        &self,
        turn_id: TurnId,
        reason: VoidReason,
    ) -> Result<(Vec<DomainEvent>, D::VoidHold), VoidPlanError> {
        self.void_calls.fetch_add(1, Ordering::SeqCst);
        self.inner.void_pending(turn_id, reason).await
    }

    async fn committed(&self, events: &[DomainEvent]) {
        self.inner.committed(events).await;
    }
}

// ---------- 时钟与定时器 ----------

/// 测试拨动的时钟：单调时钟与墙钟分开拨（等待上限取两者经过时间的较大者）。
pub struct ManualClock {
    mono_ns: AtomicU64,
    wall_ms: AtomicI64,
}

impl Default for ManualClock {
    fn default() -> Self {
        Self {
            mono_ns: AtomicU64::new(1_000_000_000),
            wall_ms: AtomicI64::new(1_700_000_000_000),
        }
    }
}

impl ManualClock {
    pub fn advance_mono(&self, d: Duration) {
        self.mono_ns
            .fetch_add(d.as_nanos() as u64, Ordering::SeqCst);
    }
    pub fn advance_wall(&self, d: Duration) {
        self.wall_ms
            .fetch_add(d.as_millis() as i64, Ordering::SeqCst);
    }
}

impl Clock for ManualClock {
    fn now(&self) -> Timestamp {
        Timestamp(self.wall_ms.load(Ordering::SeqCst))
    }
    fn monotonic(&self) -> MonotonicInstant {
        MonotonicInstant(self.mono_ns.load(Ordering::SeqCst))
    }
}

/// 只由测试推进的定时器：0 立即到；`LIMIT_CHECK_INTERVAL` 那么长的在 `tick()` 时到（提醒判定方的期限
/// 也是这么长，到了只是丢掉一次提醒，无害）；别的（检查点、停止期限）永不到。测试从不推进别的时长，
/// 所以「没有走过 GRACE_PERIOD」「没有走过 STOP_LIMIT」由构造保证。
pub struct ManualTimer {
    ticks: watch::Sender<u64>,
}

impl Default for ManualTimer {
    fn default() -> Self {
        Self {
            ticks: watch::channel(0).0,
        }
    }
}

impl ManualTimer {
    /// 让此刻在等的每一次「查期限」都到时。
    pub fn tick(&self) {
        self.ticks.send_modify(|n| *n += 1);
    }
}

impl Timer for ManualTimer {
    async fn sleep(&self, d: Duration) {
        if d.is_zero() {
            return;
        }
        if d != LIMIT_CHECK_INTERVAL {
            return std::future::pending().await;
        }
        let mut rx = self.ticks.subscribe();
        let start = *rx.borrow_and_update();
        let _ = rx.wait_for(|n| *n > start).await;
    }
}

// ---------- 布景 ----------

pub fn model() -> ModelSelection {
    ModelSelection {
        model_id: ModelId("m".into()),
        effort: None,
    }
}

/// 一个排队的回合（项目对话里、本助手的）。`n` 区分同一布景里的多个回合。
pub fn queued_turn(n: u128) -> Turn {
    let mut t = fold_fx::turn(n, 1, 1);
    t.id = TurnId::from_raw(RawUlid::from_u128(0x7700 + n));
    t.conversation_id = conversation();
    t.assistant_id = assistant();
    t
}

/// 项目、助手、对话，以及这些排队的回合。
pub fn seed_world(store: &MemStore, turns: &[&Turn]) {
    store.seed(fold_fx::world_with(
        project(),
        conversation(),
        &[assistant()],
        None,
    ));
    seed_turns(store, turns);
}

pub fn seed_turns(store: &MemStore, turns: &[&Turn]) {
    store.seed(
        turns
            .iter()
            .map(|t| TurnQueued { turn: (*t).clone() }.into())
            .collect(),
    );
}

/// 工具输入含 `marker` 的那条审批（按请求的先后找第一条）。
pub fn approval_for(store: &MemStore, marker: &str) -> Option<ApprovalId> {
    store.all().into_iter().find_map(|e| match e {
        DomainEvent::ApprovalRequested(r) if r.approval.action.raw_summary.contains(marker) => {
            Some(r.approval.id)
        }
        _ => None,
    })
}

/// 这条审批还在待定（没有决定、没有作废）。
pub fn still_pending(store: &MemStore, id: ApprovalId) -> bool {
    !store.all().iter().any(|e| match e {
        DomainEvent::ApprovalDecided(d) => d.approval_id == id,
        DomainEvent::ApprovalVoided(v) => v.approval_id == id,
        _ => false,
    })
}

/// 回合与审批的事件，渲染成便于比对的短文本：状态变化、交付、决定、作废、结束。
pub fn trail(store: &MemStore, turn: TurnId) -> Vec<String> {
    let ids: BTreeMap<ApprovalId, usize> = store
        .all()
        .iter()
        .filter_map(|e| match e {
            DomainEvent::ApprovalRequested(r) if r.approval.turn_id == turn => Some(r.approval.id),
            _ => None,
        })
        .enumerate()
        .map(|(i, id)| (id, i))
        .collect();
    let name = |id: &ApprovalId| {
        ids.get(id)
            .map_or("?".to_owned(), |i| char::from(b'A' + *i as u8).to_string())
    };
    store
        .all()
        .iter()
        .filter_map(|e| match e {
            DomainEvent::TurnStateChanged(c) if c.turn_id == turn => Some(match &c.reason {
                Some(r) => format!("{:?} -> {:?} ({r})", c.from, c.to),
                None => format!("{:?} -> {:?}", c.from, c.to),
            }),
            DomainEvent::ApprovalDecided(d) if ids.contains_key(&d.approval_id) => {
                Some(format!("decided {} {:?}", name(&d.approval_id), d.state))
            }
            DomainEvent::ApprovalDelivered(d) if d.turn_id == turn => {
                Some(format!("delivered {}", name(&d.approval_id)))
            }
            DomainEvent::ApprovalVoided(v) if ids.contains_key(&v.approval_id) => {
                Some(format!("voided {}", name(&v.approval_id)))
            }
            DomainEvent::TurnFinished(f) if f.turn_id == turn => {
                Some(format!("finished {:?}", f.state))
            }
            _ => None,
        })
        .collect()
}

/// 回合最后一次状态变化后的状态。
pub fn state_of(store: &MemStore, turn: TurnId) -> Option<TurnState> {
    store.all().iter().rev().find_map(|e| match e {
        DomainEvent::TurnStateChanged(c) if c.turn_id == turn => Some(c.to),
        _ => None,
    })
}

/// 回合用的 `SessionSpec`：工作目录由调用方给（假运行时要一个真的目录）。
pub fn session_spec(cwd: std::path::PathBuf) -> SessionSpec {
    use iwork_domain::session::{AllowWrite, NetworkPolicy, SessionConstraints, SessionToolSet};
    SessionSpec {
        cwd,
        model: model(),
        system_prompt: String::new(),
        mcp: Vec::new(),
        constraints: SessionConstraints {
            deny_read_write: Vec::new(),
            read_only: Vec::new(),
            allow_write: AllowWrite::Anywhere,
            excluded_commands: Vec::new(),
            allow_unsandboxed_retry: false,
            network: NetworkPolicy::Open,
        },
        tool_set: SessionToolSet {
            enabled_groups: ToolGroups::new(),
            include_always_on: true,
        },
        disabled_tools: Vec::new(),
        plugin_dirs: Vec::new(),
        resume: None,
        kind: TurnKind::Assistant,
    }
}

/// 等条件成立，最多 5 秒（真实时间）。
pub async fn wait_until(what: &str, mut ok: impl FnMut() -> bool) {
    let start = std::time::Instant::now();
    while !ok() {
        assert!(
            start.elapsed() < Duration::from_secs(5),
            "5 秒内没等到：{what}"
        );
        tokio::time::sleep(Duration::from_millis(2)).await;
    }
}
