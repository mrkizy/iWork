//! 进程监管（技术设计 6.5）：每个运行时进程独立成树，结束时整棵树一起结束，不留孤儿；
//! 每 2 秒采样整棵树的内存与 CPU，只用于显示与基准，不参与业务判断。
//!
//! 「树」在 macOS / Linux 是进程组（子进程以自己为组长启动，`killpg` 结束整组）；
//! Windows 是 Job Object：进程以挂起状态创建，加入 Job 之后才恢复运行，所以它起的每个后代都在
//! Job 里；Job 设为「句柄关闭即结束全部进程」，`TerminateJobObject` 主动结束。

use std::ffi::OsString;
use std::io;
use std::path::PathBuf;
use std::process::Stdio;
#[cfg(unix)]
use std::sync::Arc;
#[cfg(unix)]
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use tokio::process::{Child, ChildStderr, ChildStdin, ChildStdout, Command};
use tokio::sync::watch;
use tokio::task::JoinHandle;

/// 采样周期（技术设计 6.5）。
pub const SAMPLE_INTERVAL: Duration = Duration::from_secs(2);
/// 请对方退出后等多久再强制结束整棵树。取消与关闭用同一个时限（技术设计 6.3 第 5 条：3s）。
pub const GRACE_PERIOD: Duration = Duration::from_secs(3);
/// 强制结束后等整棵树消失的时限：信号送达、进程回收都要一点时间。
pub const TREE_CLEAR_LIMIT: Duration = Duration::from_secs(2);
/// 查进程树是否已空的间隔。
const TREE_POLL: Duration = Duration::from_millis(10);

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

