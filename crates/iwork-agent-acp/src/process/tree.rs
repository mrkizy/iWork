//! Unix 上的进程树（T06 方案 5.4a）：先找后杀，范围由 [`Scope`] 一处定义。
//!
//! - **记**：采样每 2 秒、取消一开始、清理的每一步，各扫一遍进程表交给 [`Scope::observe`]。桥接一起来就是记到过的
//!   进程（进程号与出生时间在起它之后立刻读）。
//! - **清**（[`ProcessTree::clear`]）：① 逐个 SIGSTOP 到范围不再长；② 从叶子往根逐个 SIGKILL，每个受管会话
//!   留一个停着的见证；③ 每 [`TREE_SCAN_POLL`] 再扫再杀，某个会话连续两遍只剩见证就杀掉见证、会话作废；
//!   范围里没有活成员、桥接自己也已退出才返回 `true`。扫描没做成不当成空，接着扫到期限。不对任何进程组整组发信号。
//! - **发信号**：只经 [`signal_member`]，按（进程号，出生时间）认人（「发信号按身份」）。
//! - **回收**：桥接退出用 `waitid(WEXITED | WNOWAIT)` 等（不回收，它还占着进程号，根组号不会被别人拿走）。
//!   `clear()` 判定清空之后当场回收，**拿到了退出状态**才让根组作废；拿不到就改判 `false`。`clear()` 返回
//!   `false` 时什么都不放：不回收、根组有效、范围原样留着、句柄留在这里，下次再清的是同一个范围（审查 S6-5 1/F-3）。
//!   后台里没有回收任意子进程的代码。

use std::collections::{BTreeMap, BTreeSet};
use std::io;
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use tokio::process::Child;
use tokio::sync::watch;
use tokio::time::Instant;

use super::scan;
use super::scope::{Birth, Member, Members, Pid, Run, Scope};
use super::signal::{Ident, signal_member};
use super::{ExitInfo, ResourceSample, TREE_CLEAR_LIMIT, Tick};

/// 冻结最多扫几遍（5.4a 第 3 条第 1 步）。
pub const FREEZE_ROUNDS: usize = 8;
/// 杀完之后每隔多久再扫一遍（第 3 步）。
pub const TREE_SCAN_POLL: Duration = Duration::from_millis(50);
/// 一个会话连续几遍只剩见证，才杀见证。
const QUIET_SCANS: u32 = 2;

/// 测试用的闸门：`clear()` 走到这些点时停下，等测试放行（真实进程的交错测试，5.4a「由什么保证」）。
/// 非测试构建里不停。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GateStep {
    /// 冻结的第 n 遍扫描之前
    Freeze(usize),
    /// 冻结的第 n 遍扫描之后、发 SIGSTOP 之前
    BeforeStop(usize),
    /// 杀的一遍扫描之前
    Kill,
    /// 判定之后、回收之前
    BeforeReap,
}

#[cfg(test)]
pub type Gate = tokio::sync::mpsc::UnboundedSender<(GateStep, tokio::sync::oneshot::Sender<()>)>;

/// 测试用的读表故障注入。
#[cfg(test)]
#[derive(Debug, Default)]
struct Faults {
    /// 之后的扫描都失败
    fail: bool,
    /// 下一次读表先睡这么久（只一次）
    delay_once: Option<Duration>,
    /// 下一批「复核 + 发送」先睡这么久（只一次）：代表复核时读出生时间卡住
    signal_delay_once: Option<Duration>,
    /// 每一遍扫描提交之前，先有另一遍提交落下（这一遍因此被超过去）
    overtake: bool,
    /// 下一遍扫描读完表、提交之前：先报「到了」，再等放行
    pause_commit: Option<(std::sync::mpsc::Sender<()>, std::sync::mpsc::Receiver<()>)>,
    /// 下一遍扫描读表时崩掉
    panic_once: bool,
}

#[derive(Debug, Clone)]
pub struct ProcessTree {
    root: Pid,
    /// 桥接的出生时间（起它之后立刻读）；读不到为 `None`，那时不给它发信号
    root_birth: Option<Birth>,
    scope: Arc<Mutex<Scope>>,
    /// 同一时刻只有一个 `clear()` 在跑
    clearing: Arc<tokio::sync::Mutex<()>>,
    /// 桥接的子进程句柄：`clear()` 成功并当场回收之后为 `None`
    child: Arc<Mutex<Option<Child>>>,
    #[cfg(test)]
    gate: Arc<Mutex<Option<Gate>>>,
    #[cfg(test)]
    faults: Arc<Mutex<Faults>>,
}

