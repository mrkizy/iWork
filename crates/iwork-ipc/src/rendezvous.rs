//! 会合点：客户端与后台在 socket 目录（[`crate::LocalEndpoint::run_dir`]）里互通「谁在跑、为什么起不来」
//! （T04 方案 2.2）。放在 ipc 而不是存储里：客户端不依赖存储也算得出这个目录。
//!
//! 两个文件都**只给人看、只允许程序得出「少做一次」的结论**（方案规则 R4）：读不到、解析不了一律当作没有。
//! 「再拉有没有用」只有一个判据 [`relaunchable`]，托管实例的退出状态、等锁实例接不接班、客户端拉不拉都调它。

use std::fs::File;
use std::io::{self, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::time::UNIX_EPOCH;

use serde::{Deserialize, Serialize};

/// 单实例锁文件名（socket 目录那一把；内容是 [`LockInfo`]）。
pub const LOCK_FILE: &str = "daemon.lock";
/// 启动状态文件名（内容是 [`StartupStatus`]）。
pub const STATUS_FILE: &str = "daemon.status.json";

/// 锁文件的内容：拿到锁的进程写入。客户端只读它，**绝不对这个文件加锁**。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LockInfo {
    pub pid: u32,
    pub version: String,
    pub started_at_ms: i64,
}

/// 程序文件的大小与修改时间。进程一启动就取，只用来回答「起不来的是不是现在装的这一版」。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct ExeStamp {
    pub len: u64,
    /// 修改时间，自 Unix 纪元的纳秒；早于纪元的按 0。
    pub modified_unix_ns: u64,
}

impl ExeStamp {
    /// 读 `path` 的元数据（跟随符号链接）。
    pub fn of(path: &Path) -> io::Result<ExeStamp> {
        let meta = std::fs::metadata(path)?;
        let modified_unix_ns = meta
            .modified()?
            .duration_since(UNIX_EPOCH)
            .map_or(0, |d| u64::try_from(d.as_nanos()).unwrap_or(u64::MAX));
        Ok(ExeStamp {
            len: meta.len(),
            modified_unix_ns,
        })
    }
}

/// 启动状态：只有持锁的进程写。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StartupStatus {
    pub pid: u32,
    pub version: String,
    pub protocol_major: u32,
    /// 写这份状态的程序文件，进程一启动就取（不等写状态时再取：那时文件可能已经被换掉）。
    pub exe_stamp: ExeStamp,
    pub log_dir: PathBuf,
    pub updated_at_ms: i64,
    pub phase: Phase,
    /// ipc 上报的后台故障累计，诊断页显示。
    pub ipc_faults: u32,
    /// 停机时到点还没清空的会话数。只给诊断页，不参与任何判断。
    pub not_cleared: u32,
}

/// 进程走到哪一步。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum Phase {
    /// `step` 是启动表里的步骤名。
    Starting {
        step: String,
    },
    Ready,
    /// 停机第 1 步就写下：从这一刻起「再拉有没有用」就定了，之后卡住或被强退都不变。
    Stopping {
        reason: StopReason,
    },
    /// 存储拒绝打开（逻辑码 12）。只装「这一版程序起不来」这一类。
    Refused(Refusal),
    /// `reason` 是停机原因（启动阶段就退出的没有）；`message` 给诊断页。
    Exited {
        code: ExitCode,
        reason: Option<StopReason>,
        message: Option<String>,
    },
}

/// 停机原因。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum StopReason {
    /// 用户完全退出。
    UserQuit,
    /// 收到 SIGTERM / SIGINT / SIGHUP。
    Signal,
    /// 自行重启切换到磁盘上的新版本。
    Upgrade,
    /// 运行中的不可恢复故障。
    Fatal,
}

/// 存储拒绝打开的原因。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum Refusal {
    ProtocolMajorMismatch {
        database: u32,
        program: u32,
        backup: PathBuf,
    },
    /// schema 比程序新、不是 iWork 库、迁移失败、库里有不认识的事件种类：原文照录。
    StoreOpenFailed { message: String },
}

/// 「这个库被更新的版本写过」的标记字样：存储（schema 比程序新，T02）与后台第 5a 步（库里有不认识的事件种类）
/// 给出的拒绝原文里都带它。诊断页据此提示「装回更新的那一版」。
pub const WRITTEN_BY_NEWER: &str = "被更新的";

impl Refusal {
    /// 拒绝是不是因为库被更新的版本写过（装回更新的那一版就能用）。
    pub fn written_by_newer(&self) -> bool {
        match self {
            Refusal::StoreOpenFailed { message } => message.contains(WRITTEN_BY_NEWER),
            Refusal::ProtocolMajorMismatch { .. } => false,
        }
    }
}

/// 逻辑退出码。写进状态文件的是名字不是数字：新版本加了码而旧程序不认识时，整份状态解析不了、按「读不到」处理。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum ExitCode {
    /// 用户完全退出；收到信号后停下；发现已有实例（非托管）。
    Stopped,
    /// 参数不认识。
    Usage,
    /// 算不出自己的身份（含 M1 的 Windows）。
    Unsupported,
    /// 端点或数据目录不可用。
    EndpointRefused,
    /// 存储拒绝打开。
    StoreRefused,
    /// 拿到锁却绑不上 socket。
    BindFailed,
    /// 运行中（或启动阶段）的不可恢复故障。
    Fatal,
    /// 自行重启时 `exec` 失败。
    RestartRequested,
}

