//! 在一个根目录（项目仓库根）里按相对路径读写文件，路径上任何一级都不跟随符号链接（ADR-0006）。
//!
//! 为什么要有它：仓库内容来自用户或 Agent，任何一级目录都可能被换成指向仓库外的符号链接。
//! `std::fs::read(root.join(rel))` 会顺着链接读到仓库外（例如凭据目录），再随差异或项目标记转发出去。
//!
//! **能挡住的**：
//! - 相对路径里的 `..`、绝对路径、盘符前缀（直接拒绝）；
//! - 路径中间任何一级是符号链接（[`RootedError::ThroughSymlink`]）；
//! - 最后一级是符号链接时，读写函数拒绝跟随（[`RootedError::FinalSymlink`]），需要链接本身的调用方用
//!   [`read_link`] 取链接目标文本；
//! - 写入：[`create_new_file`] 用独占创建，已存在的东西（包括链接）一律不碰；[`replace_file`] 先写同目录
//!   的临时文件再改名，改名替换的是目录项本身，不会顺着链接写到别处。
//!
//! **挡不住的（已知限制）**：只用 std 做不到 `openat` / `O_NOFOLLOW`，检查和打开是两步。检查之后、打开之前
//! 如果有**并发运行的进程**把某一级换成链接（TOCTOU），读操作仍可能读穿。静态的恶意仓库内容挡得住，
//! 与 iWork 同时运行、专门抢这个时间窗的进程挡不住。不要把它当成完整的沙箱。

use std::ffi::OsStr;
use std::fs::{self, Metadata, OpenOptions};
use std::io::{ErrorKind, Write};
use std::path::{Component, Path, PathBuf};

/// 根目录下一个路径是什么。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EntryKind {
    /// 不存在（包括中间某一级是普通文件，路径不可能存在）
    Missing,
    /// 普通文件；`executable` 取属主的执行位（非 unix 平台恒为 false）
    File {
        executable: bool,
    },
    Dir,
    /// 最后一级是符号链接（不跟随）
    Symlink,
    /// 设备、管道等
    Other,
}

#[derive(Debug)]
pub enum RootedError {
    /// 相对路径带 `..`、绝对路径或盘符
    InvalidRelative { path: PathBuf },
    /// 路径中间的 `link` 是符号链接
    ThroughSymlink { link: PathBuf },
    /// 最后一级 `path` 是符号链接
    FinalSymlink { path: PathBuf },
    /// 期望是普通文件，实际是别的（目录、设备等）
    NotAFile { path: PathBuf },
    Io {
        path: PathBuf,
        source: std::io::Error,
    },
}

// 本 crate 零依赖，不引 thiserror，Display 与 Error 手写
impl std::fmt::Display for RootedError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::InvalidRelative { path } => {
                write!(f, "{} 不是根目录内的相对路径", path.display())
            }
            Self::ThroughSymlink { link } => {
                write!(f, "路径经过符号链接 {}，拒绝跟随", link.display())
            }
            Self::FinalSymlink { path } => write!(f, "{} 是符号链接，拒绝跟随", path.display()),
            Self::NotAFile { path } => write!(f, "{} 不是普通文件", path.display()),
            Self::Io { path, source } => write!(f, "{}：{source}", path.display()),
        }
    }
}

impl std::error::Error for RootedError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Io { source, .. } => Some(source),
            _ => None,
        }
    }
}

/// 把相对路径拆成普通的各级名字；`.` 跳过，其余（`..`、根、盘符）拒绝。
fn normal_components(rel: &Path) -> Result<Vec<&OsStr>, RootedError> {
    rel.components()
        .filter(|c| !matches!(c, Component::CurDir))
        .map(|c| match c {
            Component::Normal(name) => Ok(name),
            _ => Err(RootedError::InvalidRelative {
                path: rel.to_owned(),
            }),
        })
        .collect()
}

fn io(path: &Path, source: std::io::Error) -> RootedError {
    RootedError::Io {
        path: path.to_owned(),
        source,
    }
}

fn kind_of(meta: &Metadata) -> EntryKind {
    let ft = meta.file_type();
    if ft.is_symlink() {
        EntryKind::Symlink
    } else if ft.is_dir() {
        EntryKind::Dir
    } else if ft.is_file() {
        EntryKind::File {
            executable: is_executable(meta),
        }
    } else {
        EntryKind::Other
    }
}

#[cfg(unix)]
fn is_executable(meta: &Metadata) -> bool {
    use std::os::unix::fs::PermissionsExt;
    meta.permissions().mode() & 0o100 != 0
}