fn lock<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(PoisonError::into_inner)
}

impl ProcessTree {
    /// 接管刚起来的桥接：记下根组、根会话与桥接的身份，起一个线程不回收地等它退出，退出说明发进 `exit`。失败时
    /// 把子进程句柄交回，由调用方结束并回收。
    pub fn adopt(
        child: Child,
        exit: watch::Sender<Option<ExitInfo>>,
    ) -> Result<Self, Box<(io::Error, Child)>> {
        let Some(root) = child.id().and_then(|p| Pid::try_from(p).ok()) else {
            return Err(Box::new((
                io::Error::other("运行时进程刚启动就已退出"),
                child,
            )));
        };
        // 这时后台持有它（没回收），号码不会换人
        let root_birth = scan::birth_of(root);
        // SAFETY: getsid 只读一个整数参数
        let root_sid = unsafe { libc::getsid(root) };
        if root_sid <= 0 {
            return Err(Box::new((io::Error::last_os_error(), child)));
        }
        let tree = Self {
            root,
            root_birth,
            scope: Arc::new(Mutex::new(Scope::new(root, root_birth, root_sid))),
            clearing: Arc::new(tokio::sync::Mutex::new(())),
            child: Arc::new(Mutex::new(Some(child))),
            #[cfg(test)]
            gate: Arc::new(Mutex::new(None)),
            #[cfg(test)]
            faults: Arc::new(Mutex::new(Faults::default())),
        };
        let scope = tree.scope.clone();
        let watching = std::thread::Builder::new()
            .name("iwork-exit-watch".into())
            .spawn(move || {
                let description = wait_exit_no_reap(root, root_birth, &scope);
                let _ = exit.send(Some(ExitInfo { description }));
            });
        if let Err(e) = watching {
            let child = lock(&tree.child).take().expect("刚放进去");
            return Err(Box::new((e, child)));
        }
        Ok(tree)
    }

    #[cfg(test)]
    pub fn set_gate(&self, gate: Gate) {
        *lock(&self.gate) = Some(gate);
    }

    /// 测试用：让之后的扫描都失败（`false` 恢复）。
    #[cfg(test)]
    pub fn fail_scans(&self, fail: bool) {
        lock(&self.faults).fail = fail;
    }

    /// 测试用：下一次读表先睡 `d`（只一次）。
    #[cfg(test)]
    pub fn delay_next_scan(&self, d: Duration) {
        lock(&self.faults).delay_once = Some(d);
    }

    /// 测试用：之后每一遍扫描提交之前都先被别的提交超过去（`false` 恢复）。
    #[cfg(test)]
    pub fn overtake_scans(&self, on: bool) {
        lock(&self.faults).overtake = on;
    }

    /// 测试用：下一遍扫描读完表之后停在提交之前；返回（到了的通知，放行）。
    #[cfg(test)]
    pub fn pause_next_commit(
        &self,
    ) -> (std::sync::mpsc::Receiver<()>, std::sync::mpsc::Sender<()>) {
        let (reached_tx, reached_rx) = std::sync::mpsc::channel();
        let (go_tx, go_rx) = std::sync::mpsc::channel();
        lock(&self.faults).pause_commit = Some((reached_tx, go_rx));
        (reached_rx, go_tx)
    }

    /// 测试用：下一遍扫描读表时崩掉。
    #[cfg(test)]
    pub fn panic_next_scan(&self) {
        lock(&self.faults).panic_once = true;
    }

    /// 测试用：在飞的扫描有几遍。
    #[cfg(test)]
    pub fn in_flight(&self) -> usize {
        lock(&self.scope).in_flight()
    }

    /// 测试用：在当前线程上做一遍扫描（停在提交之前的那一遍用它在别的线程里跑）。
    #[cfg(test)]
    pub fn scan_now(&self) -> io::Result<Members> {
        self.observe_blocking()
    }

    /// 测试用：下一批「复核 + 发送」先睡 `d`（只一次）。
    #[cfg(test)]
    pub fn delay_next_signals(&self, d: Duration) {
        lock(&self.faults).signal_delay_once = Some(d);
    }

    /// 测试用：范围的一份拷贝。
    #[cfg(test)]
    pub fn scope_copy(&self) -> Scope {
        lock(&self.scope).clone()
    }

