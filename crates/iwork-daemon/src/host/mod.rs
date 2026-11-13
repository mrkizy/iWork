//! 回合宿主（T10a 方案 2.5）：后台里一个常驻任务，管全部会话线；每个对话一条会话线，一个会话线任务拥有那条
//! `SessionLine`。宿主做什么只看两样：已提交的状态（对账时读库），自己的关闭状态（内存）。
//!
//! 会话线表与关闭状态在**一把锁**里，锁里没有等待点。几条硬规矩（不变量 12、14）：
//! - 交活即占用：锁里写 `job` 再叫醒任务，没有队列；占着的线不再交第二份活（D3）。
//! - 交活的依据不比这条线上一趟的结束更旧：对账读库之前在锁里记下每条线的趟数，要交活时趟数变了就这一轮不交（G8）。
//! - 发取消在锁里**同步**调用：看这一项占着的是哪一趟、调 `TurnCanceller::cancel`，同一次持锁里做完；取消器只放在
//!   锁保护的 [`LineEntry`] 里，不克隆出去（2.4「早到的取消认趟」末条）。
//! - 记一趟的结束和交下一趟是同一把锁（D4）。

pub mod changes;
mod line;
pub mod plan;
pub mod ports;
pub mod prepare;
mod quit;

use std::collections::BTreeMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::Duration;

use iwork_core::approval::engine::Scope;
use iwork_core::turn::{OwedBy, RunTag, owed_turn_events};
use iwork_domain::approval::VoidReason;
use iwork_domain::common::Actor;
use iwork_domain::conversation::ConversationState;
use iwork_domain::id::{CommandId, ConversationId, Seq, TurnId};
use iwork_domain::turn::TurnState;
use iwork_ports::clock::IdGenerator;
use iwork_ports::read_model::{ConversationWork, ReadModelPort, TurnWork};
use iwork_ports::store::{CommitRequest, StorageError, StoragePort};
use tokio::sync::{Notify, watch};
use tokio::task::JoinSet;

use self::plan::{Action, Closing, JobKind, JobView, LastRun, LineView, Round, decide};
use self::ports::{HostEngine, HostPorts};

/// 这些事件落盘就叫醒宿主（0a.6）。加一种 = 加一行。安全清单的增删不在这里：D8 每个回合开始前自己重读。
/// 漏了一种或唤醒丢了，后果是晚到保底的 [`BACKSTOP`]，不是没人管。
pub const TURN_HOST_WAKE_KINDS: &[&str] = &[
    "turn_queued",
    "turn_state_changed",
    "turn_intent_recorded",
    "conversation_deleting",
    // 删除在等卡片
    "user_action_state_changed",
];

/// 宿主的每次读、每笔提交、会话线准备里的读，等这么久（K6 之外的保底）。到时当去向不明、下一轮重来。
pub const HOST_IO_LIMIT: Duration = Duration::from_secs(30);
/// 保底每这么久对一次账（唤醒丢了、登记表漏了一种事件，后果是晚到这么久）。
pub const BACKSTOP: Duration = Duration::from_secs(30);
/// 读 `turns_to_resume` 之前让引擎追平，等这么久（D7 ①）。
pub const CATCH_UP_LIMIT: Duration = Duration::from_secs(5);
/// 一条线上一趟没成之后再交的退避（D5a）：1 秒起翻倍、封顶 60 秒。
pub const LINE_BACKOFF_START: Duration = Duration::from_secs(1);
pub const LINE_BACKOFF_MAX: Duration = Duration::from_secs(60);
/// 宿主自己的提交被拒或忙之后的退避（D5）：100 毫秒起翻倍、封顶 5 秒，不设次数上限。
pub const HOST_BACKOFF_START: Duration = Duration::from_millis(100);
pub const HOST_BACKOFF_MAX: Duration = Duration::from_secs(5);

/// 宿主宣告致命（T04 2.10 登记 `turn-host`）：整进程退出交崩溃恢复，不在进程内补事实（P3 a）。
#[derive(Debug, thiserror::Error)]
pub enum HostError {
    #[error("会话线任务异常结束：{0}")]
    LineTask(String),
    #[error("{0}")]
    Impossible(String),
}

