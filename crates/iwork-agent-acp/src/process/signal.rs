//! 发信号按身份，不按裸进程号（T06 方案 5.4a「发信号按身份」，审查 S6-5 1/F-1）。进程树里**只有这里**发信号
//! （`only_signal_member_sends_signals` 查源码）。
//!
//! 成员的身份是（进程号，原生出生时间），出生时间是扫描读到它时的值。发之前确认这个号此刻还归那个进程：
//!
//! - Linux 内核有 `pidfd_open` 时：先拿句柄，再读出生时间比对，对上才经句柄发。句柄指的是打开那一刻占着这个号的
//!   进程；之后读到的出生时间等于记下的值，说明记下的那个进程一直占着这个号，句柄指的就是它。它没了就是 `ESRCH`。
//! - macOS，以及 Linux 上 `pidfd_open` 用不了（`ENOSYS`、被系统调用过滤拒了）：当场再读一次出生时间，对上紧接着
//!   `kill`。两步之间的窗口是待产品负责人确认的已知限制（方案「剩下的窗口」）。
//!
//! 对不上、读不到 → 不发，当它已经没了；下一遍扫描照实际情况来。
//!
//! 读出生时间是同步的系统调用，可能卡住：调用方把「复核 + 发送」放在阻塞线程里，带上这次清理的绝对期限
//! `until`；读完出生时间之后、发信号之前再看一眼期限，过了就不发（审查 S6-5 2/F-2）。

use std::time::Instant;

use super::scan;
use super::scope::{Birth, Pid};

/// 一个成员的身份。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Ident {
    pub pid: Pid,
    pub birth: Birth,
}

/// 发信号要用到的两样系统操作，测试换成替身。
pub trait Os {
    /// 这个号此刻的出生时间；没了、读不到为 `None`
    fn birth_of(&self, pid: Pid) -> Option<Birth>;
    /// 按进程号发信号
    fn kill(&self, pid: Pid, sig: libc::c_int);
}

/// 真的系统。
pub struct RealOs;

impl Os for RealOs {
    fn birth_of(&self, pid: Pid) -> Option<Birth> {
        scan::birth_of(pid)
    }

    fn kill(&self, pid: Pid, sig: libc::c_int) {
        // SAFETY: kill 只读两个整数参数
        unsafe { libc::kill(pid, sig) };
    }
}

/// 给范围里的一个成员发信号。`until`：过了这个时刻就不发（`None` 不限）。返回发了没有。
pub fn signal_member(who: Ident, sig: libc::c_int, until: Option<Instant>) -> bool {
    #[cfg(target_os = "linux")]
    if let Some(sent) = pidfd::signal(who, sig, until) {
        return sent;
    }
    verify_then_kill(&RealOs, who, sig, until)
}

/// 期限还没到。
fn in_time(until: Option<Instant>) -> bool {
    until.is_none_or(|u| Instant::now() < u)
}

/// 没有句柄可用时：当场再读一次出生时间，对上、期限也没过，紧接着发。
pub fn verify_then_kill(
    os: &impl Os,
    who: Ident,
    sig: libc::c_int,
    until: Option<Instant>,
) -> bool {
    if who.pid <= 1 || os.birth_of(who.pid) != Some(who.birth) || !in_time(until) {
        return false;
    }
    os.kill(who.pid, sig);
    true
}

#[cfg(target_os = "linux")]
mod pidfd {
    use std::time::Instant;

    use super::{Ident, in_time, scan};

    /// 按句柄发。`None`：这台机器用不了 `pidfd_open`，调用方改走复核再发；`Some(sent)`：走过句柄这条路了。
    pub fn signal(who: Ident, sig: libc::c_int, until: Option<Instant>) -> Option<bool> {
        if who.pid <= 1 {
            return Some(false);
        }
        // SAFETY: pidfd_open 读两个整数参数，返回新的描述符或 -1
        let fd = unsafe { libc::syscall(libc::SYS_pidfd_open, who.pid, 0) };
        if fd < 0 {
            let e = std::io::Error::last_os_error();
            return match e.raw_os_error() {
                // 号上已经没有进程了
                Some(libc::ESRCH) => Some(false),
                // 内核没有，或者被系统调用过滤拒了
                _ => None,
            };
        }
        let fd = fd as libc::c_int;
        let sent = scan::birth_of(who.pid) == Some(who.birth)
            && in_time(until)
            // SAFETY: fd 是刚打开的 pidfd；info 为空、flags 为 0
            && unsafe {
                libc::syscall(
                    libc::SYS_pidfd_send_signal,
                    fd,
                    sig,
                    std::ptr::null::<libc::siginfo_t>(),
                    0,
                )
            } == 0;
        // SAFETY: 关掉自己打开的描述符
        unsafe { libc::close(fd) };
        Some(sent)
    }
}

