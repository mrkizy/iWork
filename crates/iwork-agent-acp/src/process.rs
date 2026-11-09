//! 进程监管（技术设计 6.5）：每个运行时进程独立成树，结束时整棵树一起结束，不留孤儿；
//! 每 2 秒采样整棵树的内存与 CPU，只用于显示与基准，不参与业务判断。
//!
//! 「树」在 macOS / Linux 是进程组（子进程以自己为组长启动，`killpg` 结束整组）；
//! Windows 是 Job Object（设为关闭即结束全部进程，`TerminateJobObject` 主动结束）。

use std::ffi::OsString;
use std::io;
use std::path::PathBuf;
use std::process::Stdio;
#[cfg(unix)]
use std::sync::Arc;
#[cfg(unix)]
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use tokio::process::{ChildStdin, ChildStdout, Command};
use tokio::sync::watch;
use tokio::task::JoinHandle;

/// 采样周期（技术设计 6.5）。
pub const SAMPLE_INTERVAL: Duration = Duration::from_secs(2);
/// 请对方退出后等多久再强制结束整棵树。取消与关闭用同一个时限（技术设计 6.3 第 5 条：3s）。
pub const GRACE_PERIOD: Duration = Duration::from_secs(3);

/// 怎么启动一个运行时进程（桥接或原生 ACP 适配器）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RuntimeLaunch {
    pub program: PathBuf,
    pub args: Vec<OsString>,
    /// 追加的环境变量；其余继承后台服务自己的环境
    pub env: Vec<(OsString, OsString)>,
}

/// 进程怎么结束的。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExitInfo {
    /// 给人看的退出说明，如 `exit status: 3`、`signal: 9`
    pub description: String,
}

/// 一次资源采样：整棵进程树的合计。
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ResourceSample {
    /// 常驻内存，字节
    pub rss_bytes: u64,
    /// CPU 占用，百分比（多核可超过 100）
    pub cpu_percent: f32,
    /// 采到的进程数
    pub processes: usize,
}

/// 一个受监管的运行时进程。丢弃时强制结束整棵树。
#[derive(Debug)]
pub struct SupervisedProcess {
    pid: u32,
    tree: ProcessTree,
    exit: watch::Receiver<Option<ExitInfo>>,
    samples: watch::Receiver<Option<ResourceSample>>,
    sampler: JoinHandle<()>,
}

impl SupervisedProcess {
    /// 启动进程，接管它的 stdin / stdout；stderr 继承后台服务的（进日志）。
    pub fn spawn(launch: &RuntimeLaunch) -> io::Result<(Self, ChildStdin, ChildStdout)> {
        let mut cmd = Command::new(&launch.program);
        cmd.args(&launch.args)
            .envs(launch.env.iter().map(|(k, v)| (k, v)))
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit());
        #[cfg(unix)]
        cmd.process_group(0);
        let mut child = cmd.spawn().map_err(|e| {
            io::Error::new(
                e.kind(),
                format!("启动运行时 {} 失败：{e}", launch.program.display()),
            )
        })?;
        let pid = child
            .id()
            .ok_or_else(|| io::Error::other("运行时进程刚启动就已退出"))?;
        let tree = ProcessTree::adopt(pid, &child)?;
        let stdin = child.stdin.take().expect("stdin 设为 piped");
        let stdout = child.stdout.take().expect("stdout 设为 piped");