impl ExitCode {
    /// 全部逻辑退出码（测试与诊断页列举用）。
    pub const ALL: [ExitCode; 8] = [
        ExitCode::Stopped,
        ExitCode::Usage,
        ExitCode::Unsupported,
        ExitCode::EndpointRefused,
        ExitCode::StoreRefused,
        ExitCode::BindFailed,
        ExitCode::Fatal,
        ExitCode::RestartRequested,
    ];

    /// 进程退出状态里的数字（非托管实例就报它）。
    pub fn code(self) -> u8 {
        match self {
            ExitCode::Stopped => 0,
            ExitCode::Usage => 2,
            ExitCode::Unsupported => 10,
            ExitCode::EndpointRefused => 11,
            ExitCode::StoreRefused => 12,
            ExitCode::BindFailed => 13,
            ExitCode::Fatal => 20,
            ExitCode::RestartRequested => 21,
        }
    }
}

/// 「再拉有没有用」的三种答案。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Relaunchable {
    /// 再拉有用：故障、崩溃、升级。
    Yes,
    /// 用户或系统让它停下：自动的拉起方不拉；用户再打开客户端时照常拉。
    NotUntilUserReturns,
    /// 这一版程序起不来：程序没换就不自动拉。
    NotWithThisProgram,
}

/// 唯一的判据。输入是状态文件里的 `Phase`；读不到状态文件时调用方按 [`Relaunchable::Yes`]。
///
/// 停在 `Starting` / `Ready` 说明进程没来得及写别的就没了（崩溃、`kill -9`）。三层都是穷举匹配、没有 `_ =>`：
/// 加一个变体而没给答案，编译不过。
pub fn relaunchable(phase: &Phase) -> Relaunchable {
    match phase {
        Phase::Starting { .. } | Phase::Ready => Relaunchable::Yes,
        Phase::Stopping { reason } => after_stop(*reason),
        Phase::Exited {
            reason: Some(reason),
            ..
        } => after_stop(*reason),
        Phase::Exited {
            code, reason: None, ..
        } => after_exit_without_reason(*code),
        Phase::Refused(_) => Relaunchable::NotWithThisProgram,
    }
}

fn after_stop(reason: StopReason) -> Relaunchable {
    match reason {
        StopReason::UserQuit | StopReason::Signal => Relaunchable::NotUntilUserReturns,
        StopReason::Upgrade | StopReason::Fatal => Relaunchable::Yes,
    }
}

/// 启动阶段就退出（没有停机原因）时按逻辑码判。
fn after_exit_without_reason(code: ExitCode) -> Relaunchable {
    match code {
        // 干净地停下而没有原因，只可能是让路或被要求停
        ExitCode::Stopped => Relaunchable::NotUntilUserReturns,
        // 同一个程序、同样的参数或环境再起还是一样
        ExitCode::Usage
        | ExitCode::Unsupported
        | ExitCode::EndpointRefused
        | ExitCode::StoreRefused => Relaunchable::NotWithThisProgram,
        // 可能是暂时的，或本来就是要重启
        ExitCode::BindFailed | ExitCode::Fatal | ExitCode::RestartRequested => Relaunchable::Yes,
    }
}

/// 写状态文件：先写同目录的临时文件再改名，读的一方永远读到完整的旧内容或新内容。Unix 上权限 0600。
pub fn write_status(run_dir: &Path, status: &StartupStatus) -> io::Result<()> {
    let bytes = serde_json::to_vec_pretty(status).map_err(io::Error::other)?;
    let tmp = run_dir.join(format!("{STATUS_FILE}.tmp.{}", std::process::id()));
    let mut file = create_private(&tmp)?;
    file.write_all(&bytes)?;
    file.sync_all()?;
    drop(file);
    std::fs::rename(&tmp, run_dir.join(STATUS_FILE)).inspect_err(|_| {
        let _ = std::fs::remove_file(&tmp);
    })
}

/// 读状态文件。没有、读不出、解析不了（含新版本加的、本版不认识的变体）都返回 `None`。
pub fn read_status(run_dir: &Path) -> Option<StartupStatus> {
    let bytes = std::fs::read(run_dir.join(STATUS_FILE)).ok()?;
    serde_json::from_slice(&bytes).ok()
}

/// 把 [`LockInfo`] 写进已经锁住的锁文件：截断后从头写。不删不换文件——锁在这个 inode 上。
pub fn write_lock_info(lock_file: &mut File, info: &LockInfo) -> io::Result<()> {
    let bytes = serde_json::to_vec(info).map_err(io::Error::other)?;
    lock_file.set_len(0)?;
    lock_file.seek(SeekFrom::Start(0))?;
    lock_file.write_all(&bytes)?;
    lock_file.sync_data()
}

/// 读锁文件里的 [`LockInfo`]（不加锁）。没有、读不出、解析不了（含正写到一半）都返回 `None`。
pub fn read_lock_info(run_dir: &Path) -> Option<LockInfo> {
    let bytes = std::fs::read(run_dir.join(LOCK_FILE)).ok()?;
    serde_json::from_slice(&bytes).ok()
}

#[cfg(unix)]
fn create_private(path: &Path) -> io::Result<File> {
    use std::os::unix::fs::OpenOptionsExt;
    std::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .open(path)
}

#[cfg(not(unix))]
fn create_private(path: &Path) -> io::Result<File> {
    File::create(path)
}

#[cfg(test)]
mod tests;
