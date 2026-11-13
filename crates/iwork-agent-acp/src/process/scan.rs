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
//! 别的错误（列不出进程号、描述符用完、格式认不出）让**整次扫描失败**，调用方不能把它当成一张空表
//! （审查 S6-5 1/F-4）。
//!
//! 会话号单独读不到（`getsid` 失败）时这一行照样进表，只是 `sid` 为 `None`：它还能按根组或父链进范围、
//! 被冻结与杀掉，但不会被记下，也不能按会话认领。
//!
//! 本机约 1200 个进程时一遍 2 毫秒左右；调用方（清树、采样）放在阻塞线程里做，并自己限时。

use std::io;

use super::scope::{Birth, Pid, Proc, Recheck, Run, Table};

/// 读一遍全机的进程表。
pub fn read_table() -> io::Result<Table> {
    let mut procs = Vec::new();
    for pid in list_pids()? {
        if let Some(p) = read_one(pid)? {
            procs.push(p);
        }
    }
    Ok(Table::from_procs(procs))
}

/// 读完进程表之后复核见证：每个进程号此刻的（出生时间，会话号）。没了为 `None`；说不清的错误让整次复核失败。
pub fn recheck(pids: &[Pid]) -> io::Result<Recheck> {
    pids.iter()
        .map(|pid| Ok((*pid, read_one(*pid)?.and_then(|p| Some((p.birth?, p.sid?))))))
        .collect()
}

/// 一个进程此刻的出生时间（发信号前的身份复核用）。没了、僵尸、没权限、读不到都是 `None`。
pub fn birth_of(pid: Pid) -> Option<Birth> {
    read_one(pid).ok().flatten().and_then(|p| p.birth)
}

/// 读一个进程时可以跳过的错误：进程没了（含读的那一刻退出、僵尸），或者不许读（别的用户的）。
fn skippable(e: &io::Error) -> bool {
    e.kind() == io::ErrorKind::NotFound
        || matches!(
            e.raw_os_error(),
            Some(libc::ESRCH | libc::EPERM | libc::EACCES | libc::ENOENT)
        )
}

#[cfg(target_os = "macos")]
fn list_pids() -> io::Result<Vec<Pid>> {
    // 先问个数，多留些余量；系统填满了余量（进程在这期间变多了）就加大再来
    // SAFETY: 缓冲区为空、长度为 0 时只返回个数
    let first = unsafe { libc::proc_listallpids(std::ptr::null_mut(), 0) };
    if first < 0 {
        return Err(io::Error::last_os_error());
    }
    let mut cap = first as usize + 256;
    loop {
        let mut buf: Vec<Pid> = vec![0; cap];
        let bytes = i32::try_from(cap * std::mem::size_of::<Pid>()).unwrap_or(i32::MAX);
        // SAFETY: 缓冲区与长度描述同一块活着的内存
        let n = unsafe { libc::proc_listallpids(buf.as_mut_ptr().cast(), bytes) };
        if n < 0 {
            return Err(io::Error::last_os_error());
        }
        let n = n as usize;
        if n < cap {
            buf.truncate(n);
            return Ok(buf);
        }
        cap *= 2;
    }
}

/// 一次 `proc_pidinfo(PROC_PIDTBSDINFO)` 读到的东西（不含会话号）。
#[cfg(any(target_os = "macos", test))]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Bsd {
    pub ppid: Pid,
    pub pgid: Pid,
    pub birth: Birth,
    pub run: Run,
}

/// 读一个进程要用到的两样系统调用，测试换成替身（审查 S6-5 5/F-1）。
#[cfg(any(target_os = "macos", test))]
pub trait ProcSys {
    /// 进程信息；没了、僵尸、没权限为 `None`；别的错误报出去
    fn bsd(&self, pid: Pid) -> io::Result<Option<Bsd>>;
    /// 会话号；读不到为 `None`
    fn sid(&self, pid: Pid) -> Option<Pid>;
}

/// 读一次进程最多重来几遍：两次出生时间对不上（号码在读的当中换了人）就重读。
#[cfg(any(target_os = "macos", test))]
const CONSISTENT_TRIES: usize = 3;

/// 读一个进程，保证会话号和出生时间属于同一个身份（审查 S6-5 5/F-1）。macOS 上出生时间和会话号来自两次系统调用，
/// 两次之间号码可以换人：先读到旧进程的出生时间、再读到新进程的会话号，拼成一个假见证。所以 `getsid` 前后各读一次
/// 出生时间，两次相同才用；不同就重读，重读几遍还对不上，这一整遍扫描算没做成（报错）：不只是跳过这一行，因为换上来
/// 的那个进程也可能是我们的，只跳过它，这一遍可能被拿去判「清空了」。
#[cfg(any(target_os = "macos", test))]
pub fn read_consistent(sys: &impl ProcSys, pid: Pid) -> io::Result<Option<Proc>> {
    if pid <= 0 {
        return Ok(None);
    }
    for _ in 0..CONSISTENT_TRIES {
        let Some(before) = sys.bsd(pid)? else {
            return Ok(None);
        };
        let sid = sys.sid(pid);
        let Some(after) = sys.bsd(pid)? else {
            return Ok(None);
        };
        if before.birth == after.birth {
            return Ok(Some(Proc {
                pid,
                ppid: after.ppid,
                pgid: after.pgid,
                sid,
                birth: Some(after.birth),
                run: after.run,
            }));
        }
    }
    Err(io::Error::other(format!(
        "进程 {pid} 在读的当中一直换人，这一遍扫描不算"
    )))
}