/// 启动失败（R3）。
#[derive(Debug, thiserror::Error)]
pub enum HostStartError {
    #[error(transparent)]
    Storage(#[from] StorageError),
    #[error("启动顺序排错了：宿主起来时库里还有占着进程的回合 {0:?}（崩溃恢复应先把它们收掉）")]
    UnsettledTurns(Vec<TurnId>),
}

/// 占着一条线的那一趟（D3）：从交活起，到任务报回结局止。
#[derive(Debug, Clone, Copy)]
struct JobInFlight {
    kind: JobKind,
    /// 这条线上的第几趟：交活时取 `runs + 1`
    run: RunTag,
    /// 过了派发边界没有（`begin_run` 置位）
    crossed: bool,
    cancel_sent: bool,
}

#[derive(Debug, Clone, Copy)]
struct Backoff {
    delay: Duration,
    ready_at: tokio::time::Instant,
}

/// 会话线表的一项。
struct LineEntry {
    job: Option<JobInFlight>,
    /// 趟数，只增；每一趟结束加一（D3、G8）
    runs: u64,
    /// 上一趟交的是哪个回合、怎么结束的、发过取消没有
    last: Option<LastRun>,
    /// 这条线此刻那个 `SessionLine` 的取消器：`begin_run` 时换成要跑的那一条的（换过线后旧的作废）
    canceller: Option<iwork_core::turn::TurnCanceller>,
    /// 叫醒这条线的任务去取活
    wake: Arc<Notify>,
    backoff: Option<Backoff>,
    /// 任务起过没有（宿主第一次要用这条线时起）
    task_started: bool,
}

impl LineEntry {
    fn new() -> Self {
        Self {
            job: None,
            runs: 0,
            last: None,
            canceller: None,
            wake: Arc::new(Notify::new()),
            backoff: None,
            task_started: false,
        }
    }

    fn view(&self, now: tokio::time::Instant) -> LineView {
        LineView {
            job: self.job.map(|j| JobView {
                kind: j.kind,
                run: j.run,
                crossed: j.crossed,
                cancel_sent: j.cancel_sent,
            }),
            last: self.last.clone(),
            backoff_ready: self.backoff.is_none_or(|b| now >= b.ready_at),
        }
    }
}

struct Lines {
    closing: Closing,
    by_conversation: BTreeMap<ConversationId, LineEntry>,
}

/// 测试的可观测点（只在测试构建里有）：对账跑到第几轮、交了几次活；两处屏障（7.2a 的 K5、K7）。
#[cfg(any(test, feature = "test-hooks"))]
#[derive(Default)]
pub struct HostCounters {
    pub rounds: std::sync::atomic::AtomicU64,
    pub handed: std::sync::atomic::AtomicU64,
    /// 会话线准备里的读发了几次
    pub prepare_reads: std::sync::atomic::AtomicU64,
    /// 一趟结束（在锁里记完结局）了几次
    pub finished: std::sync::atomic::AtomicU64,
    /// 在锁里给编排发过几次取消
    pub cancels_sent: std::sync::atomic::AtomicU64,
    /// 每次在锁里发的取消带的（回合，趟次）：宿主这一层证明发的是当前这一趟的趟次
    pub cancels_log: Mutex<Vec<(TurnId, RunTag)>>,
    pub round_done: Notify,
    pub hooks: Mutex<HostHooks>,
    /// `abort_snapshot` 读的中途调一次（同步，在调用它的线程上阻塞；只调一次；b45 2/F-2 的另一种交错）
    pub during_snapshot: Mutex<Option<Box<dyn FnOnce() + Send>>>,
}

#[cfg(any(test, feature = "test-hooks"))]
pub type HostHook =
    Arc<dyn Fn() -> std::pin::Pin<Box<dyn std::future::Future<Output = ()> + Send>> + Send + Sync>;

/// 屏障：装了就在那一处等它返回。
#[cfg(any(test, feature = "test-hooks"))]
#[derive(Default, Clone)]
pub struct HostHooks {
    /// 对账读完库、还没交活
    pub after_round_read: Option<HostHook>,
    /// 会话线准备的读之前（读还没发）
    pub before_prepare_read: Option<HostHook>,
    /// 封口那一支：这条线报回了清树结果之后、会话线任务退出之前（b45 2/F-2 的一种交错）
    pub after_abort_report: Option<HostHook>,
}

#[cfg(any(test, feature = "test-hooks"))]
impl HostCounters {
    pub(crate) fn record_cancel(&self, turn: TurnId, run: RunTag) {
        self.cancels_sent.fetch_add(1, Ordering::SeqCst);
        self.cancels_log
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .push((turn, run));
    }

