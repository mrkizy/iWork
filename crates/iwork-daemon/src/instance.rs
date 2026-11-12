//! 单实例锁与托管实例的等锁接班（T04 方案 2.3；设计 4 第 2 条）。
//!
//! 锁要护两样东西：数据库（迁移探测、写线程都假设没有第二个进程在写）与 socket（`bind` 会删残留文件）。
//! 两样东西可能不在一个目录，所以谁的资源旁边放谁的锁；两个目录规范化后相同就只有一把。
//! 锁是操作系统的锁（Unix `flock`），进程结束（含被杀）自动放开；锁文件从不删除——删了再建会出现
//! 「两个进程各锁一个 inode」。

use std::fs::{File, OpenOptions};
use std::io;
use std::path::{Path, PathBuf};
use std::time::Duration;

use iwork_ipc::rendezvous::{
    self, ExeStamp, LOCK_FILE, LockInfo, Relaunchable, StartupStatus, relaunchable,
};

/// 持有期间别的后台服务起不来。
#[derive(Debug)]
pub struct InstanceLock {
    _files: Vec<File>,
}

#[derive(Debug)]
pub enum LockOutcome {
    Acquired(InstanceLock),
    /// 有别的实例拿着；带上锁文件里记的 pid 与版本（只是提示）。
    HeldByOther(Option<LockInfo>),
}

/// 锁文件清单：先数据目录 `run/` 那一把，再 socket 目录那一把；规范化后相同就只留一个。
/// 两个目录都必须已经存在（端点与数据目录那一步建好了）。
pub fn lock_paths(data_run: &Path, socket_run: &Path) -> io::Result<Vec<PathBuf>> {
    let data = std::fs::canonicalize(data_run)?;
    let socket = std::fs::canonicalize(socket_run)?;
    let mut paths = vec![data.join(LOCK_FILE)];
    if socket != data {
        paths.push(socket.join(LOCK_FILE));
    }
    Ok(paths)
}

fn open_lock_file(path: &Path) -> io::Result<File> {
    let mut opts = OpenOptions::new();
    opts.create(true).truncate(false).read(true).write(true);
    #[cfg(unix)]
    {
        // 不跟随链接：之后要往里写 pid 与版本（write_lock_info），不能顺着别人放好的链接写到别处
        use std::os::unix::fs::OpenOptionsExt;
        opts.mode(0o600).custom_flags(libc::O_NOFOLLOW);
    }
    opts.open(path)
}

/// 按清单顺序 `try_lock`。任何一把拿不到：放掉已拿到的（随 `File` 丢弃），返回 `HeldByOther`，
/// 不碰状态文件、不碰 socket。全部拿到后把 `info` 写进 socket 目录那一把（清单最后一项）。
pub fn acquire(data_run: &Path, socket_run: &Path, info: &LockInfo) -> io::Result<LockOutcome> {
    let paths = lock_paths(data_run, socket_run)?;
    let mut files = Vec::with_capacity(paths.len());
    for path in &paths {
        let file = open_lock_file(path)?;
        match file.try_lock() {
            Ok(()) => files.push(file),
            Err(std::fs::TryLockError::WouldBlock) => {
                return Ok(LockOutcome::HeldByOther(rendezvous::read_lock_info(
                    socket_run,
                )));
            }
            Err(std::fs::TryLockError::Error(e)) => return Err(e),
        }
    }
    let last = files.last_mut().expect("锁清单至少有一项");
    rendezvous::write_lock_info(last, info)?;
    Ok(LockOutcome::Acquired(InstanceLock { _files: files }))
}

/// 托管实例撞锁后：阻塞等清单第一把（不占 CPU），拿到后对其余的 `try_lock`；有拿不到的就全部放掉，
/// `retry` 后从头再等。返回时全部锁都在手里。不写锁文件内容：接班就 `exec`，不接就退出，都轮不到它写。
pub fn wait_for_all(
    data_run: &Path,
    socket_run: &Path,
    retry: Duration,
) -> io::Result<InstanceLock> {
    let paths = lock_paths(data_run, socket_run)?;
    loop {
        let first = open_lock_file(&paths[0])?;
        first.lock()?;
        let mut files = vec![first];
        let mut all = true;
        for path in &paths[1..] {
            let file = open_lock_file(path)?;
            match file.try_lock() {
                Ok(()) => files.push(file),
                Err(std::fs::TryLockError::WouldBlock) => {
                    all = false;
                    break;
                }
                Err(std::fs::TryLockError::Error(e)) => return Err(e),
            }
        }
        if all {
            return Ok(InstanceLock { _files: files });
        }
        drop(files);
        std::thread::sleep(retry);
    }
}

