//! 读文件的接缝（T10f 方案 A3.1）。发现只经它碰文件系统；测试换成记下每一次调用的假实现，
//! 由调用记录证明哪些路径被读过、哪些目录只被列过。

use std::ffi::OsString;
use std::io;
use std::path::Path;

/// 一个路径指向什么。跟随链接（用户的技能目录常是链接）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FileKind {
    Dir,
    File,
    Other,
}

pub trait LocalFiles: Send + Sync {
    /// 读整个文件。
    fn read(&self, path: &Path) -> io::Result<Vec<u8>>;
    /// 列目录：只要条目名，次序不保证。
    fn list(&self, dir: &Path) -> io::Result<Vec<OsString>>;
    /// 这个路径指向什么（跟随链接）。
    fn kind(&self, path: &Path) -> io::Result<FileKind>;
}

/// 生产实现：`std::fs`。
#[derive(Debug, Clone, Copy, Default)]
pub struct StdFiles;

impl LocalFiles for StdFiles {
    fn read(&self, path: &Path) -> io::Result<Vec<u8>> {
        std::fs::read(path)
    }

    fn list(&self, dir: &Path) -> io::Result<Vec<OsString>> {
        std::fs::read_dir(dir)?
            .map(|e| e.map(|e| e.file_name()))
            .collect()
    }

    fn kind(&self, path: &Path) -> io::Result<FileKind> {
        let m = std::fs::metadata(path)?;
        Ok(if m.is_dir() {
            FileKind::Dir
        } else if m.is_file() {
            FileKind::File
        } else {
            FileKind::Other
        })
    }
}
