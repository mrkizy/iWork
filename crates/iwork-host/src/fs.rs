//! `PathResolver` 的真实实现（T07 方案 2.3）。
//!
//! 逐分量走真实文件系统：每个分量先 `lstat`，是符号链接就把链接内容排到剩余分量前面；
//! 已存在的普通分量取 `canonicalize` 的结果（它的上级已是真实路径、它自己不是链接，所以只是
//! 取磁盘上的实际大小写）；第一个不存在的分量之后原样接上。顺序契约见 trait 文档。

use std::collections::VecDeque;
use std::ffi::OsString;
use std::io;
use std::path::{Component, Path, PathBuf};

use iwork_domain::action::CanonicalPath;
use iwork_ports::fs::{MAX_SYMLINK_HOPS, PathResolveError, PathResolver};

/// 真实文件系统上的路径解析。
#[derive(Debug, Clone)]
pub struct OsPathResolver {
    home: PathBuf,
    case_insensitive: bool,
}

impl OsPathResolver {
    /// `home` 必须已是规范路径（daemon 启动时 `canonicalize` 一次）。`case_insensitive` 由
    /// [`OsPathResolver::detect_case_insensitive`] 在数据目录上探测。
    pub fn new(home: PathBuf, case_insensitive: bool) -> Self {
        Self {
            home,
            case_insensitive,
        }
    }

    /// 在已存在的目录 `dir` 上探测文件系统是否大小写不敏感：把路径换成全大写后还能找到同一个
    /// 目录就是不敏感。路径里没有字母时探测不出来，按敏感处理（比较更严，不会把两处当成一处）。
    pub fn detect_case_insensitive(dir: &Path) -> io::Result<bool> {
        let dir = dir.canonicalize()?;
        let upper = PathBuf::from(dir.to_string_lossy().to_uppercase());
        if upper == dir {
            return Ok(false);
        }
        match (std::fs::metadata(&dir), std::fs::metadata(&upper)) {
            (Ok(a), Ok(b)) => Ok(same_file(&a, &b)),
            _ => Ok(false),
        }
    }

    fn eq_name(&self, a: &std::ffi::OsStr, b: &std::ffi::OsStr) -> bool {
        if self.case_insensitive {
            a.to_string_lossy().to_lowercase() == b.to_string_lossy().to_lowercase()
        } else {
            a == b
        }
    }
}

#[cfg(unix)]
fn same_file(a: &std::fs::Metadata, b: &std::fs::Metadata) -> bool {
    use std::os::unix::fs::MetadataExt;
    a.dev() == b.dev() && a.ino() == b.ino()
}

#[cfg(not(unix))]
fn same_file(_a: &std::fs::Metadata, _b: &std::fs::Metadata) -> bool {
    // Windows 的 NTFS 默认大小写不敏感；拿不到文件身份时，能用大写路径取到元数据就算不敏感
    true
}

/// 路径的一个分量（自有数据，可以放进队列）。
enum Part {
    /// 根（含 Windows 的盘符前缀）
    Root(PathBuf),
    Parent,
    Name(OsString),
}

fn parts(p: &Path) -> Vec<Part> {
    let mut root = PathBuf::new();
    let mut out = Vec::new();
    for c in p.components() {
        match c {
            Component::Prefix(_) | Component::RootDir => root.push(c.as_os_str()),
            Component::CurDir => {}
            Component::ParentDir => out.push(Part::Parent),
            Component::Normal(n) => out.push(Part::Name(n.to_owned())),
        }
    }
    if !root.as_os_str().is_empty() {
        out.insert(0, Part::Root(root));
    }
    out
}

/// Windows 的 `canonicalize` 返回 `\\?\C:\…`；去掉前缀，与规则里的路径写法一致。
fn strip_verbatim(p: PathBuf) -> PathBuf {
    #[cfg(windows)]
    {
        let s = p.to_string_lossy();
        if let Some(rest) = s.strip_prefix(r"\\?\")
            && !rest.starts_with("UNC\\")
        {
            return PathBuf::from(rest);
        }
    }
    p
}

impl PathResolver for OsPathResolver {
    fn resolve(&self, cwd: &CanonicalPath, raw: &str) -> Result<CanonicalPath, PathResolveError> {
        let err_raw = || raw.to_owned();
        let io_err = |e: io::Error| PathResolveError::Io {
            raw: raw.to_owned(),
            message: e.to_string(),
        };
        if raw.is_empty() {
            return Err(PathResolveError::Empty);
        }
        let (mut current, rest) = if raw == "~" {
            (self.home.clone(), "")
        } else if let Some(rest) = raw.strip_prefix("~/") {
            (self.home.clone(), rest)
        } else if raw.starts_with('~') {
            return Err(PathResolveError::OtherUserHome { raw: err_raw() });
        } else {
            (cwd.0.clone(), raw)
        };
        let mut queue: VecDeque<Part> = parts(Path::new(rest)).into();
        let mut missing = false;
        let mut hops = 0;
        while let Some(part) = queue.pop_front() {
            match part {
                Part::Root(root) => current = root,
                Part::Parent => {
                    if missing {
                        return Err(PathResolveError::DotDotAfterMissing { raw: err_raw() });
                    }
                    current.pop();
                }
                Part::Name(name) => {
                    let candidate = current.join(name);
                    if missing {
                        current = candidate;
                        continue;
                    }
                    match std::fs::symlink_metadata(&candidate) {
                        Err(e) if e.kind() == io::ErrorKind::NotFound => {
                            missing = true;
                            current = candidate;
                        }
                        Err(e) => return Err(io_err(e)),
                        Ok(meta) if meta.file_type().is_symlink() => {
                            hops += 1;
                            if hops > MAX_SYMLINK_HOPS {
                                return Err(PathResolveError::SymlinkLoop { raw: err_raw() });
                            }
                            let target = std::fs::read_link(&candidate).map_err(io_err)?;
                            // 链接内容排到剩余分量前面；相对链接相对于当前真实前缀，绝对链接由 Root 重置
                            for p in parts(&target).into_iter().rev() {
                                queue.push_front(p);
                            }
                        }
                        Ok(_) => {
                            current = strip_verbatim(candidate.canonicalize().map_err(io_err)?);
                        }
                    }
                }
            }
        }
        if current.to_str().is_none() {
            return Err(PathResolveError::NotUtf8 { raw: err_raw() });
        }
        Ok(CanonicalPath(current))
    }

    fn same_path(&self, a: &CanonicalPath, b: &CanonicalPath) -> bool {
        let (a, b): (Vec<_>, Vec<_>) = (a.0.components().collect(), b.0.components().collect());
        a.len() == b.len()
            && a.iter()
                .zip(&b)
                .all(|(x, y)| self.eq_name(x.as_os_str(), y.as_os_str()))
    }

    fn is_under(&self, child: &CanonicalPath, dir: &CanonicalPath) -> bool {
        let dir: Vec<_> = dir.0.components().collect();
        let child: Vec<_> = child.0.components().collect();
        child.len() >= dir.len()
            && dir
                .iter()
                .zip(&child)
                .all(|(d, c)| self.eq_name(d.as_os_str(), c.as_os_str()))
    }
}