    /// 测试用：句柄还在不在（没回收）。
    #[cfg(test)]
    pub fn holds_child(&self) -> bool {
        lock(&self.child).is_some()
    }

    async fn gate(&self, step: GateStep) {
        #[cfg(test)]
        {
            let gate = lock(&self.gate).clone();
            if let Some(gate) = gate {
                let (tx, rx) = tokio::sync::oneshot::channel();
                if gate.send((step, tx)).is_ok() {
                    let _ = rx.await;
                }
            }
        }
        #[cfg(not(test))]
        let _ = step;
    }

    /// 扫一遍、复核见证、提交到范围。阻塞的系统调用，调用方放在阻塞线程里。扫描失败（列不出进程表、读出说不清的
    /// 错误）时报错，**范围不动**：已记下的身份一个不丢，之后可以重试（审查 S6-5 1/F-4）。几遍扫描可以同时在跑
    /// （采样、清理、上一次放弃的清理留下的迟到扫描）：提交按代次（[`Scope::commit`]），被超过去的那一遍只加不删、
    /// 返回的成员标成旧的（审查 S6-5 2/F-1）。没有哪一遍等别的扫描。
    fn observe_blocking(&self) -> io::Result<Members> {
        // 次序固定（方案 5.4a「扫描怎么提交到范围」）：锁里取快照、在飞的数加一 → 放锁读表 → 复核快照里的见证 → 算 →
        // 拿锁提交，同一把锁里看还有没有别的在飞，再减一。报错、崩掉时由守卫减
        let (snapshot, mut flight) = {
            let mut scope = lock(&self.scope);
            (scope.begin_scan(), InFlight::new(&self.scope))
        };
        #[cfg(test)]
        {
            let (delay, panic) = {
                let mut f = lock(&self.faults);
                (f.delay_once.take(), std::mem::take(&mut f.panic_once))
            };
            if let Some(d) = delay {
                std::thread::sleep(d);
            }
            assert!(!panic, "测试注入的扫描崩溃");
            if lock(&self.faults).fail {
                return Err(io::Error::other("测试注入的扫描失败"));
            }
        }
        let table = scan::read_table()?;
        let recheck = scan::recheck(&snapshot.witness_pids())?;
        let pass = Scope::compute(&snapshot, &table, &recheck);
        #[cfg(test)]
        {
            let pause = lock(&self.faults).pause_commit.take();
            if let Some((reached, go)) = pause {
                let _ = reached.send(());
                let _ = go.recv();
            }
        }
        let mut scope = lock(&self.scope);
        #[cfg(test)]
        if lock(&self.faults).overtake {
            let other = Scope::compute(&scope.clone(), &table, &recheck);
            scope.commit(other);
        }
        let members = scope.commit(pass);
        scope.end_scan();
        flight.done();
        Ok(members)
    }

    /// 记一次范围（采样、取消一开始）。最多等 [`TREE_CLEAR_LIMIT`]。
    pub async fn record(&self) -> Option<Members> {
        self.observe_until(Instant::now() + TREE_CLEAR_LIMIT).await
    }

    async fn observe_until(&self, deadline: Instant) -> Option<Members> {
        let tree = self.clone();
        let scan = tokio::task::spawn_blocking(move || tree.observe_blocking());
        match tokio::time::timeout_at(deadline, scan).await {
            Ok(Ok(Ok(m))) if !m.stale => Some(m),
            Ok(Ok(Ok(_))) => {
                tracing::debug!(root = self.root, "这一遍被别的扫描超过去了，不拿来下结论");
                None
            }
            Ok(Ok(Err(e))) => {
                tracing::warn!(root = self.root, error = %e, "扫描进程表失败，这一遍不算");
                None
            }
            // 扫描超时或线程出错：这一遍不算，调用方按「没做成」处理
            Ok(Err(_)) | Err(_) => None,
        }
    }

    /// 同步地看一眼范围里还有没有活成员（T04 的 `tree_is_empty`、会话的 `process_tree_is_empty`）。扫描没做成时
    /// 答「不空」。
    pub fn is_empty(&self) -> bool {
        match self.observe_blocking() {
            Ok(m) => !m.stale && !m.busy && m.is_empty(),
            Err(e) => {
                tracing::warn!(root = self.root, error = %e, "扫描进程表失败，按不空答");
                false
            }
        }
    }

