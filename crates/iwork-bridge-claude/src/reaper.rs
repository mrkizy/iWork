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
/// CLI 的身份，也是「谁能动它」的交接点（审查 S6-5b 3/F-1）：
/// - `pid`（正数）：已登记，收尾线程可以对它发信号，tokio 那边还没回收它；
/// - `-pid`：收尾线程正在「检查它还在、发信号」，这期间 tokio 那边不能回收它（回收了号就可能被别人复用）；
/// - 0：没有 CLI，或者已经交还给 tokio 回收，之后不再对这个号发任何信号。
static CLI: AtomicI32 = AtomicI32::new(0);
/// 桥接是不是自己进程组的组长：是才对整组发 SIGKILL。
static LEADER: AtomicBool = AtomicBool::new(false);
static EVENTS: OnceLock<Sender<Event>> = OnceLock::new();

/// 交还 CLI 时最多等多久让收尾线程把信号发完（绝对期限；每次看完睡 [`RELEASE_STEP`]，墙钟最多再多出一次睡眠的
/// 超出）。对面只差两个系统调用，正常一次都不用等。
const RELEASE_LIMIT: Duration = Duration::from_millis(100);
const RELEASE_STEP: Duration = Duration::from_millis(1);

/// 收尾权（审查 S6-5b 4/F-1）：第一个进入收尾的拿到它，由它发 SIGTERM、等宽限期、最后杀整组。之后进来的（比如后台
/// 没了时收尾线程和 panic 处理同时进来）不发信号、不杀组，只等先来的那一处把进程结束；等满 [`OWNER_WAIT`] 进程还
/// 在（先来的那一处所在的线程被卡住了），才自己杀。这样谁也不会抢在已经开始的 SIGTERM 宽限期之前杀整组。
static CLEANING: AtomicBool = AtomicBool::new(false);
/// 后来的收尾者最多等先来的那一处多久：它的宽限期 [`BRIDGE_TERM_WAIT`] 再加一点余量。
const OWNER_WAIT: Duration = Duration::from_secs(3);

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
                // 从收到的这一刻算，不用等之前取的 now（审查 S6-5b 3/F-3）
                due.get_or_insert_with(|| Instant::now() + TEARDOWN_WAIT);
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
    run_cleanup(&|| {
        // SAFETY: 这几个调用只读两个整数参数；组号是自己的进程号（只在确认是组长时用）
        unsafe {
            if LEADER.load(Ordering::SeqCst) {
                libc::killpg(libc::getpid(), libc::SIGKILL);
            }
            signal_cli(libc::SIGKILL);
            libc::kill(libc::getpid(), libc::SIGKILL);
        }
    });
    // SIGKILL 自己之后走不到这里
    std::process::abort()
}

/// 收尾的前半段（SIGTERM CLI，等它最多 [`BRIDGE_TERM_WAIT`]），然后调 `finish`（产品里是杀掉整组，不返回；测试里
/// 只记一笔）。拿不到收尾权的只等，见 [`CLEANING`]。
fn run_cleanup(finish: &dyn Fn()) {
    if CLEANING.swap(true, Ordering::SeqCst) {
        let deadline = Instant::now() + OWNER_WAIT;
        while Instant::now() < deadline {
            std::thread::sleep(CLI_POLL);
        }
        finish();
        return;
    }
    let cli = CLI.load(Ordering::SeqCst);
    if signal_cli(libc::SIGTERM) {
        let deadline = Instant::now() + BRIDGE_TERM_WAIT;
        while Instant::now() < deadline && running(cli) {
            std::thread::sleep(CLI_POLL);
        }
    }
    finish();
}

/// 给 CLI 发一个信号：先占住它的身份（`pid` → `-pid`），再看它还在跑才发，做完放回。占不住（已经交还给 tokio，或者
/// 别处正在发）就不发。返回发了没有。「别处正在发」只可能是另一处收尾，而收尾权只给一处（[`CLEANING`]），所以拿着
/// 收尾权的那一处看到 `false` 只会是没有 CLI、或者 CLI 已经交还。
fn signal_cli(sig: libc::c_int) -> bool {
    let pid = CLI.load(Ordering::SeqCst);
    if pid <= 0
        || CLI
            .compare_exchange(pid, -pid, Ordering::SeqCst, Ordering::SeqCst)
            .is_err()
    {
        return false;
    }
    let sent = running(pid) && {
        #[cfg(test)]
        seam::pause();
        // SAFETY: kill 只读两个整数参数；占住期间 tokio 那边不回收它，号不会换人
        unsafe { libc::kill(pid, sig) == 0 }
    };
    let _ = CLI.compare_exchange(-pid, pid, Ordering::SeqCst, Ordering::SeqCst);
    sent
}

