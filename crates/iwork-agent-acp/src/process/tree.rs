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

    /// 扫一遍、复核见证、交给范围。阻塞的系统调用，调用方放在阻塞线程里。扫描失败（列不出进程表、读出说不清的
    /// 错误）时报错，**范围不动**：已记下的身份一个不丢，之后可以重试（审查 S6-5 1/F-4）。迟到的扫描（它所属的
    /// `clear()` 已经放弃）照常交给范围：它只会加记到的进程或照见证规则作废会话，碰不到根组。
    fn observe_blocking(&self) -> io::Result<Members> {
        #[cfg(test)]
        {
            let delay = lock(&self.faults).delay_once.take();
            if let Some(d) = delay {
                std::thread::sleep(d);
            }
            if lock(&self.faults).fail {
                return Err(io::Error::other("测试注入的扫描失败"));
            }
        }
        let table = scan::read_table()?;
        let witnesses = lock(&self.scope).witness_pids();
        let recheck = scan::recheck(&witnesses)?;
        Ok(lock(&self.scope).observe(&table, &recheck))
    }

    /// 记一次范围（采样、取消一开始）。最多等 [`TREE_CLEAR_LIMIT`]。
    pub async fn record(&self) -> Option<Members> {
        self.observe_until(Instant::now() + TREE_CLEAR_LIMIT).await
    }

    async fn observe_until(&self, deadline: Instant) -> Option<Members> {
        let tree = self.clone();
        let scan = tokio::task::spawn_blocking(move || tree.observe_blocking());
        match tokio::time::timeout_at(deadline, scan).await {
            Ok(Ok(Ok(m))) => Some(m),
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
            Ok(m) => m.is_empty(),
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
        let members = match self.observe_blocking() {
            Ok(m) => m,
            Err(e) => {
                tracing::warn!(root = self.root, error = %e, "采样时扫描进程表失败，这一次不采");
                return Tick::Skip;
            }
        };
        if members.is_empty() {
            return Tick::Done;
        }
        let pids: Vec<SPid> = members
            .0
            .iter()
            .map(|m| SPid::from_u32(m.pid as u32))
            .collect();
        let kind = ProcessRefreshKind::nothing().with_memory().with_cpu();
        system.refresh_processes_specifics(ProcessesToUpdate::Some(&pids), true, kind);
        let live: Vec<_> = pids.iter().filter_map(|p| system.process(*p)).collect();
        Tick::Sample(ResourceSample {
            rss_bytes: live.iter().map(|p| p.memory()).sum(),
            cpu_percent: live.iter().map(|p| p.cpu_usage()).sum(),
            processes: members.0.len(),
        })
    }

    /// 给桥接本身发信号（`shutdown` 请它退出）。只在它还归我们时发：句柄还在（没回收；回收要拿同一把锁）、根组没
    /// 作废，并且按身份发（审查 S6-5 1/F-2）。返回发了没有。
    pub fn signal_root(&self, sig: libc::c_int) -> bool {
        let child = lock(&self.child);
        if child.is_none() || !lock(&self.scope).root_valid() {
            return false;
        }
        let Some(birth) = self.root_birth else {
            return false;
        };
        signal_member(
            Ident {
                pid: self.root,
                birth,
            },
            sig,
        )
    }

    /// 清掉范围里的全部进程并确认，最多 [`TREE_CLEAR_LIMIT`]。`true` 之后桥接已被回收、根组作废；`false` 时什么都
    /// 不放，可以再调。
    pub async fn clear(&self) -> bool {
        let _one = self.clearing.lock().await;
        let deadline = Instant::now() + TREE_CLEAR_LIMIT;
        let cleared = self.clear_until(deadline).await;
        self.gate(GateStep::BeforeReap).await;
        // 确认清空之后当场回收：拿到了退出状态才算，拿不到改判没清成
        let cleared = cleared && self.reap_now();
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
            let running: Vec<Ident> =
                m.0.iter()
                    .filter(|x| x.run == Run::Running)
                    .filter_map(ident)
                    .collect();
            if running.is_empty() && seen.as_ref().is_some_and(|s| s.is_superset(&pids)) {
                break;
            }
            self.gate(GateStep::BeforeStop(round)).await;
            for who in running {
                signal_member(who, libc::SIGSTOP);
            }
            seen = Some(pids);
        }
        // ② 从叶子往根杀，每个会话留一个见证；③ 杀到确认为空
        let mut quiet: BTreeMap<Pid, u32> = BTreeMap::new();
        loop {
            self.gate(GateStep::Kill).await;
            if let Some(m) = self.observe_until(deadline).await {
                if m.is_empty() && self.root_exited() {
                    return true;
                }
                let witnesses = lock(&self.scope).witnesses(&m);
                let mut release: BTreeSet<Pid> = BTreeSet::new();
                for (sid, w) in &witnesses {
                    let others = m.0.iter().any(|x| x.sid == Some(*sid) && x.pid != *w);
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
                for who in
                    m.0.iter()
                        .filter(|x| !keep.contains(&x.pid))
                        .filter_map(ident)
                {
                    signal_member(who, libc::SIGKILL);
                }
                let mut scope = lock(&self.scope);
                for sid in release {
                    scope.drop_session(sid);
                }
            }
            if Instant::now() >= deadline {
                return false;
            }
            tokio::time::sleep(TREE_SCAN_POLL).await;
        }
    }

    /// 桥接自己已经退出了没有（不回收地看一眼）。句柄已经交出去（回收过）算退出。
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

    /// 当场回收桥接：拿到了退出状态才让根组作废并返回 `true`；它还没退、或者回收失败而它按身份还在 → 句柄留着，
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
                    for who in members.0.iter().filter_map(ident) {
                        signal_member(who, libc::SIGKILL);
                    }
                }
            }
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