#[cfg(test)]
mod tests {
    use std::cell::RefCell;

    use super::*;

    /// 替身：出生时间按表答，发出去的信号记下来。
    struct Fake {
        births: Vec<(Pid, Birth)>,
        sent: RefCell<Vec<(Pid, libc::c_int)>>,
    }

    impl Os for Fake {
        fn birth_of(&self, pid: Pid) -> Option<Birth> {
            self.births.iter().find(|(p, _)| *p == pid).map(|(_, b)| *b)
        }
        fn kill(&self, pid: Pid, sig: libc::c_int) {
            self.sent.borrow_mut().push((pid, sig));
        }
    }

    #[test]
    fn signal_only_when_birth_still_matches() {
        let os = Fake {
            births: vec![(10, Birth(5)), (11, Birth(9))],
            sent: RefCell::default(),
        };
        let who = |pid, birth| Ident {
            pid,
            birth: Birth(birth),
        };
        // 对得上 → 发一次
        assert!(verify_then_kill(&os, who(10, 5), libc::SIGSTOP, None));
        // 号被别人拿了（出生时间不同）→ 不发
        assert!(!verify_then_kill(&os, who(11, 8), libc::SIGKILL, None));
        // 读不到 → 不发
        assert!(!verify_then_kill(&os, who(12, 1), libc::SIGKILL, None));
        // 对得上，但读完时期限已过 → 不发
        let past = Instant::now() - std::time::Duration::from_millis(1);
        assert!(!verify_then_kill(
            &os,
            who(10, 5),
            libc::SIGKILL,
            Some(past)
        ));
        assert_eq!(*os.sent.borrow(), vec![(10, libc::SIGSTOP)]);
    }

    /// 真的系统上：对一个已经退出并被回收的进程（号还记着、出生时间对不上或读不到）一次都不发。
    #[test]
    fn reaped_process_gets_no_signal() {
        let mut child = std::process::Command::new("sleep")
            .arg("30")
            .spawn()
            .unwrap();
        let pid = child.id() as Pid;
        let birth = scan::birth_of(pid).expect("活着读得到");
        child.kill().unwrap();
        child.wait().unwrap();
        assert!(!signal_member(Ident { pid, birth }, libc::SIGKILL, None));
    }

    /// Linux 句柄那条路：句柄打开之后进程退出并被回收，经句柄发信号得到 `ESRCH`，到不了后来拿到这个号的进程。
    #[cfg(target_os = "linux")]
    #[test]
    fn pidfd_of_reaped_process_is_esrch() {
        let mut child = std::process::Command::new("sleep")
            .arg("30")
            .spawn()
            .unwrap();
        let pid = child.id() as Pid;
        // SAFETY: pidfd_open 读两个整数参数
        let fd = unsafe { libc::syscall(libc::SYS_pidfd_open, pid, 0) };
        if fd < 0 {
            eprintln!("这台机器用不了 pidfd_open，跳过");
            child.kill().unwrap();
            child.wait().unwrap();
            return;
        }
        child.kill().unwrap();
        child.wait().unwrap();
        // SAFETY: fd 是刚打开的 pidfd
        let rc = unsafe {
            libc::syscall(
                libc::SYS_pidfd_send_signal,
                fd as libc::c_int,
                libc::SIGKILL,
                std::ptr::null::<libc::siginfo_t>(),
                0,
            )
        };
        let err = std::io::Error::last_os_error();
        // SAFETY: 关掉自己打开的描述符
        unsafe { libc::close(fd as libc::c_int) };
        assert_eq!((rc, err.raw_os_error()), (-1, Some(libc::ESRCH)));
    }

    /// 进程树的源码里，发信号的系统调用只出现在本文件的实现里（测试文件里的清理代码不算）。
    #[test]
    fn only_signal_member_sends_signals() {
        let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
        let mut files = vec![dir.join("process.rs")];
        for e in std::fs::read_dir(dir.join("process")).unwrap() {
            let p = e.unwrap().path();
            if p.extension().is_some_and(|x| x == "rs") {
                files.push(p);
            }
        }
        for sub in ["scope"] {
            let d = dir.join("process").join(sub);
            if d.is_dir() {
                for e in std::fs::read_dir(d).unwrap() {
                    files.push(e.unwrap().path());
                }
            }
        }
        let mut offenders = Vec::new();
        for f in &files {
            let name = f.file_name().unwrap().to_str().unwrap();
            if name == "signal.rs" || name.contains("tests") {
                continue;
            }
            let src = std::fs::read_to_string(f).unwrap();
            for needle in ["libc::kill(", "libc::killpg(", "SYS_pidfd_send_signal"] {
                if src.contains(needle) {
                    offenders.push(format!("{name}: {needle}"));
                }
            }
        }
        assert!(offenders.is_empty(), "只许 signal.rs 发信号：{offenders:?}");
    }
}