        let (exit_tx, exit) = watch::channel(None);
        tokio::spawn(async move {
            let description = match child.wait().await {
                Ok(status) => status.to_string(),
                Err(e) => format!("等待进程退出失败：{e}"),
            };
            let _ = exit_tx.send(Some(ExitInfo { description }));
        });
        let (sample_tx, samples) = watch::channel(None);
        let sampler = tokio::spawn(sample_loop(pid, sample_tx));
        Ok((
            Self {
                pid,
                tree,
                exit,
                samples,
                sampler,
            },
            stdin,
            stdout,
        ))
    }

    /// 进程号；Unix 上也是进程组号。
    pub fn pid(&self) -> u32 {
        self.pid
    }

    /// 已退出时返回退出说明。
    pub fn exit_info(&self) -> Option<ExitInfo> {
        self.exit.borrow().clone()
    }

    /// 等进程退出，最多等 `limit`；到时仍未退出返回 `None`。
    pub async fn wait_exit(&self, limit: Duration) -> Option<ExitInfo> {
        let mut exit = self.exit.clone();
        let waited = tokio::time::timeout(limit, exit.wait_for(Option::is_some)).await;
        match waited {
            Ok(Ok(info)) => info.clone(),
            _ => None,
        }
    }

    /// 最近一次采样；进程刚启动、还没采到时为 `None`。
    pub fn latest_sample(&self) -> Option<ResourceSample> {
        *self.samples.borrow()
    }

    /// 立即结束整棵树。可重复调用。
    pub fn kill_tree(&self) {
        self.tree.kill();
    }

    /// 先请进程退出，等 [`GRACE_PERIOD`]，再结束整棵树（组长退出后，组里可能还有它的子进程）。
    pub async fn shutdown(&self) {
        self.tree.terminate();
        self.wait_exit(GRACE_PERIOD).await;
        self.tree.kill();
    }

    /// 整棵树是否已经没有进程（Unix 查进程组，Windows 查 Job 里的活动进程数）。
    pub fn tree_is_empty(&self) -> io::Result<bool> {
        self.tree.is_empty()
    }
}

impl Drop for SupervisedProcess {
    fn drop(&mut self) {
        self.sampler.abort();
        self.tree.kill();
    }
}

async fn sample_loop(root: u32, out: watch::Sender<Option<ResourceSample>>) {
    use sysinfo::{Pid, ProcessRefreshKind, ProcessesToUpdate, System};
    let mut system = match System::new() {
        Ok(system) => system,
        // 采样只用于显示与基准；平台不支持时不采，不影响运行
        Err(_) => return,
    };
    let kind = ProcessRefreshKind::nothing().with_memory().with_cpu();
    let mut ticker = tokio::time::interval(SAMPLE_INTERVAL);
    loop {
        ticker.tick().await;
        system.refresh_processes_specifics(ProcessesToUpdate::All, true, kind);
        let processes = system.processes();
        // 从根往下按父进程号收集整棵树
        let mut tree = vec![Pid::from_u32(root)];
        let mut i = 0;
        while i < tree.len() {
            let parent = tree[i];
            tree.extend(
                processes
                    .iter()
                    .filter(|(_, p)| p.parent() == Some(parent))
                    .map(|(pid, _)| *pid),
            );
            i += 1;
        }
        let live: Vec<_> = tree.iter().filter_map(|pid| processes.get(pid)).collect();
        if live.is_empty() {
            return;
        }
        let sample = ResourceSample {
            rss_bytes: live.iter().map(|p| p.memory()).sum(),
            cpu_percent: live.iter().map(|p| p.cpu_usage()).sum(),
            processes: live.len(),
        };
        if out.send(Some(sample)).is_err() {
            return;
        }
    }
}

// ---------- 进程树：平台实现 ----------

#[cfg(unix)]
#[derive(Debug)]
struct ProcessTree {
    pgid: libc::pid_t,
    /// 整组已确认清空后不再发信号：进程组号可能被系统复用给无关进程
    gone: Arc<AtomicBool>,
}

#[cfg(unix)]
impl ProcessTree {
    fn adopt(pid: u32, _child: &tokio::process::Child) -> io::Result<Self> {
        let pgid = libc::pid_t::try_from(pid)
            .map_err(|_| io::Error::other(format!("进程号 {pid} 超出范围")))?;
        Ok(Self {
            pgid,
            gone: Arc::new(AtomicBool::new(false)),
        })
    }

    fn signal(&self, sig: libc::c_int) {
        if self.gone.load(Ordering::Acquire) {
            return;
        }
        // SAFETY: killpg 只读两个整数参数，没有内存安全前提。
        let rc = unsafe { libc::killpg(self.pgid, sig) };
        if rc != 0 && io::Error::last_os_error().raw_os_error() == Some(libc::ESRCH) {
            self.gone.store(true, Ordering::Release);
        }
    }

    fn terminate(&self) {
        self.signal(libc::SIGTERM);
    }

    fn kill(&self) {
        self.signal(libc::SIGKILL);
    }

    fn is_empty(&self) -> io::Result<bool> {
        if self.gone.load(Ordering::Acquire) {
            return Ok(true);
        }
        // SAFETY: 信号 0 只做存在性检查。
        let rc = unsafe { libc::killpg(self.pgid, 0) };
        if rc == 0 {
            return Ok(false);
        }
        let err = io::Error::last_os_error();
        match err.raw_os_error() {
            Some(libc::ESRCH) => {
                self.gone.store(true, Ordering::Release);
                Ok(true)
            }
            _ => Err(err),
        }
    }
}

