//! 进程监管（技术设计 6.5）：每个运行时进程独立成树，结束时整棵树一起结束，不留孤儿；
//! 每 2 秒采样整棵树的内存与 CPU，只用于显示与基准，不参与业务判断。
//!
//! 「树」在 macOS / Linux 是 T06 方案 5.4a 的清理范围（[`tree`]）：子进程以自己为组长启动，范围是它的进程组、
//! 记到过的后代、后代另立的会话；结束时先找后杀，连另立会话的命令一起清，清完才回收子进程。
//! Windows 是 Job Object：进程以挂起状态创建，加入 Job 之后才恢复运行，所以它起的每个后代都在
//! Job 里；Job 设为「句柄关闭即结束全部进程」，`TerminateJobObject` 主动结束。

#[cfg(unix)]
mod scan;
#[cfg(unix)]
mod scope;
#[cfg(unix)]
mod signal;
#[cfg(unix)]
mod tree;
#[cfg(all(test, unix))]
mod tree_tests;

use std::ffi::OsString;
use std::io;
use std::path::PathBuf;
use std::process::Stdio;
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
/// 查进程树是否已空的间隔（Windows）。
#[cfg(windows)]
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

/// 后台起运行时时设的环境变量：值是后台自己的进程号。
pub const SUPERVISOR_PID_ENV: &str = "IWORK_SUPERVISOR_PID";

impl SupervisedProcess {
    /// 启动进程并把它纳入监管。监管初始化任何一步失败，都先结束并回收已启动的进程再报错。
    pub fn spawn(launch: &RuntimeLaunch) -> io::Result<(Self, Pipes)> {
        let mut cmd = Command::new(&launch.program);
        cmd.args(&launch.args)
            .envs(launch.env.iter().map(|(k, v)| (k, v)))
            // 告诉运行时谁是后台：它的父进程号一旦不是这个，就是后台没了（T06 方案 5.4a 第 5 条；审查 S6-5b
            // 2/F-2：运行时自己看到的第一个父进程号不可信，后台可能在它进 main 之前就死了）
            .env(SUPERVISOR_PID_ENV, std::process::id().to_string())
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
        let pid = child
            .id()
            .ok_or_else(|| io::Error::other("运行时进程刚启动就已退出"))?;
        let pipes = Pipes {
            stdin: child.stdin.take().expect("stdin 设为 piped"),
            stdout: child.stdout.take().expect("stdout 设为 piped"),
            stderr: child.stderr.take().expect("stderr 设为 piped"),
        };
        let (exit_tx, exit) = watch::channel(None);
        let tree = adopt(child, exit_tx)?;
        let (sample_tx, samples) = watch::channel(None);
        #[cfg(unix)]
        let sampled = tree.clone();
        let sampler = tokio::spawn(async move {
            let Ok(Ok(mut system)) = tokio::task::spawn_blocking(sysinfo::System::new).await else {
                // 采样只用于显示与基准；平台不支持时不采，不影响运行
                return;
            };
            // Unix：采样顺带记一次清理范围（5.4a），只遍历一次进程表
            #[cfg(unix)]
            sample_loop(move || sampled.0.sample(&mut system), sample_tx).await;
            #[cfg(windows)]
            sample_loop(
                move || sample_tree(&mut system, pid).map_or(Tick::Done, Tick::Sample),
                sample_tx,
            )
            .await;
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

    /// 立即开始结束整棵树，不等。可重复调用。
    pub fn kill_tree(&self) {
        self.tree.kill();
    }

    /// 记一次清理范围（取消一开始，T06 方案 5.4a「什么时候记」）：之后 CLI 退了，它起的命令也找得到。
    pub async fn record(&self) {
        self.tree.record().await;
    }

    /// 结束整棵树并等它消失，最多等 [`TREE_CLEAR_LIMIT`]。返回是否确认已清空。
    #[cfg(unix)]
    pub async fn kill_tree_and_wait(&self) -> bool {
        self.tree.clear().await
    }

    /// 测试用：直接拿到 Unix 的进程树（装闸门、看范围）。
    #[cfg(all(test, unix))]
    fn unix_tree(&self) -> &tree::ProcessTree {
        &self.tree.0
    }

    /// 结束整棵树并等它消失，最多等 [`TREE_CLEAR_LIMIT`]。返回是否确认已清空。
    #[cfg(windows)]
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
        self.tree.terminate().await;
        let exited = self.wait_exit(GRACE_PERIOD).await.is_some();
        tracing::debug!(pid, exited, "shutdown: waited for exit");
        let cleared = self.kill_tree_and_wait().await;
        tracing::debug!(pid, cleared, "shutdown: exit");
    }

    /// 整棵树是否已经没有进程（Unix 查清理范围里有没有活成员，Windows 查 Job 里的活动进程数）。
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

/// 一次采样的结果。
#[derive(Debug, Clone, Copy, PartialEq)]
enum Tick {
    Sample(ResourceSample),
    /// 这一次没采成（扫描失败），下次再来
    #[cfg_attr(windows, allow(dead_code))]
    Skip,
    /// 树里已没有进程，采样结束
    Done,
}

/// 采样循环。全机进程扫描是同步的重活，放到阻塞线程池里做，不占异步工作线程；
/// 一次做完才开始计下一次，不会重叠。`scan` 返回 [`Tick::Done`]（树里已没有进程）时结束。
async fn sample_loop<F>(scan: F, out: watch::Sender<Option<ResourceSample>>)
where
    F: FnMut() -> Tick + Send + 'static,
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
            Tick::Sample(sample) if out.send(Some(sample)).is_ok() => {}
            Tick::Skip => {}
            _ => return,
        }
    }
}