    /// 采样：记一次范围，并对范围里的成员求内存与 CPU 合计。范围里没有活成员时 [`Tick::Done`]；扫描失败时
    /// [`Tick::Skip`]（这一次不采，不算树空了）。
    pub fn sample(&self, system: &mut sysinfo::System) -> Tick {
        use sysinfo::{Pid as SPid, ProcessRefreshKind, ProcessesToUpdate};
        // 清理进行时不另起扫描：clear() 自己每 50 毫秒扫一遍，范围不缺记录；少一个老和它撞的（方案 5.4a 第 4a 条）
        if self.clearing.try_lock().is_err() {
            return Tick::Skip;
        }
        let members = match self.observe_blocking() {
            Ok(m) => m,
            Err(e) => {
                tracing::warn!(root = self.root, error = %e, "采样时扫描进程表失败，这一次不采");
                return Tick::Skip;
            }
        };
        // 只有一遍不旧、提交时没有别的在飞的空结果才算树没了；旧的空结果跳过这一拍（审查 S6-5 3/F-3）
        if members.is_empty() {
            return if members.stale || members.busy {
                Tick::Skip
            } else {
                Tick::Done
            };
        }
        let pids: Vec<SPid> = members
            .list
            .iter()
            .map(|m| SPid::from_u32(m.pid as u32))
            .collect();
        let kind = ProcessRefreshKind::nothing().with_memory().with_cpu();
        system.refresh_processes_specifics(ProcessesToUpdate::Some(&pids), true, kind);
        let live: Vec<_> = pids.iter().filter_map(|p| system.process(*p)).collect();
        Tick::Sample(ResourceSample {
            rss_bytes: live.iter().map(|p| p.memory()).sum(),
            cpu_percent: live.iter().map(|p| p.cpu_usage()).sum(),
            processes: members.list.len(),
        })
    }