#[cfg(windows)]
#[derive(Debug)]
struct ProcessTree {
    job: job::Job,
}

#[cfg(windows)]
impl ProcessTree {
    fn adopt(_pid: u32, child: &tokio::process::Child) -> io::Result<Self> {
        let job = job::Job::new()?;
        let handle = child
            .raw_handle()
            .ok_or_else(|| io::Error::other("运行时进程刚启动就已退出"))?;
        job.assign(handle)?;
        Ok(Self { job })
    }

    /// Windows 没有可靠的「请退出」信号，直接结束。
    fn terminate(&self) {
        self.job.terminate();
    }

    fn kill(&self) {
        self.job.terminate();
    }

    fn is_empty(&self) -> io::Result<bool> {
        Ok(self.job.active_processes()? == 0)
    }
}

#[cfg(windows)]
mod job {
    use std::io;
    use std::mem::{size_of, zeroed};
    use std::os::windows::io::RawHandle;

    use windows_sys::Win32::Foundation::{CloseHandle, HANDLE};
    use windows_sys::Win32::System::JobObjects::{
        AssignProcessToJobObject, CreateJobObjectW, JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
        JOBOBJECT_BASIC_ACCOUNTING_INFORMATION, JOBOBJECT_EXTENDED_LIMIT_INFORMATION,
        JobObjectBasicAccountingInformation, JobObjectExtendedLimitInformation,
        QueryInformationJobObject, SetInformationJobObject, TerminateJobObject,
    };

    /// Job Object 句柄。设了 KILL_ON_JOB_CLOSE：句柄关闭（含后台服务崩溃）时系统结束其中全部进程。
    #[derive(Debug)]
    pub struct Job(HANDLE);

    // SAFETY: Job 句柄是内核对象引用，可在线程间传递与共享；这里只调线程安全的 Win32 API。
    unsafe impl Send for Job {}
    unsafe impl Sync for Job {}

    impl Job {
        pub fn new() -> io::Result<Self> {
            // SAFETY: 两个参数都允许为空；返回空句柄表示失败。
            let handle = unsafe { CreateJobObjectW(std::ptr::null(), std::ptr::null()) };
            if handle.is_null() {
                return Err(io::Error::last_os_error());
            }
            let job = Self(handle);
            // SAFETY: 全零是这个结构体的合法初值。
            let mut info: JOBOBJECT_EXTENDED_LIMIT_INFORMATION = unsafe { zeroed() };
            info.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
            // SAFETY: 传入的指针与长度描述同一个活着的结构体。
            let ok = unsafe {
                SetInformationJobObject(
                    job.0,
                    JobObjectExtendedLimitInformation,
                    (&raw const info).cast(),
                    size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32,
                )
            };
            if ok == 0 {
                return Err(io::Error::last_os_error());
            }
            Ok(job)
        }

        pub fn assign(&self, process: RawHandle) -> io::Result<()> {
            // SAFETY: 两个句柄在调用期间都有效。
            let ok = unsafe { AssignProcessToJobObject(self.0, process as HANDLE) };
            if ok == 0 {
                return Err(io::Error::last_os_error());
            }
            Ok(())
        }

        pub fn terminate(&self) {
            // SAFETY: 句柄有效；失败（如已无进程）不影响后续。
            unsafe { TerminateJobObject(self.0, 1) };
        }

        pub fn active_processes(&self) -> io::Result<u32> {
            // SAFETY: 同上，指针与长度描述同一个结构体。
            let mut info: JOBOBJECT_BASIC_ACCOUNTING_INFORMATION = unsafe { zeroed() };
            let ok = unsafe {
                QueryInformationJobObject(
                    self.0,
                    JobObjectBasicAccountingInformation,
                    (&raw mut info).cast(),
                    size_of::<JOBOBJECT_BASIC_ACCOUNTING_INFORMATION>() as u32,
                    std::ptr::null_mut(),
                )
            };
            if ok == 0 {
                return Err(io::Error::last_os_error());
            }
            Ok(info.ActiveProcesses)
        }
    }

    impl Drop for Job {
        fn drop(&mut self) {
            // SAFETY: 句柄只在这里关闭一次。
            unsafe { CloseHandle(self.0) };
        }
    }
}