/// 扫一次全机进程，按父进程号从根收集整棵树并求和。树里一个进程都没有时返回 `None`。
#[cfg(windows)]
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

/// Unix：清理范围（[`tree::ProcessTree`]，T06 方案 5.4a）。
#[cfg(unix)]
#[derive(Debug, Clone)]
struct ProcessTree(tree::ProcessTree);

/// 接管刚起来的子进程；失败时结束并回收它再报错。
#[cfg(unix)]
fn adopt(child: Child, exit: watch::Sender<Option<ExitInfo>>) -> io::Result<ProcessTree> {
    tree::ProcessTree::adopt(child, exit)
        .map(ProcessTree)
        .map_err(|failed| {
            let (e, child) = *failed;
            reap(child);
            io::Error::new(e.kind(), format!("运行时进程纳入监管失败：{e}"))
        })
}

#[cfg(unix)]
impl ProcessTree {
    /// 请桥接退出：只发给桥接自己，它的后代由之后的清理收拾。它的进程号已经不归我们（回收了、根作废了）就不发。
    async fn terminate(&self) {
        self.0.signal_root(libc::SIGTERM).await;
    }

    fn kill(&self) {
        self.0.kill_detached();
    }

    fn is_empty(&self) -> io::Result<bool> {
        Ok(self.0.is_empty())
    }

    async fn record(&self) {
        self.0.record().await;
    }

    async fn clear(&self) -> bool {
        self.0.clear().await
    }
}

#[cfg(windows)]
#[derive(Debug)]
struct ProcessTree {
    job: job::Job,
}

/// 接管刚起来的子进程（挂起状态）；失败时结束并回收它再报错。退出说明由等待任务发进 `exit`。
#[cfg(windows)]
fn adopt(mut child: Child, exit: watch::Sender<Option<ExitInfo>>) -> io::Result<ProcessTree> {
    let tree = match ProcessTree::adopt_job(&child) {
        Ok(tree) => tree,
        Err(e) => {
            reap(child);
            return Err(io::Error::new(
                e.kind(),
                format!("运行时进程纳入监管失败：{e}"),
            ));
        }
    };
    tokio::spawn(async move {
        let description = match child.wait().await {
            Ok(status) => status.to_string(),
            Err(e) => format!("等待进程退出失败：{e}"),
        };
        let _ = exit.send(Some(ExitInfo { description }));
    });
    Ok(tree)
}