    /// 在阻塞线程里做 `f`，最多等到 `deadline`；到时没做完返回 `None`（结果丢掉）。复核出生时间、看桥接退没退、
    /// 回收都是同步的系统调用，不在异步线程上做，期限才管得到（审查 S6-5 2/F-2）。
    async fn blocking<T: Send + 'static>(
        &self,
        deadline: Instant,
        f: impl FnOnce(&Self) -> T + Send + 'static,
    ) -> Option<T> {
        let tree = self.clone();
        let task = tokio::task::spawn_blocking(move || f(&tree));
        match tokio::time::timeout_at(deadline, task).await {
            Ok(Ok(v)) => Some(v),
            Ok(Err(_)) | Err(_) => None,
        }
    }

    /// 一批「复核 + 发送」：每个成员的复核与发送在同一个阻塞任务里挨着做，中间不让出。每发一个之前看一眼期限，
    /// 到了就不再发：任务迟到了（调用侧已经不等它）也不会再发出信号。返回这一批在期限内发完了没有。
    async fn send_batch(&self, who: Vec<Ident>, sig: libc::c_int, deadline: Instant) -> bool {
        let until = deadline.into_std();
        #[cfg(test)]
        let delay = lock(&self.faults).signal_delay_once.take();
        self.blocking(deadline, move |_| {
            #[cfg(test)]
            if let Some(d) = delay {
                std::thread::sleep(d);
            }
            for w in who {
                if std::time::Instant::now() >= until {
                    return false;
                }
                signal_member(w, sig, Some(until));
            }
            true
        })
        .await
        .unwrap_or(false)
    }

    /// 给桥接本身发信号（`shutdown` 请它退出）。只在它还归我们时发：句柄还在（没回收；回收要拿同一把锁）、根组没
    /// 作废，并且按身份发（审查 S6-5 1/F-2）。复核与发送在阻塞线程里做，最多等 [`TREE_CLEAR_LIMIT`]。返回发了没有。
    pub async fn signal_root(&self, sig: libc::c_int) -> bool {
        let deadline = Instant::now() + TREE_CLEAR_LIMIT;
        let until = deadline.into_std();
        self.blocking(deadline, move |t| {
            let child = lock(&t.child);
            if child.is_none() || !lock(&t.scope).root_valid() || std::time::Instant::now() >= until
            {
                return false;
            }
            let Some(birth) = t.root_birth else {
                return false;
            };
            signal_member(Ident { pid: t.root, birth }, sig, Some(until))
        })
        .await
        .unwrap_or(false)
    }

    /// 清掉范围里的全部进程并确认，最多 [`TREE_CLEAR_LIMIT`]。`true` 之后桥接已被回收、根组作废；`false` 时什么都
    /// 不放，可以再调。
    pub async fn clear(&self) -> bool {
        let _one = self.clearing.lock().await;
        let deadline = Instant::now() + TREE_CLEAR_LIMIT;
        let cleared = self.clear_until(deadline).await;
        self.gate(GateStep::BeforeReap).await;
        // 确认清空之后当场回收：拿到了退出状态才算，拿不到（含到了期限）改判没清成
        let cleared = cleared
            && self
                .blocking(deadline, |t| t.reap_now())
                .await
                .unwrap_or(false);
        tracing::debug!(root = self.root, cleared, "clear");
        cleared
    }

    async fn clear_until(&self, deadline: Instant) -> bool {
        // ① 冻到不再长。扫描没做成：这一遍不算，期限没到就接着来
        let mut seen: Option<BTreeSet<Pid>> = None;
        for round in 0..FREEZE_ROUNDS {
            self.gate(GateStep::Freeze(round)).await;
            let Some(m) = self.observe_until(deadline).await else {
                if Instant::now() >= deadline {
                    return false;
                }
                tokio::time::sleep(TREE_SCAN_POLL).await;
                continue;
            };
            let pids = m.pids();
            let running: Vec<Ident> = m
                .list
                .iter()
                .filter(|x| x.run == Run::Running)
                .filter_map(ident)
                .collect();
            if running.is_empty() && seen.as_ref().is_some_and(|s| s.is_superset(&pids)) {
                break;
            }
            self.gate(GateStep::BeforeStop(round)).await;
            if !self.send_batch(running, libc::SIGSTOP, deadline).await {
                return false;
            }
            seen = Some(pids);
        }
        // ② 从叶子往根杀，每个会话留一个见证；③ 杀到确认为空
        let mut quiet: BTreeMap<Pid, u32> = BTreeMap::new();
        loop {
            self.gate(GateStep::Kill).await;
            if let Some(m) = self.observe_until(deadline).await {
                // 下「清空了」的结论：这一遍不旧（observe_until 已经滤掉）、提交时没有别的扫描在飞、桥接自己已退出
                if m.is_empty()
                    && !m.busy
                    && self
                        .blocking(deadline, |t| t.root_exited())
                        .await
                        .unwrap_or(false)
                {
                    return true;
                }
                let witnesses = lock(&self.scope).witnesses(&m);
                let mut release: BTreeSet<Pid> = BTreeSet::new();
                for (sid, w) in &witnesses {
                    let others = m.list.iter().any(|x| x.sid == Some(*sid) && x.pid != *w);
                    let n = quiet.entry(*sid).or_default();
                    *n = if others { 0 } else { *n + 1 };
                    if *n >= QUIET_SCANS {
                        release.insert(*sid);
                    }
                }
                let keep: BTreeSet<Pid> = witnesses
                    .iter()
                    .filter(|(sid, _)| !release.contains(sid))
                    .map(|(_, w)| *w)
                    .collect();
                let victims: Vec<Ident> = m
                    .list
                    .iter()
                    .filter(|x| !keep.contains(&x.pid))
                    .filter_map(ident)
                    .collect();
                if !self.send_batch(victims, libc::SIGKILL, deadline).await {
                    return false;
                }
            }
            if Instant::now() >= deadline {
                return false;
            }
            tokio::time::sleep(TREE_SCAN_POLL).await;
        }
    }

    /// 桥接自己已经退出了没有（不回收地看一眼）。句柄已经交出去（回收过）算退出。同步的系统调用：只在阻塞线程里调。
    fn root_exited(&self) -> bool {
        if lock(&self.child).is_none() {
            return true;
        }
        // SAFETY: 全零是 siginfo_t 的合法初值；指针指向本地变量
        let mut info: libc::siginfo_t = unsafe { std::mem::zeroed() };
        let rc = unsafe {
            libc::waitid(
                libc::P_PID,
                self.root as libc::id_t,
                &mut info,
                libc::WEXITED | libc::WNOHANG | libc::WNOWAIT,
            )
        };
        if rc == 0 {
            // SAFETY: waitid 成功时 si_pid 有效；还没退出时为 0
            return unsafe { info.si_pid() } != 0;
        }
        // ECHILD：被别处回收了。按身份看它还在不在
        self.root_gone_by_identity()
    }

    /// 桥接这个进程按身份确实不在了（读不到出生时间，或者号上换了人）。
    fn root_gone_by_identity(&self) -> bool {
        match self.root_birth {
            Some(birth) => scan::birth_of(self.root) != Some(birth),
            // 起来时就没读到身份：没法确认它不在，当它还在
            None => false,
        }
    }

    /// 当场回收桥接（同步，只在阻塞线程里调）：拿到了退出状态才让根组作废并返回 `true`；它还没退、或者回收失败而它按身份还在 → 句柄留着，
    /// 根组照旧有效，返回 `false`。
    fn reap_now(&self) -> bool {
        let mut slot = lock(&self.child);
        let Some(child) = slot.as_mut() else {
            return true;
        };
        match child.try_wait() {
            Ok(Some(_)) => {
                *slot = None;
                lock(&self.scope).mark_reaped();
                true
            }
            Ok(None) => false,
            Err(e) => {
                if self.root_gone_by_identity() {
                    *slot = None;
                    lock(&self.scope).mark_reaped();
                    true
                } else {
                    tracing::error!(root = self.root, error = %e, "回收桥接失败、它按身份还在：根组不作废，这次清理算没清成");
                    false
                }
            }
        }
    }

    /// 不能等的地方（丢弃时）用：有运行时就在后台清；没有就同步扫一遍、全杀一遍，尽力而为。
    pub fn kill_detached(&self) {
        match tokio::runtime::Handle::try_current() {
            Ok(rt) => {
                let tree = self.clone();
                rt.spawn(async move {
                    tree.clear().await;
                });
            }
            Err(_) => {
                if let Ok(members) = self.observe_blocking() {
                    for who in members.list.iter().filter_map(ident) {
                        signal_member(who, libc::SIGKILL, None);
                    }
                }
            }
        }
    }
}

