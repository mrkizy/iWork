//! 数据目录（技术设计 5.1，方案 2.1）：定位与目录结构。
//!
//! 只有这里知道各子目录叫什么；别的模块（迁移的备份、制品的三个区、T04 的 `run/`、T06 写进沙箱
//! 只读区的 `attachments/`、`imports/`）都问它要路径。

use std::path::{Path, PathBuf};

use iwork_domain::common::ArtifactArea;

/// 覆盖数据目录的环境变量：开发、测试与诊断时换目录用。
pub const DATA_DIR_ENV: &str = "IWORK_DATA_DIR";

const DB_FILE: &str = "iwork.db";

/// 数据目录定位或建目录失败。
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum DataDirError {
    #[error("找不到用户主目录，无法定位数据目录（可设环境变量 {DATA_DIR_ENV} 指定）")]
    HomeNotFound,
    #[error("数据目录 {path} 读写失败：{message}")]
    Io { path: PathBuf, message: String },
}

impl DataDirError {
    pub(crate) fn io(path: &Path, err: std::io::Error) -> Self {
        Self::Io {
            path: path.to_path_buf(),
            message: err.to_string(),
        }
    }
}

/// iWork 的数据目录。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DataDir {
    root: PathBuf,
}

impl DataDir {
    /// 三平台定位：macOS `~/Library/Application Support/iWork/`；Linux `$XDG_DATA_HOME/iwork/`
    /// （未设则 `~/.local/share/iwork/`）；Windows `%LOCALAPPDATA%\iWork\`。
    /// 环境变量 [`DATA_DIR_ENV`] 存在且非空时优先。
    pub fn locate() -> Result<Self, DataDirError> {
        let var = |name: &str| std::env::var_os(name).filter(|v| !v.is_empty());
        if let Some(root) = var(DATA_DIR_ENV) {
            return Ok(Self::from_root(PathBuf::from(root)));
        }
        let root = if cfg!(target_os = "windows") {
            PathBuf::from(var("LOCALAPPDATA").ok_or(DataDirError::HomeNotFound)?).join("iWork")
        } else {
            let home = PathBuf::from(var("HOME").ok_or(DataDirError::HomeNotFound)?);
            if cfg!(target_os = "macos") {
                home.join("Library/Application Support/iWork")
            } else {
                var("XDG_DATA_HOME")
                    .map(PathBuf::from)
                    .unwrap_or_else(|| home.join(".local/share"))
                    .join("iwork")
            }
        };
        Ok(Self::from_root(root))
    }

    pub fn from_root(root: PathBuf) -> Self {
        Self { root }
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    /// 建全部子目录；Unix 下新建的目录权限 0700。已存在的不改权限（用户自己改过就尊重）。
    pub fn ensure_layout(&self) -> Result<(), DataDirError> {
        let mut dirs = vec![
            self.root.clone(),
            self.backups(),
            self.imports(),
            self.run(),
            self.logs(),
        ];
        for area in ALL_AREAS {
            dirs.push(self.area(area));
            dirs.push(self.area_tmp(area));
            dirs.push(self.area_objects(area));
        }
        for dir in dirs {
            create_private_dir(&dir)?;
        }
        Ok(())
    }

    /// 主库文件 `iwork.db`；WAL 与共享内存文件在旁边，后缀 `-wal`、`-shm`。
    pub fn db_path(&self) -> PathBuf {
        self.root.join(DB_FILE)
    }

    /// 迁移与协议门的备份（技术设计 5.5）。协议门探测用的临时副本也放在这里。
    pub fn backups(&self) -> PathBuf {
        self.root.join("backups")
    }

    /// 制品区目录。三个区互不引用、不跨区去重（需求 v1.68）。
    pub fn area(&self, area: ArtifactArea) -> PathBuf {
        self.root.join(area_dir_name(area))
    }

    /// 区内放写到一半的临时文件的目录，与最终目录同一文件系统，改名是原子的。
    pub fn area_tmp(&self, area: ArtifactArea) -> PathBuf {
        self.area(area).join("tmp")
    }

    /// 区内按哈希存放对象的目录：`<区>/sha256/<前 2 位>/<其余>`。
    pub fn area_objects(&self, area: ArtifactArea) -> PathBuf {
        self.area(area).join("sha256")
    }

    pub fn imports(&self) -> PathBuf {
        self.root.join("imports")
    }

    pub fn run(&self) -> PathBuf {
        self.root.join("run")
    }

    pub fn logs(&self) -> PathBuf {
        self.root.join("logs")
    }
}

/// 三个制品区，按目录建立的顺序。
pub(crate) const ALL_AREAS: [ArtifactArea; 3] = [
    ArtifactArea::Artifacts,
    ArtifactArea::Attachments,
    ArtifactArea::Private,
];

fn area_dir_name(area: ArtifactArea) -> &'static str {
    match area {
        ArtifactArea::Artifacts => "artifacts",
        ArtifactArea::Attachments => "attachments",
        ArtifactArea::Private => "private",
    }
}

/// 建目录（含上级）；只对这次新建的目录设 0700。
fn create_private_dir(dir: &Path) -> Result<(), DataDirError> {
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
            .map_err(|e| DataDirError::io(dir, e))
    }
    #[cfg(not(unix))]
    {
        std::fs::create_dir_all(dir).map_err(|e| DataDirError::io(dir, e))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn layout_creates_every_subdir_and_three_separate_areas() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = DataDir::from_root(tmp.path().join("data"));
        dir.ensure_layout().unwrap();
        for sub in ["backups", "imports", "run", "logs"] {
            assert!(dir.root().join(sub).is_dir(), "缺 {sub}/");
        }
        let areas: Vec<_> = ALL_AREAS.iter().map(|a| dir.area(*a)).collect();
        for area in &areas {
            assert!(area.join("tmp").is_dir() && area.join("sha256").is_dir());
        }
        // 三个区三个不同的目录，没有谁是谁的子目录
        for (i, a) in areas.iter().enumerate() {
            for (j, b) in areas.iter().enumerate() {
                if i != j {
                    assert!(!a.starts_with(b), "{a:?} 在 {b:?} 里");
                }
            }
        }
        // 幂等：再来一次不报错
        dir.ensure_layout().unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn new_dirs_are_owner_only_and_existing_dirs_keep_their_mode() {
        use std::os::unix::fs::PermissionsExt;
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("data");
        std::fs::create_dir(&root).unwrap();
        std::fs::set_permissions(&root, std::fs::Permissions::from_mode(0o750)).unwrap();
        let dir = DataDir::from_root(root.clone());
        dir.ensure_layout().unwrap();
        let mode = |p: &Path| std::fs::metadata(p).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode(&root), 0o750, "已存在的目录不改权限");
        assert_eq!(mode(&dir.area(ArtifactArea::Private)), 0o700);
        assert_eq!(mode(&dir.backups()), 0o700);
    }
}