/// 等锁的托管实例拿到锁之后做什么。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StandbyAction {
    /// 接班：原地 `exec` 自己，从启动表第 1 步走。
    TakeOver,
    /// 不接：以 0 退出。
    StandDown,
}

/// 读前任留下的状态，照 2.2 的角色表「等锁的托管实例」一列做。这里没有自己的判断：只查 `relaunchable`。
/// 读不到状态文件按 `Yes`。`disk_exe` 是现在磁盘上程序的戳（取不到为 `None`，按「换过了」）。
pub fn standby_action(prev: Option<&StartupStatus>, disk_exe: Option<ExeStamp>) -> StandbyAction {
    let Some(prev) = prev else {
        return StandbyAction::TakeOver;
    };
    match relaunchable(&prev.phase) {
        Relaunchable::Yes => StandbyAction::TakeOver,
        Relaunchable::NotUntilUserReturns => StandbyAction::StandDown,
        Relaunchable::NotWithThisProgram => {
            if disk_exe == Some(prev.exe_stamp) {
                StandbyAction::StandDown
            } else {
                StandbyAction::TakeOver
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use iwork_ipc::rendezvous::{ExitCode, Phase, Refusal, StopReason};

    /// 锁文件的位置上是指向别处的链接 → 打开失败（不会往链接指向的文件里写 pid），被指向的文件不变。
    #[cfg(unix)]
    #[test]
    fn lock_file_symlink_is_refused() {
        let d = tempfile::tempdir().unwrap();
        let victim = d.path().join("victim");
        std::fs::write(&victim, "keep me").unwrap();
        std::os::unix::fs::symlink(&victim, d.path().join(LOCK_FILE)).unwrap();
        assert!(acquire(d.path(), d.path(), &info(1)).is_err());
        assert_eq!(std::fs::read_to_string(&victim).unwrap(), "keep me");
    }

    fn info(pid: u32) -> LockInfo {
        LockInfo {
            pid,
            version: "t".into(),
            started_at_ms: 1,
        }
    }

    fn dirs() -> (tempfile::TempDir, PathBuf, PathBuf) {
        let t = tempfile::tempdir().unwrap();
        let a = t.path().join("data/run");
        let b = t.path().join("sock");
        std::fs::create_dir_all(&a).unwrap();
        std::fs::create_dir_all(&b).unwrap();
        (t, a, b)
    }

    /// 放掉锁之后反复试拿，直到拿到（最多 10s）。`flock` 挂在「打开的文件」上：同一测试进程里别的测试正在起子进程时，
    /// 子进程在 exec 之前复制着锁文件的句柄（锁文件都带 `O_CLOEXEC`，exec 时才关），这段时间里锁还被那份副本拿着
    /// （`forked_copy_keeps_lock_until_it_closes` 确定性地复现了这一点）。所以「放掉之后马上就拿得到」在多线程的
    /// 测试进程里不成立，只能断言「很快拿得到」。
    #[cfg(unix)]
    fn acquire_eventually(a: &Path, b: &Path, pid: u32) -> InstanceLock {
        let deadline = std::time::Instant::now() + Duration::from_secs(10);
        loop {
            match acquire(a, b, &info(pid)).unwrap() {
                LockOutcome::Acquired(l) => return l,
                LockOutcome::HeldByOther(_) => {
                    assert!(
                        std::time::Instant::now() < deadline,
                        "放掉之后 10s 内没拿到锁"
                    );
                    std::thread::sleep(Duration::from_millis(10));
                }
            }
        }
    }

    // 读持锁中的锁文件内容：Windows 的 LockFileEx 是强制锁，持锁时别的句柄读不了；M1 不支持 Windows（T03）
    #[cfg(unix)]
    #[test]
    fn second_acquire_is_held_by_other() {
        let (_t, a, b) = dirs();
        let first = acquire(&a, &b, &info(7)).unwrap();
        assert!(matches!(first, LockOutcome::Acquired(_)));
        match acquire(&a, &b, &info(8)).unwrap() {
            LockOutcome::HeldByOther(Some(i)) => assert_eq!(i.pid, 7),
            other => panic!("{other:?}"),
        }
        drop(first);
        acquire_eventually(&a, &b, 9);
    }

    /// 锁文件的句柄都带 `FD_CLOEXEC`：后台起的子进程（成员 CLI、桥接、git）exec 之后不再握着单实例锁。
    #[cfg(unix)]
    #[test]
    fn lock_files_are_close_on_exec() {
        use std::os::fd::AsRawFd;
        let (_t, a, b) = dirs();
        let LockOutcome::Acquired(lock) = acquire(&a, &b, &info(1)).unwrap() else {
            panic!("拿不到锁");
        };
        assert_eq!(lock._files.len(), 2);
        for f in &lock._files {
            // SAFETY：只读 fd 标志
            let flags = unsafe { libc::fcntl(f.as_raw_fd(), libc::F_GETFD) };
            assert!(
                flags >= 0 && flags & libc::FD_CLOEXEC != 0,
                "锁文件句柄没带 FD_CLOEXEC"
            );
        }
    }

    /// 确定性复现 4ea1efca 那次偶发：fork 出一个不 exec 的子进程（相当于别的线程起子进程、还没走到 exec 的那一刻），
    /// 它复制着锁文件的句柄 → 本进程放掉锁之后，锁还被副本拿着；子进程退出、副本关掉，锁才放开。
    #[cfg(unix)]
    #[test]
    fn forked_copy_keeps_lock_until_it_closes() {
        let (_t, a, b) = dirs();
        let first = acquire(&a, &b, &info(1)).unwrap();
        // SAFETY：子进程只调 async-signal-safe 的 pause 与 _exit
        let child = unsafe { libc::fork() };
        assert!(child >= 0, "fork 失败");
        if child == 0 {
            loop {
                unsafe { libc::pause() };
            }
        }
        drop(first);
        let held = acquire(&a, &b, &info(2)).unwrap();
        // SAFETY：只对自己 fork 的子进程发信号、收尸
        unsafe {
            libc::kill(child, libc::SIGKILL);
            libc::waitpid(child, std::ptr::null_mut(), 0);
        }
        assert!(
            matches!(held, LockOutcome::HeldByOther(_)),
            "子进程握着副本时锁不该放开：{held:?}"
        );
        // 自己的子进程已收尸，它那份副本关了；但同一测试进程里别的测试可能在 `first` 持锁期间 fork 了、还没 exec，
        // 它们的副本还握着锁，所以这里也只能断言「很快拿得到」（第 1 轮审查 1/F-1）
        acquire_eventually(&a, &b, 3);
    }

    /// 已经 exec 完的子进程不握锁：锁文件句柄随 `O_CLOEXEC` 在 exec 时关掉，放掉之后马上拿得到。
    /// （`Command::spawn` 返回时 exec 已经成功。）
    #[cfg(unix)]
    #[test]
    fn execed_child_does_not_keep_lock() {
        let (_t, a, b) = dirs();
        let first = acquire(&a, &b, &info(1)).unwrap();
        let mut child = std::process::Command::new("sleep")
            .arg("30")
            .spawn()
            .unwrap();
        drop(first);
        // 本测试自己的子进程（睡 30s）已 exec：它不握锁。同一测试进程里别的测试正好 fork 出的副本会在它们 exec 时
        // 关掉，所以拿不到时再等一会儿；要是自己的子进程握着锁（句柄没带 CLOEXEC），10s 内一直拿不到，测试失败
        if let LockOutcome::HeldByOther(_) = acquire(&a, &b, &info(2)).unwrap() {
            acquire_eventually(&a, &b, 3);
        }
        let _ = child.kill();
        let _ = child.wait();
    }

    // 读持锁中的锁文件内容：Windows 的 LockFileEx 是强制锁，持锁时别的句柄读不了；M1 不支持 Windows（T03）
    #[cfg(unix)]
    #[test]
    fn two_dirs_two_locks() {
        let (_t, a, b) = dirs();
        assert_eq!(lock_paths(&a, &b).unwrap().len(), 2);
        let _l = acquire(&a, &b, &info(1)).unwrap();
        assert!(a.join(LOCK_FILE).exists());
        assert_eq!(rendezvous::read_lock_info(&b).unwrap().pid, 1);
    }

    // 读持锁中的锁文件内容：Windows 的 LockFileEx 是强制锁，持锁时别的句柄读不了；M1 不支持 Windows（T03）
    #[cfg(unix)]
    #[test]
    fn same_dir_one_lock() {
        let (_t, a, _b) = dirs();
        // 经过一个 `..` 指回同一目录：规范化后相同
        let alias = a.join("../run");
        assert_eq!(lock_paths(&a, &alias).unwrap().len(), 1);
        let _l = acquire(&a, &alias, &info(3)).unwrap();
        assert_eq!(rendezvous::read_lock_info(&a).unwrap().pid, 3);
    }

    /// 第二把拿不到时第一把要放掉：否则一个半途失败的实例会挡住数据库。
    #[test]
    fn second_lock_failure_releases_first() {
        let (_t, a, b) = dirs();
        // 别人只拿着 socket 目录那一把
        let other = open_lock_file(&b.join(LOCK_FILE)).unwrap();
        other.try_lock().unwrap();
        assert!(matches!(
            acquire(&a, &b, &info(1)).unwrap(),
            LockOutcome::HeldByOther(_)
        ));
        // 数据目录那一把是空的（同 `acquire_eventually`：别的测试 fork 时的副本可能晚一点才关）
        let probe = open_lock_file(&a.join(LOCK_FILE)).unwrap();
        let deadline = std::time::Instant::now() + Duration::from_secs(10);
        while probe.try_lock().is_err() {
            assert!(std::time::Instant::now() < deadline, "第一把应已放开");
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    #[test]
    fn wait_for_all_returns_once_released() {
        let (_t, a, b) = dirs();
        let held = acquire(&a, &b, &info(1)).unwrap();
        let (a2, b2) = (a.clone(), b.clone());
        let waiter =
            std::thread::spawn(move || wait_for_all(&a2, &b2, Duration::from_millis(10)).unwrap());
        std::thread::sleep(Duration::from_millis(100));
        assert!(!waiter.is_finished(), "前任还在时不该拿到");
        drop(held);
        let _lock = waiter.join().unwrap();
        assert!(matches!(
            acquire(&a, &b, &info(2)).unwrap(),
            LockOutcome::HeldByOther(_)
        ));
    }

    fn status(phase: Phase, stamp: ExeStamp) -> StartupStatus {
        StartupStatus {
            pid: 1,
            version: "v".into(),
            protocol_major: 1,
            exe_stamp: stamp,
            log_dir: PathBuf::from("/l"),
            updated_at_ms: 0,
            phase,
            ipc_faults: 0,
            not_cleared: 0,
        }
    }

    /// 对每一种 Phase，等锁实例的动作等于角色表按 `relaunchable` 查出来的那一格：没有自己的判断。
    #[test]
    fn standby_has_no_own_rule() {
        let same = ExeStamp {
            len: 10,
            modified_unix_ns: 5,
        };
        let other = ExeStamp {
            len: 11,
            modified_unix_ns: 5,
        };
        let mut phases = vec![
            Phase::Starting { step: "x".into() },
            Phase::Ready,
            Phase::Refused(Refusal::StoreOpenFailed {
                message: "m".into(),
            }),
        ];
        for r in [
            StopReason::UserQuit,
            StopReason::Signal,
            StopReason::Upgrade,
            StopReason::Fatal,
        ] {
            phases.push(Phase::Stopping { reason: r });
            phases.push(Phase::Exited {
                code: ExitCode::Stopped,
                reason: Some(r),
                message: None,
            });
        }
        for code in ExitCode::ALL {
            phases.push(crate::exit::exited(code));
        }
        for phase in phases {
            for disk in [Some(same), Some(other), None] {
                let s = status(phase.clone(), same);
                let want = match relaunchable(&phase) {
                    Relaunchable::Yes => StandbyAction::TakeOver,
                    Relaunchable::NotUntilUserReturns => StandbyAction::StandDown,
                    Relaunchable::NotWithThisProgram if disk == Some(same) => {
                        StandbyAction::StandDown
                    }
                    Relaunchable::NotWithThisProgram => StandbyAction::TakeOver,
                };
                assert_eq!(standby_action(Some(&s), disk), want, "{phase:?} {disk:?}");
            }
        }
        assert_eq!(standby_action(None, Some(same)), StandbyAction::TakeOver);
    }
}
