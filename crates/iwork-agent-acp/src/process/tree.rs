//! Unix 上的进程树（T06 方案 5.4a）：先找后杀，范围由 [`Scope`] 一处定义。
//!
//! - **记**：采样每 2 秒、取消一开始、清理的每一步，各扫一遍进程表交给 [`Scope::observe`]。
//! - **清**（[`ProcessTree::clear`]）：① 逐个 SIGSTOP 到范围不再长；② 从叶子往根逐个 SIGKILL，每个受管会话
//!   留一个停着的见证；③ 每 [`TREE_SCAN_POLL`] 再扫再杀，某个会话连续两遍只剩见证就杀掉见证、会话作废；
//!   范围里没有活成员才返回 `true`。不对任何进程组整组发信号。
//! - **回收**：桥接退出用 `waitid(WEXITED | WNOWAIT)` 等（不回收，它还占着进程号，根组号不会被别人拿走），
//!   `clear()` 返回之后才回收，回收之前先让根组作废。后台里没有回收任意子进程的代码。

use std::collections::{BTreeMap, BTreeSet};
use std::io;
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use tokio::process::Child;
use tokio::sync::watch;
use tokio::time::Instant;

use super::scan;
use super::scope::{Members, Pid, Run, Scope};
use super::{ExitInfo, ResourceSample, TREE_CLEAR_LIMIT};

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
    /// 返回之后、回收之前
    BeforeReap,
}

#[cfg(test)]
pub type Gate = tokio::sync::mpsc::UnboundedSender<(GateStep, tokio::sync::oneshot::Sender<()>)>;

#[derive(Debug, Clone)]
pub struct ProcessTree {
    root: Pid,
    scope: Arc<Mutex<Scope>>,
    /// 同一时刻只有一个 `clear()` 在跑
    clearing: Arc<tokio::sync::Mutex<()>>,
    /// 桥接的子进程句柄：清完才回收，之后为 `None`
    child: Arc<Mutex<Option<Child>>>,
    #[cfg(test)]
    gate: Arc<Mutex<Option<Gate>>>,
}

fn lock<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(PoisonError::into_inner)
}

