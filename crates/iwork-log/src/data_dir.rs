//! 数据目录在哪（技术设计 5.1）：根目录的定位与日志子目录。只算路径，不建整套目录（那是 `iwork-store` 的事）。
//!
//! 放在这里而不是存储层，是因为客户端也要找日志目录，而客户端不能依赖带 SQLite 的 `iwork-store`（ADR-0013）。

use std::io;
use std::path::{Path, PathBuf};

/// 覆盖数据目录的环境变量：开发、测试与诊断时换目录用。
pub const DATA_DIR_ENV: &str = "IWORK_DATA_DIR";

const LOGS_DIR: &str = "logs";

/// 找不到用户主目录（`HOME`，Windows 是 `LOCALAPPDATA`），定位不了数据目录。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct HomeNotFound;

/// 三平台定位数据目录的根：macOS `~/Library/Application Support/iWork/`；Linux `$XDG_DATA_HOME/iwork/`
/// （未设则 `~/.local/share/iwork/`）；Windows `%LOCALAPPDATA%\iWork\`。环境变量 [`DATA_DIR_ENV`] 存在且非空时优先。
pub fn locate_root() -> Result<PathBuf, HomeNotFound> {
    let var = |name: &str| std::env::var_os(name).filter(|v| !v.is_empty());
    if let Some(root) = var(DATA_DIR_ENV) {
        return Ok(PathBuf::from(root));
    }
    let root = if cfg!(target_os = "windows") {
        PathBuf::from(var("LOCALAPPDATA").ok_or(HomeNotFound)?).join("iWork")
    } else {
        let home = PathBuf::from(var("HOME").ok_or(HomeNotFound)?);
        if cfg!(target_os = "macos") {
            home.join("Library/Application Support/iWork")
        } else {
            var("XDG_DATA_HOME")
                .map(PathBuf::from)
                .unwrap_or_else(|| home.join(".local/share"))
                .join("iwork")
        }
    };
    Ok(root)
}

/// 数据目录下的日志目录。
pub fn logs_dir(root: &Path) -> PathBuf {
    root.join(LOGS_DIR)
}

/// 建目录（含上级）；只对这次新建的目录设 0700。已存在的不改权限（用户自己改过就尊重）。
pub fn create_private_dir(dir: &Path) -> io::Result<()> {
    if dir.is_dir() {
        return Ok(());
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt;
        std::fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(dir)
    }
    #[cfg(not(unix))]
    {
        std::fs::create_dir_all(dir)
    }
}