/// 真的系统（macOS）。
#[cfg(target_os = "macos")]
struct MacSys;

#[cfg(target_os = "macos")]
impl ProcSys for MacSys {
    fn bsd(&self, pid: Pid) -> io::Result<Option<Bsd>> {
        let size = std::mem::size_of::<libc::proc_bsdinfo>();
        // SAFETY: 全零是这个纯数据结构体的合法初值
        let mut info: libc::proc_bsdinfo = unsafe { std::mem::zeroed() };
        // SAFETY: 传入的指针与长度描述同一个活着的结构体
        let n = unsafe {
            libc::proc_pidinfo(
                pid,
                libc::PROC_PIDTBSDINFO,
                0,
                (&raw mut info).cast(),
                size as libc::c_int,
            )
        };
        if n as usize != size {
            let e = io::Error::last_os_error();
            // macOS 对僵尸报 ESRCH
            return if n <= 0 && skippable(&e) {
                Ok(None)
            } else {
                Err(io::Error::new(e.kind(), format!("读不了进程 {pid}：{e}")))
            };
        }
        let run = match info.pbi_status {
            libc::SZOMB => return Ok(None),
            libc::SSTOP => Run::Stopped,
            _ => Run::Running,
        };
        Ok(Some(Bsd {
            ppid: info.pbi_ppid as Pid,
            pgid: info.pbi_pgid as Pid,
            birth: Birth(info.pbi_start_tvsec * 1_000_000 + info.pbi_start_tvusec),
            run,
        }))
    }

    fn sid(&self, pid: Pid) -> Option<Pid> {
        // SAFETY: getsid 只读一个整数参数
        let sid = unsafe { libc::getsid(pid) };
        (sid > 0).then_some(sid)
    }
}

/// 读一个进程：读到了为 `Some`；没了、僵尸、没权限为 `None`；别的错误报出去。
#[cfg(target_os = "macos")]
fn read_one(pid: Pid) -> io::Result<Option<Proc>> {
    read_consistent(&MacSys, pid)
}

#[cfg(target_os = "linux")]
fn list_pids() -> io::Result<Vec<Pid>> {
    let mut pids = Vec::new();
    for entry in std::fs::read_dir("/proc")? {
        if let Some(pid) = entry?
            .file_name()
            .to_str()
            .and_then(|n| n.parse::<Pid>().ok())
        {
            pids.push(pid);
        }
    }
    Ok(pids)
}

/// Linux：出生时间（starttime）和会话号在同一个文件里，一次 `read` 读出来；内核生成这份内容时拿的是同一个进程的
/// 结构，所以两者天然属于同一个身份，不会像 macOS 那样两次系统调用之间换人（审查 S6-5 5/F-1）。
#[cfg(target_os = "linux")]
fn read_one(pid: Pid) -> io::Result<Option<Proc>> {
    match std::fs::read_to_string(format!("/proc/{pid}/stat")) {
        Ok(stat) => parse_stat(pid, &stat),
        Err(e) if skippable(&e) => Ok(None),
        Err(e) => Err(io::Error::new(
            e.kind(),
            format!("读不了 /proc/{pid}/stat：{e}"),
        )),
    }
}

/// `/proc/<pid>/stat`：第 2 个字段是括起来的程序名，里面可以有空格和括号，所以从最后一个 `)` 往后数。
/// 之后依次是状态（第 3 个字段）、父进程号、组号、会话号……出生时间是第 22 个。僵尸为 `None`；格式认不出报错。
#[cfg(any(target_os = "linux", test))]
fn parse_stat(pid: Pid, stat: &str) -> io::Result<Option<Proc>> {
    let bad = || {
        io::Error::new(
            io::ErrorKind::InvalidData,
            format!("/proc/{pid}/stat 的格式认不出"),
        )
    };
    let rest = &stat[stat.rfind(')').ok_or_else(bad)? + 1..];
    let f: Vec<&str> = rest.split_whitespace().collect();
    let field = |n: usize| f.get(n - 3).copied().ok_or_else(bad);
    let num = |n: usize| field(n)?.parse::<Pid>().map_err(|_| bad());
    let run = match field(3)? {
        "Z" | "X" | "x" => return Ok(None),
        "T" | "t" => Run::Stopped,
        _ => Run::Running,
    };
    Ok(Some(Proc {
        pid,
        ppid: num(4)?,
        pgid: num(5)?,
        sid: Some(num(6)?).filter(|s| *s > 0),
        birth: Some(Birth(field(22)?.parse().map_err(|_| bad())?)),
        run,
    }))
}