#[cfg(windows)]
impl ProcessTree {
    /// 进程此时是挂起的：先加入 Job，再恢复主线程。任何一步失败由调用方结束进程。
    fn adopt_job(child: &Child) -> io::Result<Self> {
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
    async fn terminate(&self) {
        self.job.terminate();
    }

    fn kill(&self) {
        self.job.terminate();
    }

    fn is_empty(&self) -> io::Result<bool> {
        Ok(self.job.active_processes()? == 0)
    }

    async fn record(&self) {}
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

    /// 采样不卡住异步任务：单线程运行时上，扫描进行中异步任务照样能推进。
    ///
    /// 扫描和异步侧握手：扫描一开始就通知异步侧，然后等异步侧放行才返回。扫描若卡住异步线程（直接在异步线程上
    /// 扫，或在别的线程扫、异步线程同步等它），放行的任务跑不起来，扫描等到兜底时限、记一次「没放行」。
    /// 不比停顿时长（CI 机器的调度抖动就能有上百毫秒）；兜底时限只防测试本身死锁，给得很宽。
    /// 等两份样本：首轮扫描和第一次周期扫描（间隔 [`SAMPLE_INTERVAL`]）都覆盖。
    #[test]
    fn slow_scan_does_not_block_the_async_thread() {
        const RELEASE_LIMIT: Duration = Duration::from_secs(10);
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        rt.block_on(async {
            // 握手带轮次号：上一轮超时后才迟到的放行不能算给这一轮
            let (started_tx, mut started_rx) = tokio::sync::mpsc::unbounded_channel::<u32>();
            let (release_tx, release_rx) = std::sync::mpsc::channel::<u32>();
            // 异步侧：扫描每开始一次就放行一次
            let releaser = tokio::spawn(async move {
                while let Some(round) = started_rx.recv().await {
                    if release_tx.send(round).is_err() {
                        return;
                    }
                }
            });
            // 单线程运行时：所有异步任务都在 block_on 的这个线程上跑
            let async_thread = std::thread::current().id();
            // 每次扫描：是否在兜底时限内等到放行、跑在哪个线程上
            let released = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
            let record = released.clone();
            let (tx, mut rx) = watch::channel(None);
            let mut round = 0u32;
            let sampler = tokio::spawn(sample_loop(
                move || {
                    round += 1;
                    started_tx.send(round).unwrap();
                    let deadline = std::time::Instant::now() + RELEASE_LIMIT;
                    let ok = loop {
                        let left = deadline.saturating_duration_since(std::time::Instant::now());
                        match release_rx.recv_timeout(left) {
                            Ok(r) if r == round => break true,
                            Ok(_) => {}
                            Err(_) => break false,
                        }
                    };
                    record
                        .lock()
                        .unwrap()
                        .push((ok, std::thread::current().id()));
                    Tick::Sample(ResourceSample {
                        rss_bytes: 1,
                        cpu_percent: 0.0,
                        processes: 1,
                    })
                },
                tx,
            ));
            for round in ["首轮", "周期"] {
                tokio::time::timeout(Duration::from_secs(60), rx.changed())
                    .await
                    .unwrap_or_else(|_| panic!("60s 内没收到{round}扫描的样本"))
                    .unwrap();
            }
            sampler.abort();
            releaser.abort();
            let released = released.lock().unwrap().clone();
            assert!(
                released.len() >= 2,
                "对照：首轮和周期扫描都做了：{released:?}"
            );
            assert!(
                released.iter().all(|(ok, _)| *ok),
                "扫描进行中异步任务没能推进（{RELEASE_LIMIT:?} 内没放行）：{released:?}"
            );
            // 辅助：扫描本身也不在异步线程上
            assert!(
                released.iter().all(|(_, t)| *t != async_thread),
                "扫描跑在了异步线程 {async_thread:?} 上：{released:?}"
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
