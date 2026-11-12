//! 启动状态文件的唯一写入方（T04 方案 2.2「会合点」）。只有持锁的进程有它；拿到锁才算状态文件的主人。
//!
//! 每次改动都整份重写（临时文件 + 改名）。写失败只记日志：状态文件只给人看，不能因为它让后台停下。

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};

use iwork_ipc::rendezvous::{self, ExeStamp, Phase, StartupStatus};

/// 状态文件的持有者。克隆共享同一份。
#[derive(Clone)]
pub struct StatusBoard {
    run_dir: PathBuf,
    current: Arc<Mutex<StartupStatus>>,
}

/// 状态文件里不随阶段变化的那几项。
pub struct StatusIdentity {
    pub version: String,
    pub protocol_major: u32,
    pub exe_stamp: ExeStamp,
    pub log_dir: PathBuf,
}

fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| i64::try_from(d.as_millis()).unwrap_or(i64::MAX))
}

impl StatusBoard {
    /// 建好并立刻写出第一份状态。
    pub fn create(run_dir: &Path, id: StatusIdentity, phase: Phase) -> Self {
        let status = StartupStatus {
            pid: std::process::id(),
            version: id.version,
            protocol_major: id.protocol_major,
            exe_stamp: id.exe_stamp,
            log_dir: id.log_dir,
            updated_at_ms: now_ms(),
            phase,
            ipc_faults: 0,
            not_cleared: 0,
        };
        let board = Self {
            run_dir: run_dir.to_path_buf(),
            current: Arc::new(Mutex::new(status)),
        };
        board.update(|_| {});
        board
    }

    pub fn set_phase(&self, phase: Phase) {
        self.update(|s| s.phase = phase);
    }

    pub fn phase(&self) -> Phase {
        self.lock().phase.clone()
    }

    pub fn snapshot(&self) -> StartupStatus {
        self.lock().clone()
    }

    pub fn add_ipc_fault(&self) {
        self.update(|s| s.ipc_faults = s.ipc_faults.saturating_add(1));
    }

    pub fn set_not_cleared(&self, n: u32) {
        self.update(|s| s.not_cleared = n);
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, StartupStatus> {
        self.current.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// 改内存里的那份并写盘。持锁写盘：两次更新不会以相反的顺序落盘。
    fn update(&self, f: impl FnOnce(&mut StartupStatus)) {
        let mut s = self.lock();
        f(&mut s);
        s.updated_at_ms = now_ms();
        if let Err(e) = rendezvous::write_status(&self.run_dir, &s) {
            tracing::warn!(error = %e, dir = %self.run_dir.display(), "写启动状态文件失败");
        }
    }
}