#[cfg(test)]
pub(super) mod tests {
    use super::*;

    /// 替身：按次序吐出预先排好的回答。
    pub struct Scripted {
        pub bsd: std::cell::RefCell<std::collections::VecDeque<Option<Bsd>>>,
        pub sid: std::cell::RefCell<std::collections::VecDeque<Option<Pid>>>,
    }

    impl Scripted {
        pub fn new(bsd: Vec<Option<Bsd>>, sid: Vec<Option<Pid>>) -> Self {
            Self {
                bsd: std::cell::RefCell::new(bsd.into()),
                sid: std::cell::RefCell::new(sid.into()),
            }
        }
    }

    impl ProcSys for Scripted {
        fn bsd(&self, _pid: Pid) -> io::Result<Option<Bsd>> {
            Ok(self.bsd.borrow_mut().pop_front().expect("脚本里还有 bsd"))
        }
        fn sid(&self, _pid: Pid) -> Option<Pid> {
            self.sid.borrow_mut().pop_front().expect("脚本里还有 sid")
        }
    }

    pub fn bsd(birth: u64) -> Option<Bsd> {
        Some(Bsd {
            ppid: 1,
            pgid: 21,
            birth: Birth(birth),
            run: Run::Running,
        })
    }

    /// 5/F-1：getsid 前后出生时间相同才用。
    #[test]
    fn sid_and_birth_come_from_one_identity() {
        // 没换人：直接用
        let got = read_consistent(&Scripted::new(vec![bsd(21), bsd(21)], vec![Some(21)]), 21);
        assert_eq!(
            got.unwrap().map(|p| (p.birth, p.sid)),
            Some((Some(Birth(21)), Some(21)))
        );
        // 读到旧进程的出生时间之后号码换了人，getsid 读到新进程的会话号：这一次不用，重读拿到新进程自己的一致结果，
        // 拼不出「旧出生时间 + 新会话号」
        let sys = Scripted::new(
            vec![bsd(21), bsd(999), bsd(999), bsd(999)],
            vec![Some(20), Some(20)],
        );
        let got = read_consistent(&sys, 21).unwrap().unwrap();
        assert_eq!((got.birth, got.sid), (Some(Birth(999)), Some(20)));
        // 一直在换：这一遍扫描算没做成
        let sys = Scripted::new(
            (0..CONSISTENT_TRIES * 2)
                .map(|i| bsd(100 + i as u64))
                .collect(),
            vec![Some(20); CONSISTENT_TRIES],
        );
        assert!(read_consistent(&sys, 21).is_err());
        // 读到一半没了：当它没了
        let sys = Scripted::new(vec![bsd(21), None], vec![Some(21)]);
        assert_eq!(read_consistent(&sys, 21).unwrap(), None);
    }

    #[test]
    fn stat_line_with_parens_in_the_name() {
        let line = "4242 (a b) (c)) T 1 4242 4242 0 -1 4194560 1 0 0 0 0 0 0 0 20 0 1 0 987654 0 0";
        let p = parse_stat(4242, line).unwrap().unwrap();
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
        let zombie = "1 (x) Z 0 1 1 0 -1 0 0 0 0 0 0 0 0 0 20 0 1 0 5";
        assert_eq!(parse_stat(1, zombie).unwrap(), None);
        // 认不出的格式不当成「没有这个进程」
        assert!(parse_stat(1, "1 (x) R 0").is_err());
        assert!(parse_stat(1, "garbage").is_err());
    }

    /// 没了、僵尸、没权限可以跳过；别的错误（如描述符用完）让整次扫描失败。
    #[test]
    fn only_gone_or_forbidden_is_skipped() {
        for errno in [libc::ESRCH, libc::EPERM, libc::EACCES, libc::ENOENT] {
            assert!(skippable(&io::Error::from_raw_os_error(errno)), "{errno}");
        }
        for errno in [libc::EMFILE, libc::ENFILE, libc::EIO, libc::ENOMEM] {
            assert!(!skippable(&io::Error::from_raw_os_error(errno)), "{errno}");
        }
    }

    /// 自己读得到：父进程号、会话号对得上系统调用，出生时间读得到；复核和读表给的一样。
    #[test]
    fn reads_this_process() {
        let me = std::process::id() as Pid;
        let table = read_table().unwrap();
        let p = table.0.get(&me).expect("表里有自己");
        // SAFETY: 都只读整数
        let (ppid, sid, pgid) = unsafe { (libc::getppid(), libc::getsid(0), libc::getpgrp()) };
        assert_eq!(
            (p.ppid, p.sid, p.pgid, p.run),
            (ppid, Some(sid), pgid, Run::Running)
        );
        assert!(p.birth.is_some());
        assert_eq!(birth_of(me), p.birth);
        assert_eq!(
            recheck(&[me]).unwrap(),
            Recheck::from([(me, Some((p.birth.unwrap(), sid)))])
        );
        assert!(table.0.len() > 1, "列得到别的进程");
    }
}