impl ProcessTree {
    /// 接管刚起来的桥接：记下根组与根会话，起一个线程不回收地等它退出，退出说明发进 `exit`。失败时把子进程
    /// 句柄交回，由调用方结束并回收。
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
        // SAFETY: getsid 只读一个整数参数
        let root_sid = unsafe { libc::getsid(root) };
        if root_sid <= 0 {
            return Err(Box::new((io::Error::last_os_error(), child)));
        }
        let tree = Self {
            root,
            scope: Arc::new(Mutex::new(Scope::new(root, root_sid))),
            clearing: Arc::new(tokio::sync::Mutex::new(())),
            child: Arc::new(Mutex::new(Some(child))),
            #[cfg(test)]
            gate: Arc::new(Mutex::new(None)),
        };
        let scope = tree.scope.clone();
        let watching = std::thread::Builder::new()
            .name("iwork-exit-watch".into())
            .spawn(move || {
                let description = wait_exit_no_reap(root, &scope);
                let _ = exit.send(Some(ExitInfo { description }));
            });
        if let Err(e) = watching {
            let child = lock(&tree.child).take().expect("刚放进去");
            return Err(Box::new((e, child)));
        }
        Ok(tree)
    }

    pub fn root(&self) -> Pid {
        self.root
    }

    #[cfg(test)]
    pub fn set_gate(&self, gate: Gate) {
        *lock(&self.gate) = Some(gate);
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

    /// 扫一遍、复核见证、交给范围。阻塞的系统调用放在阻塞线程里。
    fn observe_blocking(scope: &Mutex<Scope>) -> Members {
        let table = scan::read_table();
        let witnesses = lock(scope).witness_pids();
        let recheck = scan::recheck(&witnesses);
        lock(scope).observe(&table, &recheck)
    }

    /// 记一次范围（采样、取消一开始）。最多等到 `deadline`。
    pub async fn record(&self) -> Option<Members> {
        self.observe_until(Instant::now() + TREE_CLEAR_LIMIT).await
    }

    async fn observe_until(&self, deadline: Instant) -> Option<Members> {
        let scope = self.scope.clone();
        let scan = tokio::task::spawn_blocking(move || Self::observe_blocking(&scope));
        match tokio::time::timeout_at(deadline, scan).await {
            Ok(Ok(m)) => Some(m),
            // 扫描超时或线程出错：这一遍不算，调用方按「没确认」处理
            Ok(Err(_)) | Err(_) => None,
        }
    }

    /// 同步地看一眼范围里还有没有活成员（T04 的 `tree_is_empty`、会话的 `process_tree_is_empty`）。
    pub fn is_empty(&self) -> bool {
        Self::observe_blocking(&self.scope).is_empty()
    }

    /// 采样：记一次范围，并对范围里的成员求内存与 CPU 合计。范围里没有活成员时返回 `None`。
    pub fn sample(&self, system: &mut sysinfo::System) -> Option<ResourceSample> {
        use sysinfo::{Pid as SPid, ProcessRefreshKind, ProcessesToUpdate};
        let members = Self::observe_blocking(&self.scope);
        if members.is_empty() {
            return None;
        }
        let pids: Vec<SPid> = members
            .0
            .iter()
            .map(|m| SPid::from_u32(m.pid as u32))
            .collect();
        let kind = ProcessRefreshKind::nothing().with_memory().with_cpu();
        system.refresh_processes_specifics(ProcessesToUpdate::Some(&pids), true, kind);
        let live: Vec<_> = pids.iter().filter_map(|p| system.process(*p)).collect();
        Some(ResourceSample {
            rss_bytes: live.iter().map(|p| p.memory()).sum(),
            cpu_percent: live.iter().map(|p| p.cpu_usage()).sum(),
            processes: members.0.len(),
        })
    }

    /// 清掉范围里的全部进程并确认，最多 [`TREE_CLEAR_LIMIT`]。返回之后回收桥接（根组从此作废）。
    pub async fn clear(&self) -> bool {
        let _one = self.clearing.lock().await;
        let deadline = Instant::now() + TREE_CLEAR_LIMIT;
        let cleared = self.clear_until(deadline).await;
        tracing::debug!(root = self.root, cleared, "clear");
        self.gate(GateStep::BeforeReap).await;
        self.reap();
        cleared
    }

    async fn clear_until(&self, deadline: Instant) -> bool {
        // ① 冻到不再长
        let mut seen: Option<BTreeSet<Pid>> = None;
        for round in 0..FREEZE_ROUNDS {
            self.gate(GateStep::Freeze(round)).await;
            let Some(m) = self.observe_until(deadline).await else {
                return false;
            };
            let pids = m.pids();
            let running: Vec<Pid> =
                m.0.iter()
                    .filter(|x| x.run == Run::Running)
                    .map(|x| x.pid)
                    .collect();
            if running.is_empty() && seen.as_ref().is_some_and(|s| s.is_superset(&pids)) {
                break;
            }
            self.gate(GateStep::BeforeStop(round)).await;
            for pid in running {
                signal(pid, libc::SIGSTOP);
            }
            seen = Some(pids);
        }
        // ② 从叶子往根杀，每个会话留一个见证；③ 杀到确认为空
        let mut quiet: BTreeMap<Pid, u32> = BTreeMap::new();
        loop {
            self.gate(GateStep::Kill).await;
            let Some(m) = self.observe_until(deadline).await else {
                return false;
            };
            if m.is_empty() {
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
            for x in &m.0 {
                if !keep.contains(&x.pid) {
                    signal(x.pid, libc::SIGKILL);
                }
            }
            {
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

    /// 回收桥接：先让根组作废，再回收（它没退出就等它退出时回收）。只做一次。
    fn reap(&self) {
        let Some(mut child) = lock(&self.child).take() else {
            return;
        };
        lock(&self.scope).mark_reaped();
        tokio::spawn(async move {
            let _ = child.wait().await;
        });
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
                for m in Self::observe_blocking(&self.scope).0 {
                    signal(m.pid, libc::SIGKILL);
                }
            }
        }
    }
}

/// 按进程号发信号。进程已经没了（`ESRCH`）或不许发（`EPERM`）都不算错：下一遍扫描会照实际情况来。
fn signal(pid: Pid, sig: libc::c_int) {
    if pid <= 1 {
        return;
    }
    // SAFETY: kill 只读两个整数参数
    unsafe { libc::kill(pid, sig) };
}

/// 不回收地等子进程退出，返回退出说明。等的时候得到 `ECHILD`（被别处回收了）就让根组作废。
pub(super) fn wait_exit_no_reap(pid: Pid, scope: &Mutex<Scope>) -> String {
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
                lock(scope).mark_reaped();
                return "进程已被别处回收".into();
            }
            _ => return format!("等待进程退出失败：{err}"),
        }
    }
}