/// 一遍在飞的扫描：离开作用域时（报错、崩掉）把在飞的数减一；正常提交时在提交的那把锁里已经减过，`done()`。
struct InFlight<'a> {
    scope: &'a Mutex<Scope>,
    armed: bool,
}

impl<'a> InFlight<'a> {
    fn new(scope: &'a Mutex<Scope>) -> Self {
        Self { scope, armed: true }
    }

    fn done(&mut self) {
        self.armed = false;
    }
}

impl Drop for InFlight<'_> {
    fn drop(&mut self) {
        if self.armed {
            lock(self.scope).end_scan();
        }
    }
}

/// 成员的身份；这一遍没读到出生时间的不发信号，留给下一遍。
fn ident(m: &Member) -> Option<Ident> {
    Some(Ident {
        pid: m.pid,
        birth: m.birth?,
    })
}

/// 不回收地等子进程退出，返回退出说明。等的时候得到 `ECHILD`（被别处回收了）：按身份看桥接还在不在，不在了才让
/// 根组作废；还在就不作废，记一条错误（方案 5.4a「根组也有有效期」）。
pub(super) fn wait_exit_no_reap(pid: Pid, birth: Option<Birth>, scope: &Mutex<Scope>) -> String {
    use std::os::unix::process::ExitStatusExt;
    loop {
        // SAFETY: 全零是 siginfo_t 的合法初值；指针指向本地变量
        let mut info: libc::siginfo_t = unsafe { std::mem::zeroed() };
        let rc = unsafe {
            libc::waitid(
                libc::P_PID,
                pid as libc::id_t,
                &mut info,
                libc::WEXITED | libc::WNOWAIT,
            )
        };
        if rc == 0 {
            // SAFETY: waitid 成功时 si_status 有效
            let status = unsafe { info.si_status() };
            let raw = match info.si_code {
                libc::CLD_EXITED => (status & 0xff) << 8,
                libc::CLD_DUMPED => status | 0x80,
                _ => status,
            };
            return std::process::ExitStatus::from_raw(raw).to_string();
        }
        let err = io::Error::last_os_error();
        match err.raw_os_error() {
            Some(libc::EINTR) => continue,
            Some(libc::ECHILD) => {
                let alive = birth.is_some_and(|b| scan::birth_of(pid) == Some(b));
                if alive {
                    tracing::error!(pid, "等桥接退出时得到 ECHILD，但它按身份还在：根组不作废");
                    return "等待进程退出失败（ECHILD，进程还在）".into();
                }
                lock(scope).mark_reaped();
                return "进程已被别处回收".into();
            }
            _ => return format!("等待进程退出失败：{err}"),
        }
    }
}