    pub(crate) async fn at(&self, pick: impl Fn(&HostHooks) -> Option<HostHook>) {
        let hook = pick(&self.hooks.lock().unwrap_or_else(PoisonError::into_inner));
        if let Some(h) = hook {
            h().await;
        }
    }
}

struct Shared {
    lines: Mutex<Lines>,
    /// 叫醒对账：提交观察者手里的也是这一个（0a.6）
    wake: Arc<Notify>,
    /// 启动时读到的 head（R7）
    boot_head: Seq,
    /// 封口了没有：会话线任务收到就丢掉手上的一整趟（E3）
    sealed: watch::Sender<bool>,
    /// 一趟结束、一条线的任务退出时叫一声（`cancel_all`、`abort_all` 等它们）
    idle: Notify,
    /// 还活着的会话线任务与封口时它们报回的结果（b45 2/F-2：一把锁里一起改、一起读）
    tally: Mutex<LineTally>,
    // 等存储的地方各自的在途额度（不变量 13；方案 0a.8 表下，b45 第 2 轮定的划分）。会话线准备的读（B6）每条线一个，
    // 在 `line.rs`；拍改动的读（全后台 2）在 `changes.rs`。
    /// B1 宿主对账的读（全局 1）
    read_busy: Arc<AtomicBool>,
    /// B2 对账里引擎追平（全局 1）：卡住只意味着这一轮不按批准续接
    catch_up_busy: Arc<AtomicBool>,
    /// B3 宿主自己的提交（全局 1）：补账连同它前面那次读、收掉回合、删除的最后一笔、完全退出时撤排队。许可跟着
    /// 干活的小任务走，它真的结束才还
    host_commit: Arc<tokio::sync::Semaphore>,
    /// B4 退出前数目（E1，全局 1）：命令路径上的读，不挡对账，也不被对账的读挡成「不知道」
    quit_read_busy: Arc<AtomicBool>,
    /// B5 完全退出时读排着的回合（E2 ③，全局 1）：同上，不被一次正常的对账读挡进 KA1
    withdraw_read_busy: Arc<AtomicBool>,
    #[cfg(any(test, feature = "test-hooks"))]
    counters: HostCounters,
}

/// 会话线任务的点名册：还活着、还没报回封口结果的条数，与已经报回的结果。一条线报回结果和从「没报回」里摘掉它是
/// 同一次加锁里的一次改动，快照也在同一把锁里读（b45 2/F-2）。
#[derive(Default)]
struct LineTally {
    unreported: usize,
    report: crate::work::AbortReport,
}

impl Shared {
    fn lock(&self) -> MutexGuard<'_, Lines> {
        self.lines.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn tally(&self) -> MutexGuard<'_, LineTally> {
        self.tally.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

/// 回合宿主。
pub struct TurnHost<P: HostPorts> {
    ports: Arc<P>,
    shared: Arc<Shared>,
}

impl<P: HostPorts> Clone for TurnHost<P> {
    fn clone(&self) -> Self {
        Self {
            ports: self.ports.clone(),
            shared: self.shared.clone(),
        }
    }
}

/// 至多一个在途的一次读或提交：交给不随等的人被丢掉的小任务，在途标记跟着它，它真的结束才放（不变量 13）。
/// 宿主与会话线里每一处等存储的读都经它，宿主自己的提交经 [`with_permit`]（b45 1/F-2、第 2 轮的划分）；唯一的
/// 例外是换线时补记绑定回退（`SessionLine::settle`），见 `line.rs` 的 `settle_and_retire`。
pub(crate) enum Bounded<T> {
    Done(T),
    /// 上一次还没真回来：这一次没发
    Busy,
    TimedOut,
}

pub(crate) async fn bounded<T, F>(flag: &Arc<AtomicBool>, limit: Duration, work: F) -> Bounded<T>
where
    T: Send + 'static,
    F: std::future::Future<Output = T> + Send + 'static,
{
    if flag.swap(true, Ordering::SeqCst) {
        return Bounded::Busy;
    }
    let held = flag.clone();
    let task = tokio::spawn(async move {
        let out = work.await;
        held.store(false, Ordering::SeqCst);
        out
    });
    match tokio::time::timeout(limit, task).await {
        Ok(Ok(out)) => Bounded::Done(out),
        // 小任务 panic：标记没放，照「去向不明」处理，记日志
        Ok(Err(e)) => {
            tracing::error!(error = %e, "宿主的读或提交异常结束");
            flag.store(false, Ordering::SeqCst);
            Bounded::TimedOut
        }
        Err(_) => Bounded::TimedOut,
    }
}

/// 拿着一份许可（B3）做一次读写：交给小任务，许可跟着它，它真的结束才还；等的人到时只是走开。
async fn with_permit<T, F>(
    permit: tokio::sync::OwnedSemaphorePermit,
    limit: Duration,
    work: F,
) -> Bounded<T>
where
    T: Send + 'static,
    F: std::future::Future<Output = T> + Send + 'static,
{
    let task = tokio::spawn(async move {
        let out = work.await;
        drop(permit);
        out
    });
    match tokio::time::timeout(limit, task).await {
        Ok(Ok(out)) => Bounded::Done(out),
        // 小任务 panic：许可随它的栈一起放掉，照「去向不明」处理
        Ok(Err(e)) => {
            tracing::error!(error = %e, "宿主的提交异常结束");
            Bounded::TimedOut
        }
        Err(_) => Bounded::TimedOut,
    }
}

/// 一轮对账之后多久再来。
enum Next {
    Now,
    After(Duration),
}

impl<P: HostPorts> TurnHost<P> {
    /// 启动表第 9 步：读 head 记成 `boot_head`；库里有占着进程的回合就失败并点名（R3）。`wake` 是提交观察者
    /// 叫醒宿主用的那一个（`PublishState::host`）。
    pub async fn start(ports: Arc<P>, wake: Arc<Notify>) -> Result<Self, HostStartError> {
        let work = ports.read().turn_work().await?;
        let held: Vec<TurnId> = work
            .conversations
            .iter()
            .flat_map(|w| w.open_turns.iter())
            .filter(|t| t.turn.state.holds_process())
            .map(|t| t.turn.id)
            .collect();
        if !held.is_empty() {
            return Err(HostStartError::UnsettledTurns(held));
        }
        Ok(Self {
            ports,
            shared: Arc::new(Shared {
                lines: Mutex::new(Lines {
                    closing: Closing::Open,
                    by_conversation: BTreeMap::new(),
                }),
                wake,
                boot_head: work.head,
                sealed: watch::Sender::new(false),
                idle: Notify::new(),
                tally: Mutex::default(),
                read_busy: Arc::default(),
                catch_up_busy: Arc::default(),
                host_commit: Arc::new(tokio::sync::Semaphore::new(1)),
                quit_read_busy: Arc::default(),
                withdraw_read_busy: Arc::default(),
                #[cfg(any(test, feature = "test-hooks"))]
                counters: HostCounters::default(),
            }),
        })
    }