#[cfg(not(unix))]
fn is_executable(_: &Metadata) -> bool {
    false
}

/// 逐级 `lstat`，看 `root` 下的 `rel` 是什么。中间某一级是符号链接时返回 [`RootedError::ThroughSymlink`]；
/// 最后一级是链接时返回 [`EntryKind::Symlink`]（不跟随）。`rel` 为空表示根本身。
pub fn inspect(root: &Path, rel: &Path) -> Result<EntryKind, RootedError> {
    let names = normal_components(rel)?;
    let mut cur = root.to_path_buf();
    let Some((last, parents)) = names.split_last() else {
        return fs::symlink_metadata(root)
            .map(|m| kind_of(&m))
            .map_err(|e| io(root, e));
    };
    for name in parents {
        cur.push(name);
        match fs::symlink_metadata(&cur) {
            Ok(m) if m.file_type().is_symlink() => {
                return Err(RootedError::ThroughSymlink { link: cur });
            }
            Ok(m) if m.is_dir() => {}
            Ok(_) => return Ok(EntryKind::Missing),
            Err(e) if e.kind() == ErrorKind::NotFound => return Ok(EntryKind::Missing),
            Err(e) => return Err(io(&cur, e)),
        }
    }
    cur.push(last);
    match fs::symlink_metadata(&cur) {
        Ok(m) => Ok(kind_of(&m)),
        Err(e) if e.kind() == ErrorKind::NotFound => Ok(EntryKind::Missing),
        Err(e) => Err(io(&cur, e)),
    }
}

/// 读普通文件。不存在返回 `Ok(None)`；任何一级是链接、或不是普通文件，都是错误。
pub fn read_file(root: &Path, rel: &Path) -> Result<Option<Vec<u8>>, RootedError> {
    let path = root.join(rel);
    match inspect(root, rel)? {
        EntryKind::Missing => Ok(None),
        EntryKind::File { .. } => fs::read(&path).map(Some).map_err(|e| io(&path, e)),
        EntryKind::Symlink => Err(RootedError::FinalSymlink { path }),
        EntryKind::Dir | EntryKind::Other => Err(RootedError::NotAFile { path }),
    }
}

/// 读最后一级符号链接本身的目标（不跟随）。最后一级不是链接时返回 [`RootedError::NotAFile`]。
pub fn read_link(root: &Path, rel: &Path) -> Result<PathBuf, RootedError> {
    let path = root.join(rel);
    match inspect(root, rel)? {
        EntryKind::Symlink => fs::read_link(&path).map_err(|e| io(&path, e)),
        _ => Err(RootedError::NotAFile { path }),
    }
}

/// 建一级目录：已是目录就用它；是链接或文件时报错。上级必须已存在且不经过链接。
pub fn ensure_dir(root: &Path, rel: &Path) -> Result<(), RootedError> {
    let path = root.join(rel);
    match inspect(root, rel)? {
        EntryKind::Dir => Ok(()),
        EntryKind::Missing => fs::create_dir(&path).map_err(|e| io(&path, e)),
        EntryKind::Symlink => Err(RootedError::FinalSymlink { path }),
        EntryKind::File { .. } | EntryKind::Other => Err(RootedError::NotAFile { path }),
    }
}

/// 独占新建文件并写入。已存在（包括是链接）时返回 `ErrorKind::AlreadyExists` 的 [`RootedError::Io`]，
/// 不覆盖、不跟随。
pub fn create_new_file(root: &Path, rel: &Path, contents: &[u8]) -> Result<(), RootedError> {
    let path = root.join(rel);
    match inspect(root, rel)? {
        EntryKind::Missing => {}
        EntryKind::Symlink => return Err(RootedError::FinalSymlink { path }),
        _ => {
            return Err(io(
                &path,
                std::io::Error::new(ErrorKind::AlreadyExists, "已存在"),
            ));
        }
    }
    // create_new = O_CREAT|O_EXCL：最后一级哪怕在检查后被换成链接也不会跟随
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&path)
        .map_err(|e| io(&path, e))?;
    file.write_all(contents).map_err(|e| io(&path, e))?;
    file.sync_all().map_err(|e| io(&path, e))
}

