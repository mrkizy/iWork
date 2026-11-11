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
        use std::os::unix::fs::OpenOptionsExt;
        opts.mode(0o600);
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
        assert!(matches!(
            acquire(&a, &b, &info(9)).unwrap(),
            LockOutcome::Acquired(_)
        ));
    }

    #[test]
    fn two_dirs_two_locks() {
        let (_t, a, b) = dirs();
        assert_eq!(lock_paths(&a, &b).unwrap().len(), 2);
        let _l = acquire(&a, &b, &info(1)).unwrap();
        assert!(a.join(LOCK_FILE).exists());
        assert_eq!(rendezvous::read_lock_info(&b).unwrap().pid, 1);
    }

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
        // 数据目录那一把是空的
        let probe = open_lock_file(&a.join(LOCK_FILE)).unwrap();
        probe.try_lock().expect("第一把应已放开");
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
