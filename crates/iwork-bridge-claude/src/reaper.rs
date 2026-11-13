//! 收尾线程（T06 方案 5.4a 第 5 条第二种；审查 S6-5b 第 2 轮）。
//!
//! 桥接自己收尾的「触发」和「执行」都在这一个 `std::thread` 里，不经过 tokio、不经过 ACP、不读写任何管道，只用
//! 系统调用和时限。之前的做法把收尾挂在会等待的路径上：ACP 连接结束要先排空 stdout，执行之前要先同步写日志，
//! 后台是谁靠第一次看到的父进程号，任何一处卡住收尾就到不了。
//!
//! - 触发（任一）：
//!   - 后台没了：每 [`PARENT_POLL`] 比一次 `getppid()` 和后台告诉桥接的进程号（[`ENV_SUPERVISOR_PID`]）；
//!   - 输入结束、写不出去、连接结束：tokio 那边发 [`Event::Ended`]（发送从不等待）；
//!   - 致命错误之后等满 [`TEARDOWN_WAIT`]：会话停放时发 [`Event::Parked`]，从那一刻计时。
//! - 执行 [`self_clean`]：给 CLI 发 SIGTERM，最多等 [`BRIDGE_TERM_WAIT`] 让它收拾自己的命令，再对自己的进程组
//!   发 SIGKILL（不是组长时只杀 CLI 和自己）。可以被收尾线程和 panic 处理同时执行：只有系统调用和原子量，没有锁。
//!
//! 后台还在、桥接只是停放着等它来清的时候（第 5 条第一种），这里什么都不做，只计时。

use std::io;
use std::sync::OnceLock;
use std::sync::atomic::{AtomicBool, AtomicI32, Ordering};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender};
use std::time::{Duration, Instant};

use crate::deadlines::{BRIDGE_TERM_WAIT, PARENT_POLL, TEARDOWN_WAIT};

/// 后台起运行时时告诉它的「后台进程号」（`iwork-agent-acp` 的 `SupervisedProcess::spawn` 设）。
pub const ENV_SUPERVISOR_PID: &str = "IWORK_SUPERVISOR_PID";

/// 后台的进程号；0 表示收尾线程还没起（进程内测试里就是这样），这时一律当后台还在。
static SUPERVISOR: AtomicI32 = AtomicI32::new(0);
/// CLI 的进程号；0 表示还没起 CLI。
static CLI: AtomicI32 = AtomicI32::new(0);
/// 桥接是不是自己进程组的组长：是才对整组发 SIGKILL。
static LEADER: AtomicBool = AtomicBool::new(false);
static EVENTS: OnceLock<Sender<Event>> = OnceLock::new();

/// 看 CLI 退了没有的间隔。
const CLI_POLL: Duration = Duration::from_millis(50);

/// tokio 那边告诉收尾线程的事。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Event {
    /// 后台没了或接不了手：输入读到结束、输出写不出去、连接结束。附一句原因进日志
    Ended(&'static str),
    /// 会话结束、CLI 停放好了：开始等后台来清，最多 [`TEARDOWN_WAIT`]
    Parked,
}

/// 告诉收尾线程一件事。不等待；收尾线程没起（进程内测试）时什么都不做。
pub fn notify(event: Event) {
    if let Some(tx) = EVENTS.get() {
        let _ = tx.send(event);
    }
}

/// 记下 CLI 的进程号（一个桥接只有一条会话、一个 CLI）。
pub fn register_cli(pid: Option<u32>) {
    if let Some(pid) = pid.and_then(|p| i32::try_from(p).ok()) {
        CLI.store(pid, Ordering::SeqCst);
    }
}

/// 后台还在：父进程号还是后台告诉我们的那个。收尾线程没起时当它还在。
pub fn daemon_alive() -> bool {
    let supervisor = SUPERVISOR.load(Ordering::SeqCst);
    // SAFETY: getppid 不读写任何内存
    supervisor == 0 || unsafe { libc::getppid() } == supervisor
}