    /// 叫醒对账（提交观察者、`engine-sync`、会话线任务报回结局都调它）。只置位。
    pub fn wake(&self) {
        self.shared.wake.notify_one();
    }

    /// 受理路径读内存的唯一一处（A6）：宿主关了没有。
    pub fn is_closed(&self) -> bool {
        self.shared.lock().closing != Closing::Open
    }

    #[cfg(any(test, feature = "test-hooks"))]
    pub fn counters(&self) -> &HostCounters {
        &self.shared.counters
    }

    /// 常驻任务（致命）：一直对账；会话线任务 panic 或宿主碰到不可能的状态时返回错误。
    pub async fn run(&self) -> HostError {
        let mut tasks: JoinSet<()> = JoinSet::new();
        let mut host_backoff = HOST_BACKOFF_START;
        let mut spawn = Vec::new();
        loop {
            let notified = self.shared.wake.notified();
            let next = tokio::select! {
                biased;
                Some(done) = tasks.join_next(), if !tasks.is_empty() => {
                    match done {
                        Ok(()) => continue,
                        Err(e) => return HostError::LineTask(e.to_string()),
                    }
                }
                r = self.round(&mut spawn, &mut host_backoff) => match r {
                    Ok(next) => next,
                    Err(e) => return e,
                },
            };
            // 这一轮第一次交活的线：起它的任务（交活时这一项已写好，任务起来就取得到）
            for (c, wake) in spawn.drain(..) {
                self.shared.tally().unreported += 1;
                tasks.spawn(line::run(self.ports.clone(), self.shared.clone(), c, wake));
            }
            #[cfg(any(test, feature = "test-hooks"))]
            {
                self.shared.counters.rounds.fetch_add(1, Ordering::SeqCst);
                self.shared.counters.round_done.notify_waiters();
            }
            let wait = match next {
                Next::Now => continue,
                Next::After(d) => d,
            };
            tokio::select! {
                biased;
                Some(done) = tasks.join_next(), if !tasks.is_empty() => {
                    if let Err(e) = done {
                        return HostError::LineTask(e.to_string());
                    }
                }
                _ = notified => {}
                _ = tokio::time::sleep(wait) => {}
            }
        }
    }