/// tokio 那边回收 CLI 之前必须先调（`CliProcess` 的 `kill`、`wait` 与 `Drop`，CLI 只有这几条回收路径）：把身份交还
/// （`pid` → 0），之后收尾线程不再对这个号发信号。收尾线程正在发（`-pid`）就等它发完，最多到 [`RELEASE_LIMIT`]
/// 这个绝对期限；等不到返回 `false`，调用方就不回收（宁可留一个僵尸到桥接退出，也不让号被复用）。
/// 不是登记过的那个 CLI、或者已经交还过，返回 `true`。
pub fn release_cli(pid: Option<u32>) -> bool {
    let Some(pid) = pid.and_then(|p| i32::try_from(p).ok()) else {
        return true;
    };
    let deadline = Instant::now() + RELEASE_LIMIT;
    loop {
        match CLI.compare_exchange(pid, 0, Ordering::SeqCst, Ordering::SeqCst) {
            Ok(_) => return true,
            Err(now) if now == -pid => {
                if Instant::now() >= deadline {
                    return false;
                }
                std::thread::sleep(RELEASE_STEP);
            }
            Err(_) => return true,
        }
    }
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

/// 测试用的接缝：让「检查之后、发信号之前」停住，好在中间插进别的事。只在 cfg(test) 下存在。
#[cfg(test)]
mod seam {
    use std::sync::Mutex;
    use std::sync::mpsc::{Receiver, Sender};

    type Hold = (Sender<()>, Receiver<()>);
    static HOLD: Mutex<Option<Hold>> = Mutex::new(None);

    /// 装上：下一次发信号走到检查之后，先往 `at` 报到，再等 `go`。
    pub fn set(at: Sender<()>, go: Receiver<()>) {
        *HOLD.lock().unwrap() = Some((at, go));
    }

    pub fn pause() {
        let hold = HOLD.lock().unwrap().take();
        if let Some((at, go)) = hold {
            let _ = at.send(());
            let _ = go.recv();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    use std::sync::mpsc;

    /// 这几条测试都改进程级的 CLI 状态，一条一条来。
    static ONE_AT_A_TIME: std::sync::Mutex<()> = std::sync::Mutex::new(());

    /// 收尾线程停在「检查过 CLI 还在」和「发信号」之间，另一个线程启动并停在那里，返回 (CLI, 放行的发送端, 发信号的线程)。
    fn signal_paused_after_check() -> (
        std::process::Child,
        mpsc::Sender<()>,
        std::thread::JoinHandle<bool>,
    ) {
        let child = std::process::Command::new("sleep")
            .arg("30")
            .spawn()
            .unwrap();
        register_cli(Some(child.id()));
        let (at_tx, at_rx) = mpsc::channel();
        let (go_tx, go_rx) = mpsc::channel();
        seam::set(at_tx, go_rx);
        let signaler = std::thread::spawn(|| signal_cli(libc::SIGTERM));
        at_rx.recv().unwrap();
        (child, go_tx, signaler)
    }

    /// 检查过 CLI 还在、还没发信号的时候，tokio 那边要回收它（审查 S6-5b 3/F-1）：交还要等信号发完才成功，回收排在
    /// 发信号之后。不然回收之后这个号可能被别的进程复用，接下来的信号就打到了别人身上。
    #[test]
    fn release_waits_for_an_in_flight_signal() {
        let _one = ONE_AT_A_TIME
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let (mut child, go, signaler) = signal_paused_after_check();
        let pid = child.id();
        let released = Arc::new(AtomicBool::new(false));
        let r = Arc::clone(&released);
        let releasing = std::thread::spawn(move || {
            let ok = release_cli(Some(pid));
            r.store(true, Ordering::SeqCst);
            ok
        });
        std::thread::sleep(Duration::from_millis(40));
        let released_early = released.load(Ordering::SeqCst);
        go.send(()).unwrap();
        let sent = signaler.join().unwrap();
        let ok = releasing.join().unwrap();
        let _ = child.kill();
        let _ = child.wait();
        CLI.store(0, Ordering::SeqCst);
        assert!(
            !released_early,
            "信号还没发，交还就成功了：回收可能排到发信号前面"
        );
        assert!(ok, "信号发完之后交还成功");
        assert!(sent, "信号发给了还没被回收的 CLI");
    }

    /// 收尾线程一直占着（比交还的上限久）：交还有界地放弃、返回 `false`，调用方不回收，不会无限等。
    #[test]
    fn release_gives_up_after_its_bound() {
        let _one = ONE_AT_A_TIME
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let (mut child, go, signaler) = signal_paused_after_check();
        let started = Instant::now();
        let ok = release_cli(Some(child.id()));
        let took = started.elapsed();
        go.send(()).unwrap();
        signaler.join().unwrap();
        let _ = child.kill();
        let _ = child.wait();
        CLI.store(0, Ordering::SeqCst);
        assert!(!ok, "占着的时候交还不成功");
        // 绝对期限 100 毫秒，加一次睡眠的超出；负载高时调度会再拖一点，这里留余量
        assert!(took < Duration::from_millis(500), "有界：{took:?}");
    }

    /// 两处同时收尾（后台没了时收尾线程和 panic 处理一起进来，审查 S6-5b 4/F-1）：先进来的停在检查之后、SIGTERM
    /// 之前；后进来的绝不能抢在它的 SIGTERM 和宽限期之前杀掉整组。CLI 照样先收到 SIGTERM。
    #[test]
    fn concurrent_cleanup_keeps_the_term_grace() {
        let _one = ONE_AT_A_TIME
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        reset_cleanup();
        let (mut child, go, first) = cleanup_paused_after_check();
        let finished = Arc::new(AtomicBool::new(false));
        let f = Arc::clone(&finished);
        let second = std::thread::spawn(move || {
            run_cleanup(&|| f.store(true, Ordering::SeqCst));
        });
        std::thread::sleep(Duration::from_millis(200));
        let killed_early = finished.load(Ordering::SeqCst);
        go.send(()).unwrap();
        first.join().unwrap();
        second.join().unwrap();
        let status = child.wait().unwrap();
        CLI.store(0, Ordering::SeqCst);
        assert!(!killed_early, "第一处还没发 SIGTERM，第二处就去杀整组了");
        assert_eq!(
            std::os::unix::process::ExitStatusExt::signal(&status),
            Some(libc::SIGTERM),
            "CLI 先收到了 SIGTERM：{status:?}"
        );
    }

    /// 对照：只有一处收尾时，CLI 收到 SIGTERM，宽限期里退出，然后才杀整组。
    #[test]
    fn single_cleanup_terms_the_cli_first() {
        let _one = ONE_AT_A_TIME
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        reset_cleanup();
        let mut child = std::process::Command::new("sleep")
            .arg("30")
            .spawn()
            .unwrap();
        register_cli(Some(child.id()));
        let finished = AtomicBool::new(false);
        run_cleanup(&|| finished.store(true, Ordering::SeqCst));
        let status = child.wait().unwrap();
        CLI.store(0, Ordering::SeqCst);
        assert!(finished.load(Ordering::SeqCst));
        assert_eq!(
            std::os::unix::process::ExitStatusExt::signal(&status),
            Some(libc::SIGTERM),
            "{status:?}"
        );
    }

    /// 起一条收尾，停在「检查过 CLI 还在」与 SIGTERM 之间。返回 (CLI, 放行的发送端, 那条收尾的线程)。
    fn cleanup_paused_after_check() -> (
        std::process::Child,
        mpsc::Sender<()>,
        std::thread::JoinHandle<()>,
    ) {
        let child = std::process::Command::new("sleep")
            .arg("30")
            .spawn()
            .unwrap();
        register_cli(Some(child.id()));
        let (at_tx, at_rx) = mpsc::channel();
        let (go_tx, go_rx) = mpsc::channel();
        seam::set(at_tx, go_rx);
        let first = std::thread::spawn(|| run_cleanup(&|| {}));
        at_rx.recv().unwrap();
        (child, go_tx, first)
    }

    /// 收尾的进程级状态复位（测试一条一条来，每条从头开始）。
    fn reset_cleanup() {
        CLEANING.store(false, Ordering::SeqCst);
    }

    /// 已经交还给 tokio 回收的 CLI，收尾时不再对这个号发任何信号。
    #[test]
    fn released_cli_is_never_signalled() {
        let _one = ONE_AT_A_TIME
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let mut child = std::process::Command::new("sleep")
            .arg("30")
            .spawn()
            .unwrap();
        register_cli(Some(child.id()));
        assert!(release_cli(Some(child.id())));
        let sent = signal_cli(libc::SIGTERM);
        let _ = child.kill();
        let _ = child.wait();
        CLI.store(0, Ordering::SeqCst);
        assert!(!sent, "交还之后不发信号");
    }
}