/// 起收尾线程。只调一次（`main` 里，确认过父进程就是后台之后）。
pub fn start(supervisor: i32, leader: bool) -> io::Result<()> {
    SUPERVISOR.store(supervisor, Ordering::SeqCst);
    LEADER.store(leader, Ordering::SeqCst);
    let (tx, rx) = mpsc::channel();
    EVENTS
        .set(tx)
        .map_err(|_| io::Error::other("收尾线程已经起过"))?;
    std::thread::Builder::new()
        .name("iwork-reaper".into())
        .spawn(move || watch(&rx))
        .map(drop)
}

fn watch(events: &Receiver<Event>) -> ! {
    let mut due: Option<Instant> = None;
    loop {
        if !daemon_alive() {
            tracing::warn!("父进程不再是后台：后台没了，自己收尾");
            self_clean();
        }
        let now = Instant::now();
        if due.is_some_and(|d| now >= d) {
            tracing::warn!("致命错误之后等满了 TEARDOWN_WAIT，后台没来清树，自己收尾");
            self_clean();
        }
        let wait = due.map_or(PARENT_POLL, |d| {
            d.saturating_duration_since(now).min(PARENT_POLL)
        });
        match events.recv_timeout(wait) {
            Ok(Event::Ended(why)) => {
                tracing::warn!(why, "后台没了或接不了手，自己收尾");
                self_clean();
            }
            Ok(Event::Parked) => {
                due.get_or_insert(now + TEARDOWN_WAIT);
            }
            Err(RecvTimeoutError::Timeout) => {}
            // 发送端放在 static 里，不会断；万一断了，照样按时限轮询父进程
            Err(RecvTimeoutError::Disconnected) => std::thread::sleep(wait),
        }
    }
}

/// 桥接自己收尾（5.4a 第 5 条第二种）：SIGTERM CLI，最多等 [`BRIDGE_TERM_WAIT`]，再对自己的进程组发 SIGKILL。
/// 只用系统调用和原子量，可以从任何线程、同时从几处调用（重复的 SIGTERM、SIGKILL 没有害处）。不返回。
pub fn self_clean() -> ! {
    let cli = CLI.load(Ordering::SeqCst);
    if cli > 0 && running(cli) {
        // SAFETY: kill 只读两个整数参数；CLI 是桥接的子进程，还没被回收（running 刚看过），号不会是别人的
        unsafe { libc::kill(cli, libc::SIGTERM) };
        let deadline = Instant::now() + BRIDGE_TERM_WAIT;
        while Instant::now() < deadline && running(cli) {
            std::thread::sleep(CLI_POLL);
        }
    }
    // SAFETY: 这几个调用只读两个整数参数；组号是自己的进程号（只在确认是组长时用）
    unsafe {
        if LEADER.load(Ordering::SeqCst) {
            libc::killpg(libc::getpid(), libc::SIGKILL);
        }
        if cli > 0 && running(cli) {
            libc::kill(cli, libc::SIGKILL);
        }
        libc::kill(libc::getpid(), libc::SIGKILL);
    }
    // SIGKILL 自己之后走不到这里
    std::process::abort()
}

/// CLI 还在跑（没退出）：只看不回收（`WNOWAIT`），回收照旧归 tokio 的 `Child`，或者桥接死后归 init。已经被回收了
/// （`ECHILD`）算不在跑：那时这个号可能已经是别人的，不能再对它发信号。
fn running(pid: i32) -> bool {
    // SAFETY: 全零是 siginfo_t 的合法初值；指针指向本地变量
    let mut info: libc::siginfo_t = unsafe { std::mem::zeroed() };
    // SAFETY: waitid 只写 info
    let rc = unsafe {
        libc::waitid(
            libc::P_PID,
            pid as libc::id_t,
            &mut info,
            libc::WEXITED | libc::WNOHANG | libc::WNOWAIT,
        )
    };
    // SAFETY: waitid 成功时 si_pid 有效；还没退出时为 0
    rc == 0 && unsafe { info.si_pid() } == 0
}