    /// 对账的一轮（2.5）。
    async fn round(
        &self,
        spawn: &mut Vec<(ConversationId, Arc<Notify>)>,
        host_backoff: &mut Duration,
    ) -> Result<Next, HostError> {
        // 1. 封口了就什么都不做；在锁里记下每条线此刻的趟数（G8）
        let runs_before: BTreeMap<ConversationId, u64> = {
            let lines = self.shared.lock();
            if lines.closing == Closing::Sealed {
                return Ok(Next::After(BACKSTOP));
            }
            lines
                .by_conversation
                .iter()
                .map(|(c, e)| (*c, e.runs))
                .collect()
        };
        // 2. 读库，至多一个在途
        let ports = self.ports.clone();
        let work: TurnWork = match bounded(&self.shared.read_busy, HOST_IO_LIMIT, async move {
            ports.read().turn_work().await
        })
        .await
        {
            Bounded::Done(Ok(w)) => w,
            Bounded::Done(Err(e)) => {
                tracing::warn!(error = %e, "宿主对账读库失败，退避后再来");
                return Ok(Next::After(next_backoff(host_backoff)));
            }
            Bounded::Busy | Bounded::TimedOut => {
                tracing::error!("宿主对账的上一次读还没回来，这一轮不发新的读");
                return Ok(Next::After(BACKSTOP));
            }
        };
        #[cfg(any(test, feature = "test-hooks"))]
        self.shared
            .counters
            .at(|h| h.after_round_read.clone())
            .await;
        // 3. 引擎追平了才用它的续接名单
        let ports = self.ports.clone();
        let caught_up = bounded(&self.shared.catch_up_busy, CATCH_UP_LIMIT, async move {
            ports.engine().catch_up().await
        })
        .await;
        let resume: BTreeMap<TurnId, Seq> = match caught_up {
            Bounded::Done(Ok(())) => self.ports.engine().turns_to_resume(),
            Bounded::Done(Err(e)) => {
                tracing::warn!(error = %e, "引擎没追平，这一轮不按批准续接");
                BTreeMap::new()
            }
            Bounded::Busy | Bounded::TimedOut => {
                tracing::warn!("引擎 5 秒没追平（或上一次还没回来），这一轮不按批准续接");
                BTreeMap::new()
            }
        };
        // 4. 每个对话一件事；锁里的事（交活、发取消）当场做，要提交的收起来出锁做
        let mut commits: Vec<(ConversationWork, Action)> = Vec::new();
        let mut again = false;
        let now = tokio::time::Instant::now();
        let mut soonest: Option<tokio::time::Instant> = None;
        {
            let mut lines = self.shared.lock();
            let round = Round {
                closing: lines.closing,
                boot_head: self.shared.boot_head,
                turns_to_resume: &resume,
            };
            for w in &work.conversations {
                let c = w.conversation.id;
                // G8：读库之后这条线上又有一趟结束了，库里这个对话的状态可能已经不是读到的那样。拿旧读判这条线的任何事
                // （交活、补账、判「不可能」）都不对：这一轮跳过它，立刻再对一轮
                let runs_now = lines.by_conversation.get(&c).map_or(0, |e| e.runs);
                if runs_before.get(&c).copied().unwrap_or(0) != runs_now {
                    again = true;
                    continue;
                }
                let view = lines.by_conversation.get(&c).map(|e| e.view(now));
                if let Some(b) = lines.by_conversation.get(&c).and_then(|e| e.backoff)
                    && b.ready_at > now
                {
                    soonest = Some(soonest.map_or(b.ready_at, |s| s.min(b.ready_at)));
                }
                let Some(action) = decide(w, view.as_ref(), &round) else {
                    continue;
                };
                match action {
                    Action::Hand(kind) => {
                        let entry = lines
                            .by_conversation
                            .entry(c)
                            .or_insert_with(LineEntry::new);
                        debug_assert!(entry.job.is_none(), "decide 只对空着的线交活");
                        entry.job = Some(JobInFlight {
                            kind,
                            run: RunTag(entry.runs + 1),
                            crossed: false,
                            cancel_sent: false,
                        });
                        #[cfg(any(test, feature = "test-hooks"))]
                        self.shared.counters.handed.fetch_add(1, Ordering::SeqCst);
                        if !entry.task_started {
                            entry.task_started = true;
                            spawn.push((c, entry.wake.clone()));
                        }
                        entry.wake.notify_one();
                    }
                    Action::Cancel { turn, run } => {
                        // 发取消在锁里同步调用（不变量 14）：看这一项占着的是哪一趟、调取消，同一次持锁里做完
                        if let Some(entry) = lines.by_conversation.get_mut(&c)
                            && let Some(job) = entry.job.as_mut()
                            && job.run == run
                            && job.crossed
                            && !job.cancel_sent
                            && let Some(canceller) = entry.canceller.as_ref()
                        {
                            // 带的是占着这条线的这一趟的趟次（2.4「早到的取消认趟」）
                            let tag = job.run;
                            canceller.cancel(turn, tag);
                            job.cancel_sent = true;
                            #[cfg(any(test, feature = "test-hooks"))]
                            self.shared.counters.record_cancel(turn, tag);
                        }
                    }
                    Action::Impossible(why) => return Err(HostError::Impossible(why)),
                    other @ (Action::Settle { .. }
                    | Action::Withdraw { .. }
                    | Action::FinishDelete) => commits.push((w.clone(), other)),
                }
            }
        }
        // 5. 宿主自己的提交，一笔一笔来，至多一个在途
        let mut committed = false;
        for (w, action) in commits {
            match self.commit_action(&w, action).await {
                Ok(true) => committed = true,
                Ok(false) => {}
                Err(()) => return Ok(Next::After(next_backoff(host_backoff))),
            }
        }
        *host_backoff = HOST_BACKOFF_START;
        // 6. 有提交成功、或趟数变了：立刻再对一轮；否则等唤醒、退避到时或保底
        if committed || again {
            return Ok(Next::Now);
        }
        let wait = soonest.map_or(BACKSTOP, |t| t.saturating_duration_since(now).min(BACKSTOP));
        Ok(Next::After(wait))
    }