/// 运行时进程的三个管道。stderr 交给调用方过滤，不直接进后台服务的日志。
#[derive(Debug)]
pub struct Pipes {
    pub stdin: ChildStdin,
    pub stdout: ChildStdout,
    pub stderr: ChildStderr,
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
    /// 启动进程并把它纳入监管。监管初始化任何一步失败，都先结束并回收已启动的进程再报错。
    pub fn spawn(launch: &RuntimeLaunch) -> io::Result<(Self, Pipes)> {
        let mut cmd = Command::new(&launch.program);
        cmd.args(&launch.args)
            .envs(launch.env.iter().map(|(k, v)| (k, v)))
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        #[cfg(unix)]
        cmd.process_group(0);
        #[cfg(windows)]
        cmd.creation_flags(windows_sys::Win32::System::Threading::CREATE_SUSPENDED);
        let mut child = cmd.spawn().map_err(|e| {
            io::Error::new(
                e.kind(),
                format!("启动运行时 {} 失败：{e}", launch.program.display()),
            )
        })?;
        let tree = match ProcessTree::adopt(&child) {
            Ok(tree) => tree,
            Err(e) => {
                reap(child);
                return Err(io::Error::new(
                    e.kind(),
                    format!("运行时进程纳入监管失败：{e}"),
                ));
            }
        };
        let pid = child.id().expect("adopt 成功说明进程还在");
        let pipes = Pipes {
            stdin: child.stdin.take().expect("stdin 设为 piped"),
            stdout: child.stdout.take().expect("stdout 设为 piped"),
            stderr: child.stderr.take().expect("stderr 设为 piped"),
        };

        let (exit_tx, exit) = watch::channel(None);
        tokio::spawn(async move {
            let description = match child.wait().await {
                Ok(status) => status.to_string(),
                Err(e) => format!("等待进程退出失败：{e}"),
            };
            let _ = exit_tx.send(Some(ExitInfo { description }));
        });
        let (sample_tx, samples) = watch::channel(None);
        let sampler = tokio::spawn(async move {
            let Ok(Ok(mut system)) = tokio::task::spawn_blocking(sysinfo::System::new).await else {
                // 采样只用于显示与基准；平台不支持时不采，不影响运行
                return;
            };
            sample_loop(move || sample_tree(&mut system, pid), sample_tx).await;
        });
        Ok((
            Self {
                pid,
                tree,
                exit,
                samples,
                sampler,
            },
            pipes,
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

    /// 等到进程退出。
    pub async fn exited(&self) -> ExitInfo {
        let mut exit = self.exit.clone();
        match exit.wait_for(Option::is_some).await {
            Ok(info) => info.clone().expect("wait_for 保证是 Some"),
            // 发送端随等待任务结束，任务结束前一定先发出退出信息
            Err(_) => self.exit.borrow().clone().unwrap_or(ExitInfo {
                description: "进程已退出".into(),
            }),
        }
    }

    /// 等进程退出，最多等 `limit`；到时仍未退出返回 `None`。
    pub async fn wait_exit(&self, limit: Duration) -> Option<ExitInfo> {
        tokio::time::timeout(limit, self.exited()).await.ok()
    }

    /// 最近一次采样；进程刚启动、还没采到时为 `None`。
    pub fn latest_sample(&self) -> Option<ResourceSample> {
        *self.samples.borrow()
    }

    /// 立即结束整棵树。可重复调用。
    pub fn kill_tree(&self) {
        self.tree.kill();
    }

    /// 结束整棵树并等它消失，最多等 [`TREE_CLEAR_LIMIT`]。返回是否确认已清空。
    pub async fn kill_tree_and_wait(&self) -> bool {
        let pid = self.pid();
        tracing::debug!(pid, "kill_tree_and_wait: enter");
        self.tree.kill();
        let deadline = tokio::time::Instant::now() + TREE_CLEAR_LIMIT;
        loop {
            if matches!(self.tree.is_empty(), Ok(true)) {
                tracing::debug!(pid, cleared = true, "kill_tree_and_wait: exit");
                return true;
            }
            if tokio::time::Instant::now() >= deadline {
                tracing::debug!(pid, cleared = false, "kill_tree_and_wait: exit");
                return false;
            }
            // 组里还有进程（含刚被结束、尚未回收的），再补一次信号
            self.tree.kill();
            tokio::time::sleep(TREE_POLL).await;
        }
    }

    /// 先请进程退出，等 [`GRACE_PERIOD`]，再结束整棵树（组长退出后，组里可能还有它的子进程）。
    pub async fn shutdown(&self) {
        let pid = self.pid();
        tracing::debug!(pid, "shutdown: enter");
        self.tree.terminate();
        let exited = self.wait_exit(GRACE_PERIOD).await.is_some();
        tracing::debug!(pid, exited, "shutdown: waited for exit");
        let cleared = self.kill_tree_and_wait().await;
        tracing::debug!(pid, cleared, "shutdown: exit");
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

/// 结束并回收一个还没纳入监管的子进程。
fn reap(mut child: Child) {
    let _ = child.start_kill();
    tokio::spawn(async move {
        let _ = child.wait().await;
    });
}

/// 采样循环。全机进程扫描是同步的重活，放到阻塞线程池里做，不占异步工作线程；
/// 一次做完才开始计下一次，不会重叠。`scan` 返回 `None`（树里已没有进程）时结束。
async fn sample_loop<F>(scan: F, out: watch::Sender<Option<ResourceSample>>)
where
    F: FnMut() -> Option<ResourceSample> + Send + 'static,
{
    let mut scan = scan;
    let mut ticker = tokio::time::interval(SAMPLE_INTERVAL);
    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    loop {
        ticker.tick().await;
        let Ok((back, sample)) = tokio::task::spawn_blocking(move || {
            let sample = scan();
            (scan, sample)
        })
        .await
        else {
            return;
        };
        scan = back;
        match sample {
            Some(sample) if out.send(Some(sample)).is_ok() => {}
            _ => return,
        }
    }
}

/// 扫一次全机进程，按父进程号从根收集整棵树并求和。树里一个进程都没有时返回 `None`。
fn sample_tree(system: &mut sysinfo::System, root: u32) -> Option<ResourceSample> {
    use sysinfo::{Pid, ProcessRefreshKind, ProcessesToUpdate};
    let kind = ProcessRefreshKind::nothing().with_memory().with_cpu();
    system.refresh_processes_specifics(ProcessesToUpdate::All, true, kind);
    let processes = system.processes();
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
        return None;
    }
    Some(ResourceSample {
        rss_bytes: live.iter().map(|p| p.memory()).sum(),
        cpu_percent: live.iter().map(|p| p.cpu_usage()).sum(),
        processes: live.len(),
    })
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
    fn adopt(child: &Child) -> io::Result<Self> {
        let pid = child
            .id()
            .ok_or_else(|| io::Error::other("运行时进程刚启动就已退出"))?;
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

    /// `ESRCH`：组里已没有进程。`EPERM`：组里还有进程，只是不许我们发信号（如组里有进程换了
    /// 用户），不能当作已清空。
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
            Some(libc::EPERM) => Ok(false),
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
    /// 进程此时是挂起的：先加入 Job，再恢复主线程。任何一步失败由调用方结束进程。
    fn adopt(child: &Child) -> io::Result<Self> {
        let pid = child
            .id()
            .ok_or_else(|| io::Error::other("运行时进程刚启动就已退出"))?;
        let handle = child
            .raw_handle()
            .ok_or_else(|| io::Error::other("运行时进程刚启动就已退出"))?;
        let job = job::Job::new()?;
        job.assign(handle)?;
        job::resume_main_thread(pid)?;
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

    use windows_sys::Win32::Foundation::{CloseHandle, HANDLE, INVALID_HANDLE_VALUE};
    use windows_sys::Win32::System::Diagnostics::ToolHelp::{
        CreateToolhelp32Snapshot, TH32CS_SNAPTHREAD, THREADENTRY32, Thread32First, Thread32Next,
    };
    use windows_sys::Win32::System::JobObjects::{
        AssignProcessToJobObject, CreateJobObjectW, JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
        JOBOBJECT_BASIC_ACCOUNTING_INFORMATION, JOBOBJECT_EXTENDED_LIMIT_INFORMATION,
        JobObjectBasicAccountingInformation, JobObjectExtendedLimitInformation,
        QueryInformationJobObject, SetInformationJobObject, TerminateJobObject,
    };
    use windows_sys::Win32::System::Threading::{OpenThread, ResumeThread, THREAD_SUSPEND_RESUME};

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
            // SAFETY: 全零是合法初值；指针与长度描述同一个结构体。
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

    /// 恢复以 `CREATE_SUSPENDED` 创建的进程：它此时只有主线程一个线程。
    pub fn resume_main_thread(pid: u32) -> io::Result<()> {
        // SAFETY: 快照句柄在本函数内使用并关闭；THREADENTRY32 先填好 dwSize 再交给系统。
        unsafe {
            let snapshot = CreateToolhelp32Snapshot(TH32CS_SNAPTHREAD, 0);
            if snapshot == INVALID_HANDLE_VALUE {
                return Err(io::Error::last_os_error());
            }
            let mut entry: THREADENTRY32 = zeroed();
            entry.dwSize = size_of::<THREADENTRY32>() as u32;
            let mut resumed = 0;
            let mut more = Thread32First(snapshot, &mut entry) != 0;
            while more {
                if entry.th32OwnerProcessID == pid {
                    let thread = OpenThread(THREAD_SUSPEND_RESUME, 0, entry.th32ThreadID);
                    if !thread.is_null() {
                        if ResumeThread(thread) != u32::MAX {
                            resumed += 1;
                        }
                        CloseHandle(thread);
                    }
                }
                more = Thread32Next(snapshot, &mut entry) != 0;
            }
            CloseHandle(snapshot);
            if resumed == 0 {
                return Err(io::Error::other(format!("恢复进程 {pid} 的主线程失败")));
            }
            Ok(())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(unix)]
    fn launch(program: &str, args: &[&str]) -> RuntimeLaunch {
        RuntimeLaunch {
            program: program.into(),
            args: args.iter().map(OsString::from).collect(),
            env: Vec::new(),
        }
    }

    /// 采样在阻塞线程池里做：单线程运行时上，一次很慢的扫描（300ms）期间，定时器照常触发。
    /// 扫描若跑在异步线程上，这里会出现 ≥300ms 的停顿。
    #[test]
    fn slow_scan_does_not_block_the_async_thread() {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        rt.block_on(async {
            let (tx, rx) = watch::channel(None);
            let scans = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
            let counter = scans.clone();
            let sampler = tokio::spawn(sample_loop(
                move || {
                    counter.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                    std::thread::sleep(Duration::from_millis(300));
                    Some(ResourceSample {
                        rss_bytes: 1,
                        cpu_percent: 0.0,
                        processes: 1,
                    })
                },
                tx,
            ));
            let start = tokio::time::Instant::now();
            let mut worst = Duration::ZERO;
            let mut last = start;
            // 覆盖首次扫描与一次周期扫描
            while start.elapsed() < Duration::from_millis(2600) {
                tokio::time::sleep(Duration::from_millis(5)).await;
                let now = tokio::time::Instant::now();
                worst = worst.max(now - last);
                last = now;
            }
            sampler.abort();
            assert!(
                scans.load(std::sync::atomic::Ordering::SeqCst) >= 2,
                "对照：期间确实扫了两次"
            );
            assert!(rx.borrow().is_some(), "对照：样本送到了");
            eprintln!("最长停顿 {worst:?}");
            assert!(
                worst < Duration::from_millis(150),
                "异步线程被卡了 {worst:?}"
            );
        });
    }

    /// 结束后整棵树确认清空，组长的子进程也在内。
    #[cfg(unix)]
    #[tokio::test]
    async fn kill_tree_and_wait_clears_children_too() {
        let (process, _pipes) =
            SupervisedProcess::spawn(&launch("sh", &["-c", "sleep 30 & sleep 30"])).unwrap();
        tokio::time::sleep(Duration::from_millis(100)).await;
        assert!(!process.tree_is_empty().unwrap(), "对照：运行中非空");
        assert!(process.kill_tree_and_wait().await);
        assert!(process.tree_is_empty().unwrap());
    }

    /// 启动失败时报出程序路径。
    #[tokio::test]
    async fn spawn_failure_names_the_program() {
        let err = SupervisedProcess::spawn(&RuntimeLaunch {
            program: "/nonexistent/iwork-runtime".into(),
            args: Vec::new(),
            env: Vec::new(),
        })
        .unwrap_err();
        assert!(
            err.to_string().contains("/nonexistent/iwork-runtime"),
            "{err}"
        );
    }
}