/// 用新内容替换普通文件（不存在就新建）。最后一级是链接时拒绝。
///
/// 先独占新建同目录下的临时文件，写完再改名覆盖目标：改名替换目录项本身，不会顺着链接写到别处。
pub fn replace_file(root: &Path, rel: &Path, contents: &[u8]) -> Result<(), RootedError> {
    let path = root.join(rel);
    match inspect(root, rel)? {
        EntryKind::Missing | EntryKind::File { .. } => {}
        EntryKind::Symlink => return Err(RootedError::FinalSymlink { path }),
        EntryKind::Dir | EntryKind::Other => return Err(RootedError::NotAFile { path }),
    }
    let file_name = path
        .file_name()
        .ok_or_else(|| RootedError::InvalidRelative {
            path: rel.to_owned(),
        })?
        .to_string_lossy()
        .into_owned();
    let tmp = path.with_file_name(format!(".{file_name}.iwork-tmp-{}", std::process::id()));
    let result = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&tmp)
        .and_then(|mut f| {
            f.write_all(contents)?;
            f.sync_all()
        })
        .and_then(|()| fs::rename(&tmp, &path));
    if let Err(e) = result {
        // 临时文件是这次新建的，失败时删掉；删不掉也只是留一个临时文件，以写入的错误为准
        let _ = fs::remove_file(&tmp);
        return Err(io(&path, e));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp() -> tempfile::TempDir {
        tempfile::Builder::new()
            .prefix("iwork-rootfs-")
            .tempdir()
            .unwrap()
    }

    #[test]
    fn rejects_paths_that_leave_the_root_lexically() {
        let d = tmp();
        for bad in ["../x", "a/../../x", "/etc/passwd"] {
            assert!(
                matches!(
                    inspect(d.path(), Path::new(bad)),
                    Err(RootedError::InvalidRelative { .. })
                ),
                "{bad}"
            );
        }
        assert_eq!(
            inspect(d.path(), Path::new("./nope")).unwrap(),
            EntryKind::Missing
        );
    }

    #[test]
    fn regular_files_and_dirs_work_normally() {
        let d = tmp();
        let root = d.path();
        fs::create_dir(root.join("a")).unwrap();
        create_new_file(root, Path::new("a/f"), b"one").unwrap();
        assert_eq!(
            read_file(root, Path::new("a/f")).unwrap(),
            Some(b"one".to_vec())
        );
        replace_file(root, Path::new("a/f"), b"two").unwrap();
        assert_eq!(
            read_file(root, Path::new("a/f")).unwrap(),
            Some(b"two".to_vec())
        );
        assert!(matches!(
            create_new_file(root, Path::new("a/f"), b"x"),
            Err(RootedError::Io { source, .. }) if source.kind() == ErrorKind::AlreadyExists
        ));
        assert_eq!(read_file(root, Path::new("a/missing")).unwrap(), None);
        ensure_dir(root, Path::new("a/sub")).unwrap();
        ensure_dir(root, Path::new("a/sub")).unwrap();
        assert_eq!(inspect(root, Path::new("a/sub")).unwrap(), EntryKind::Dir);
    }

    #[cfg(unix)]
    #[test]
    fn never_follows_symlinks_at_any_level() {
        use std::os::unix::fs::symlink;
        let inside = tmp();
        let outside = tmp();
        let root = inside.path();
        fs::write(outside.path().join("secret"), "OUTSIDE").unwrap();
        symlink(outside.path(), root.join("dir")).unwrap();
        symlink(outside.path().join("secret"), root.join("leaf")).unwrap();

        assert!(matches!(
            read_file(root, Path::new("dir/secret")),
            Err(RootedError::ThroughSymlink { .. })
        ));
        assert!(matches!(
            read_file(root, Path::new("leaf")),
            Err(RootedError::FinalSymlink { .. })
        ));
        assert_eq!(
            read_link(root, Path::new("leaf")).unwrap(),
            outside.path().join("secret")
        );
        assert!(create_new_file(root, Path::new("dir/new"), b"x").is_err());
        assert!(replace_file(root, Path::new("leaf"), b"x").is_err());
        assert!(replace_file(root, Path::new("dir/secret"), b"x").is_err());
        assert!(ensure_dir(root, Path::new("dir")).is_err());
        assert_eq!(
            fs::read_to_string(outside.path().join("secret")).unwrap(),
            "OUTSIDE"
        );
        assert!(!outside.path().join("new").exists());
    }

    #[cfg(unix)]
    #[test]
    fn reports_owner_execute_bit() {
        use std::os::unix::fs::PermissionsExt;
        let d = tmp();
        fs::write(d.path().join("x"), "").unwrap();
        fs::set_permissions(d.path().join("x"), fs::Permissions::from_mode(0o755)).unwrap();
        assert_eq!(
            inspect(d.path(), Path::new("x")).unwrap(),
            EntryKind::File { executable: true }
        );
    }
}