    /// 宿主直接提交（D5 补账、X2 / E2 ③ 收掉没过边界的回合）。`Ok(true)` 落了盘，`Ok(false)` 没有要做的，
    /// `Err` 没做成（退避后重读重算）。
    async fn commit_action(&self, w: &ConversationWork, action: Action) -> Result<bool, ()> {
        let c = w.conversation.id;
        let deleting = w.conversation.state == ConversationState::Deleting;
        let (turn, job) = match action {
            Action::Settle {
                turn,
                reason,
                cancel_requested,
            } => {
                let Some(open) = w.open_turns.iter().find(|t| t.turn.id == turn) else {
                    return Ok(false);
                };
                let void = if cancel_requested || open.turn.state == TurnState::Cancelling {
                    VoidReason::TurnCancelled
                } else {
                    VoidReason::TurnFailed
                };
                let job = HostCommit::Settle {
                    turn: open.turn.clone(),
                    by: OwedBy::BrokenRun {
                        cancel_requested,
                        reason,
                    },
                    void,
                };
                (turn, job)
            }
            Action::Withdraw { turn, from } => {
                let Some(open) = w.open_turns.iter().find(|t| t.turn.id == turn) else {
                    return Ok(false);
                };
                // 已中断的回合名下可能挂着待审批与放行：经引擎一并作废
                let void = matches!(from, TurnState::Interrupted { .. }).then_some(if deleting {
                    VoidReason::ConversationDeleted
                } else {
                    VoidReason::AssistantDeleted
                });
                let job = HostCommit::Withdraw {
                    turn: open.turn.clone(),
                    void,
                };
                (turn, job)
            }
            // X4 归步 6（删除对话）
            Action::FinishDelete => return Ok(false),
            Action::Hand(_) | Action::Cancel { .. } | Action::Impossible(_) => return Ok(false),
        };
        // 宿主提交这件活至多一份在途（B3）：上一份还没真回来就不发
        let Ok(permit) = self.shared.host_commit.clone().try_acquire_owned() else {
            tracing::error!(%turn, "宿主的上一笔提交还没回来，这一轮不发新的");
            return Err(());
        };
        let ports = self.ports.clone();
        match with_permit(permit, HOST_IO_LIMIT, job.run(ports, c)).await {
            Bounded::Done(Ok(committed)) => Ok(committed),
            Bounded::Done(Err(e)) => {
                tracing::warn!(error = %e, %turn, "宿主的提交没成，重读重算");
                Err(())
            }
            Bounded::Busy | Bounded::TimedOut => {
                tracing::error!(%turn, "宿主的提交 30 秒没回来，这一轮不发新的");
                Err(())
            }
        }
    }
}

/// 宿主自己的一次提交（B3 的一份活）：补账连同它前面那次读，或收掉一个回合。整份交给拿着许可的小任务做。
enum HostCommit {
    /// D5：读这一趟留下的流式消息，算欠的事件，经引擎作废它的审批
    Settle {
        turn: iwork_domain::turn::Turn,
        by: OwedBy,
        void: VoidReason,
    },
    /// X2 / 归属没了：收成已取消；已中断的经引擎作废
    Withdraw {
        turn: iwork_domain::turn::Turn,
        void: Option<VoidReason>,
    },
}

impl HostCommit {
    /// `Ok(true)` 落了盘，`Ok(false)` 没有要提交的。
    async fn run<P: HostPorts>(self, ports: Arc<P>, c: ConversationId) -> Result<bool, String> {
        let (turn, events, scope) = match self {
            HostCommit::Settle { turn, by, void } => {
                let streaming: Vec<_> = ports
                    .read()
                    .unsettled()
                    .await
                    .map_err(|e| format!("补账前读流式消息失败：{e}"))?
                    .streaming
                    .into_iter()
                    .filter(|m| m.turn_id == Some(turn.id))
                    .collect();
                let events = owed_turn_events(&turn, &streaming, &by)
                    .map_err(|e| format!("算补账的事件失败：{e}"))?;
                (turn.id, events, Some(void))
            }
            HostCommit::Withdraw { turn, void } => {
                let events = owed_turn_events(&turn, &[], &OwedBy::Closing)
                    .map_err(|e| format!("算收尾事件失败：{e}"))?;
                (turn.id, events, void)
            }
        };
        if events.is_empty() {
            return Ok(false);
        }
        let req = CommitRequest {
            command_id: CommandId::from_raw(ports.ids().next()),
            actor: Actor(HOST_ACTOR.into()),
            events,
            outbox: Vec::new(),
        };
        match scope {
            Some(reason) => ports
                .engine()
                .commit_with(
                    Scope::VoidTurn {
                        conversation: c,
                        turn,
                        reason,
                    },
                    req,
                )
                .await
                .map(|_| true)
                .map_err(|e| e.to_string()),
            None => ports
                .store()
                .commit(req)
                .await
                .map(|_| true)
                .map_err(|e| e.to_string()),
        }
    }
}

/// 宿主自己提交时的 actor。
pub const HOST_ACTOR: &str = "system:turn-host";

fn next_backoff(cur: &mut Duration) -> Duration {
    let wait = *cur;
    *cur = (*cur * 2).min(HOST_BACKOFF_MAX);
    wait
}

impl<P: HostPorts> crate::handler::dispatch::HostState for TurnHost<P> {
    fn is_closed(&self) -> bool {
        TurnHost::is_closed(self)
    }
}
