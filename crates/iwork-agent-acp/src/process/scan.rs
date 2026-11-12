//! 读进程表（T06 方案 5.4a「怎么记」）。范围要每个进程的父进程号、组号、会话号、是否停住和**系统原生的
//! 高精度出生时间**；`sysinfo` 给不全（出生时间只有秒，没有组号、会话号），所以直接向系统要：
//!
//! - macOS：`proc_listallpids` 列进程号，每个进程一次 `proc_pidinfo(PROC_PIDTBSDINFO)`（父进程号、组号、
//!   状态、出生时间的秒与微秒）加一次 `getsid`；
//! - Linux：`/proc/<pid>/stat` 一个文件里都有（出生时间是开机以来的时钟滴答）。
//!
//! 进程表里只放**读得到的活进程**。三种进程不进表，也就不算活成员、不能当任何东西的证明：
//!
//! - 已经退出、等回收的（僵尸）：macOS 上 `proc_pidinfo` 对它报 `ESRCH`，Linux 上状态是 `Z`/`X`；
//! - 读的那一刻刚好退出的：同上，下一遍扫描自然看不到；
//! - 读不到的（别的用户的进程，`EPERM`）：运行时的后代和后台是同一个用户；换了用户的后代我们也发不了
//!   信号，算已知限制。
//!
//! 会话号单独读不到（`getsid` 失败）时这一行照样进表，只是 `sid` 为 `None`：它还能按根组或父链进范围、
//! 被冻结与杀掉，但不会被记下，也不能按会话认领。
//!
//! 本机约 1200 个进程时一遍 2 毫秒左右；调用方（清树、采样）放在阻塞线程里做，并自己限时。

use super::scope::{Birth, Pid, Proc, Recheck, Run, Table};

/// 读一遍全机的进程表。
pub fn read_table() -> Table {
    Table::from_procs(list_pids().into_iter().filter_map(read_one))
}

/// 读完进程表之后复核见证：每个进程号此刻的（出生时间，会话号）。
pub fn recheck(pids: &[Pid]) -> Recheck {
    pids.iter()
        .map(|pid| {
            let now = read_one(*pid).and_then(|p| Some((p.birth?, p.sid?)));
            (*pid, now)
        })
        .collect()
}

#[cfg(target_os = "macos")]
fn list_pids() -> Vec<Pid> {
    // 先问个数，多留些余量；系统填满了余量（进程在这期间变多了）就加大再来
    // SAFETY: 缓冲区为空、长度为 0 时只返回个数
    let mut cap = unsafe { libc::proc_listallpids(std::ptr::null_mut(), 0) }.max(0) as usize + 256;
    loop {
        let mut buf: Vec<Pid> = vec![0; cap];
        let bytes = i32::try_from(cap * std::mem::size_of::<Pid>()).unwrap_or(i32::MAX);
        // SAFETY: 缓冲区与长度描述同一块活着的内存
        let n = unsafe { libc::proc_listallpids(buf.as_mut_ptr().cast(), bytes) };
        if n < 0 {
            return Vec::new();
        }
        let n = n as usize;
        if n < cap {
            buf.truncate(n);
            return buf;
        }
        cap *= 2;
    }
}

#[cfg(target_os = "macos")]
fn read_one(pid: Pid) -> Option<Proc> {
    if pid <= 0 {
        return None;
    }
    let size = std::mem::size_of::<libc::proc_bsdinfo>();
    // SAFETY: 全零是这个纯数据结构体的合法初值；传入的指针与长度描述同一个结构体
    let info = unsafe {
        let mut info: libc::proc_bsdinfo = std::mem::zeroed();
        let n = libc::proc_pidinfo(
            pid,
            libc::PROC_PIDTBSDINFO,
            0,
            (&raw mut info).cast(),
            size as libc::c_int,
        );
        if n as usize != size {
            return None;
        }
        info
    };
    let run = match info.pbi_status {
        libc::SZOMB => return None,
        libc::SSTOP => Run::Stopped,
        _ => Run::Running,
    };
    // SAFETY: getsid 只读一个整数参数
    let sid = unsafe { libc::getsid(pid) };
    Some(Proc {
        pid,
        ppid: info.pbi_ppid as Pid,
        pgid: info.pbi_pgid as Pid,
        sid: (sid > 0).then_some(sid),
        birth: Some(Birth(
            info.pbi_start_tvsec * 1_000_000 + info.pbi_start_tvusec,
        )),
        run,
    })
}

#[cfg(target_os = "linux")]
fn list_pids() -> Vec<Pid> {
    let Ok(dir) = std::fs::read_dir("/proc") else {
        return Vec::new();
    };
    dir.filter_map(|e| e.ok()?.file_name().to_str()?.parse::<Pid>().ok())
        .collect()
}

#[cfg(target_os = "linux")]
fn read_one(pid: Pid) -> Option<Proc> {
    let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    parse_stat(pid, &stat)
}

/// `/proc/<pid>/stat`：第 2 个字段是括起来的程序名，里面可以有空格和括号，所以从最后一个 `)` 往后数。
/// 之后依次是状态（第 3 个字段）、父进程号、组号、会话号……出生时间是第 22 个。
#[cfg(any(target_os = "linux", test))]
fn parse_stat(pid: Pid, stat: &str) -> Option<Proc> {
    let rest = &stat[stat.rfind(')')? + 1..];
    let f: Vec<&str> = rest.split_whitespace().collect();
    let field = |n: usize| f.get(n - 3).copied();
    let run = match field(3)? {
        "Z" | "X" | "x" => return None,
        "T" | "t" => Run::Stopped,
        _ => Run::Running,
    };
    Some(Proc {
        pid,
        ppid: field(4)?.parse().ok()?,
        pgid: field(5)?.parse().ok()?,
        sid: field(6)?.parse().ok().filter(|s: &Pid| *s > 0),
        birth: field(22).and_then(|s| s.parse().ok()).map(Birth),
        run,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stat_line_with_parens_in_the_name() {
        let line = "4242 (a b) (c)) T 1 4242 4242 0 -1 4194560 1 0 0 0 0 0 0 0 20 0 1 0 987654 0 0";
        let p = parse_stat(4242, line).unwrap();
        assert_eq!(
            p,
            Proc {
                pid: 4242,
                ppid: 1,
                pgid: 4242,
                sid: Some(4242),
                birth: Some(Birth(987654)),
                run: Run::Stopped,
            }
        );
        assert_eq!(
            parse_stat(1, "1 (x) Z 0 1 1 0 -1 0 0 0 0 0 0 0 0 0 20 0 1 0 5"),
            None
        );
    }

    /// 自己读得到：父进程号、会话号对得上系统调用，出生时间读得到；复核和读表给的一样。
    #[test]
    fn reads_this_process() {
        let me = std::process::id() as Pid;
        let table = read_table();
        let p = table.0.get(&me).expect("表里有自己");
        // SAFETY: 都只读整数
        let (ppid, sid, pgid) = unsafe { (libc::getppid(), libc::getsid(0), libc::getpgrp()) };
        assert_eq!(
            (p.ppid, p.sid, p.pgid, p.run),
            (ppid, Some(sid), pgid, Run::Running)
        );
        assert!(p.birth.is_some());
        assert_eq!(
            recheck(&[me]),
            Recheck::from([(me, Some((p.birth.unwrap(), sid)))])
        );
        assert!(table.0.len() > 1, "列得到别的进程");
    }
}
