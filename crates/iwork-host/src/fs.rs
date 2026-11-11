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
use iwork_ports::fs::{
    MAX_SYMLINK_HOPS, PathResolveError, PathResolver, ResolvedPath, split_last_component,
};

/// 真实文件系统上的路径解析。
#[derive(Debug, Clone)]
pub struct OsPathResolver {
    home: PathBuf,
    case_insensitive: bool,
}

impl OsPathResolver {
    /// `home` 必须已是规范路径（daemon 启动时 `canonicalize` 一次）。`case_insensitive` 由
    /// [`OsPathResolver::detect_case_insensitive`] 在数据目录上探测。Windows 上 `canonicalize` 带的
    /// `\\?\` 前缀在这里去掉：解析结果是指纹的键，同一处文件不能有带前缀、不带前缀两种写法。
    pub fn new(home: PathBuf, case_insensitive: bool) -> Self {
        Self {
            home: strip_verbatim(home),
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

    /// 大小写不敏感时把整条路径折成小写（2/F-7，1/F-10）：规范路径也是动作指纹的键，每个分量都要用同一种
    /// 等价表示，不能「已存在的取磁盘拼写、不存在的取原样或小写」——否则先建父目录就让同一路径换了键。
    fn fold(&self, p: PathBuf) -> PathBuf {
        if self.case_insensitive {
            PathBuf::from(p.to_string_lossy().to_lowercase())
        } else {
            p
        }
    }

    fn eq_name(&self, a: &std::ffi::OsStr, b: &std::ffi::OsStr) -> bool {
        if a == b {
            return true;
        }
        if !self.case_insensitive {
            return false;
        }
        match (a.to_str(), b.to_str()) {
            (Some(x), Some(y)) if x.is_ascii() && y.is_ascii() => x.eq_ignore_ascii_case(y),
            _ => a.to_string_lossy().to_lowercase() == b.to_string_lossy().to_lowercase(),
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

/// 尾段没有 `..` 时：最深的已存在上级（解析后）接上不存在的尾段。走不了快路径返回 `None`。
fn existing_ancestor(joined: &Path) -> Option<Result<PathBuf, PathResolveError>> {
    if joined.components().any(|c| c == Component::ParentDir) {
        return None;
    }
    let mut tail: Vec<&std::ffi::OsStr> = Vec::new();
    let mut cur = joined;
    while let Some(parent) = cur.parent() {
        tail.push(cur.file_name()?);
        if let Ok(real) = parent.canonicalize() {
            let first = real.join(tail.last()?);
            // 紧接着的分量必须确实不存在：存在（含悬空链接）就交给慢路径
            match std::fs::symlink_metadata(&first) {
                Err(e) if e.kind() == io::ErrorKind::NotFound => {}
                _ => return None,
            }
            let mut out = strip_verbatim(real);
            for name in tail.iter().rev() {
                out.push(name);
            }
            return Some(if out.to_str().is_some() {
                Ok(out)
            } else {
                Err(PathResolveError::NotUtf8 {
                    raw: joined.display().to_string(),
                })
            });
        }
        cur = parent;
    }
    None
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

/// 文件系统错误 → 解析错误。上级分量是普通文件单独一类（走不通），其余 I/O 错误（含权限）是查不清。
fn io_error(raw: &str, e: &io::Error) -> PathResolveError {
    if e.kind() == io::ErrorKind::NotADirectory {
        PathResolveError::NotADirectory {
            raw: raw.to_owned(),
        }
    } else {
        PathResolveError::Io {
            raw: raw.to_owned(),
            message: e.to_string(),
        }
    }
}

impl OsPathResolver {
    /// 末级跟随的完全解析（`ResolvedPath::target`）。
    fn resolve_target(&self, cwd: &CanonicalPath, raw: &str) -> Result<PathBuf, PathResolveError> {
        let err_raw = || raw.to_owned();
        let io_err = |e: io::Error| io_error(raw, &e);
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
            // cwd 可能是调用方自己 `canonicalize` 出来的（Windows 上带 `\\?\`），与 home 同样去掉
            (strip_verbatim(cwd.0.clone()), raw)
        };
        // 快路径：整条路径已存在时 realpath 就是逐分量、先解析链接的同一个顺序，一次系统调用链完成
        let joined = current.join(rest);
        if let Ok(real) = joined.canonicalize() {
            let real = strip_verbatim(real);
            return if real.to_str().is_some() {
                Ok(real)
            } else {
                Err(PathResolveError::NotUtf8 { raw: err_raw() })
            };
        }
        // 第二条快路径：目标还不存在（将要写的文件）。尾段没有 `..` 时，找最深的已存在上级一次解析，
        // 再确认紧接着的分量确实不存在（悬空的符号链接也算存在，交给逐分量的慢路径）
        if let Some(found) = existing_ancestor(&joined) {
            return found;
        }
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
                    let candidate = current.join(&name);
                    if missing {
                        current = current.join(&name);
                        continue;
                    }
                    match std::fs::symlink_metadata(&candidate) {
                        Err(e) if e.kind() == io::ErrorKind::NotFound => {
                            missing = true;
                            current = current.join(&name);
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
        Ok(current)
    }

    /// 末级不跟随的视图：末级是符号链接时是「父路径的完全解析 / 链接名」，否则等于 `target`。
    fn resolve_entry(
        &self,
        cwd: &CanonicalPath,
        raw: &str,
        target: &Path,
    ) -> Result<PathBuf, PathResolveError> {
        let Some((parent_raw, name)) = split_last_component(raw) else {
            return Ok(target.to_path_buf());
        };
        // 先一次 lstat（内核按同样的顺序解析中间分量）：末级不是链接时就等于 target，常见情况不再解析父路径
        let written = match raw.strip_prefix("~/") {
            Some(rest) => self.home.join(rest),
            None => cwd.0.join(raw),
        };
        if !std::fs::symlink_metadata(&written).is_ok_and(|m| m.file_type().is_symlink()) {
            return Ok(target.to_path_buf());
        }
        let parent = self.resolve_target(cwd, parent_raw)?;
        let at = parent.join(name);
        match std::fs::symlink_metadata(&at) {
            Ok(meta) if meta.file_type().is_symlink() => {
                Ok(parent.join(self.actual_name(&parent, name)))
            }
            _ => Ok(target.to_path_buf()),
        }
    }

    /// 已存在的目录项在磁盘上的写法：大小写不敏感时从目录里找，指纹与规则比较都用同一种写法。
    fn actual_name(&self, dir: &Path, name: &str) -> OsString {
        if self.case_insensitive
            && let Ok(entries) = std::fs::read_dir(dir)
        {
            for e in entries.flatten() {
                if self.eq_name(&e.file_name(), std::ffi::OsStr::new(name)) {
                    return e.file_name();
                }
            }
        }
        OsString::from(name)
    }
}

impl PathResolver for OsPathResolver {
    fn resolve(&self, cwd: &CanonicalPath, raw: &str) -> Result<ResolvedPath, PathResolveError> {
        let target = self.resolve_target(cwd, raw)?;
        let entry = self.resolve_entry(cwd, raw, &target)?;
        Ok(ResolvedPath {
            entry: CanonicalPath(self.fold(entry)),
            target: CanonicalPath(self.fold(target)),
        })
    }

    fn same_path<A: AsRef<Path> + ?Sized, B: AsRef<Path> + ?Sized>(&self, a: &A, b: &B) -> bool {
        let (mut a, mut b) = (a.as_ref().components(), b.as_ref().components());
        loop {
            match (a.next(), b.next()) {
                (None, None) => return true,
                (Some(x), Some(y)) if self.eq_name(x.as_os_str(), y.as_os_str()) => {}
                _ => return false,
            }
        }
    }

    fn is_under<A: AsRef<Path> + ?Sized, B: AsRef<Path> + ?Sized>(
        &self,
        child: &A,
        dir: &B,
    ) -> bool {
        let mut child = child.as_ref().components();
        dir.as_ref().components().all(|d| {
            child
                .next()
                .is_some_and(|c| self.eq_name(d.as_os_str(), c.as_os_str()))
        })
    }
}
