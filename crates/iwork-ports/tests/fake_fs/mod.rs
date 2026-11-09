//! `PathResolver` 的内存假实现（T07 方案 2.3）。别的 crate 的测试经 `#[path]` 引入同一份。
//!
//! 解析顺序与真实实现（`iwork-host::fs::OsPathResolver`）相同：逐分量、先解析链接、`..` 作用在
//! 已解析的真实前缀上、第一个不存在的分量之后原样接上。`iwork-host` 的对拍测试在真实临时目录上
//! 搭同样的结构，断言两者结果相同，所以这里的行为不是凭空设想的。

#![allow(dead_code, reason = "各测试各用一部分")]

use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::ffi::OsString;
use std::path::{Component, Path, PathBuf};

use iwork_domain::action::CanonicalPath;
use iwork_ports::fs::{MAX_SYMLINK_HOPS, PathResolveError, PathResolver};

/// 内存文件系统：一组已存在的路径（父目录自动存在）与一组符号链接。
#[derive(Debug, Clone)]
pub struct FakeFs {
    pub home: PathBuf,
    pub case_insensitive: bool,
    existing: BTreeSet<PathBuf>,
    /// 链接所在位置 → 链接内容（可以是相对路径）
    symlinks: BTreeMap<PathBuf, PathBuf>,
}

enum Entry {
    Missing,
    Link(PathBuf),
    Exists(PathBuf),
}

impl FakeFs {
    pub fn new(home: impl Into<PathBuf>, case_insensitive: bool) -> Self {
        let home = home.into();
        let mut fs = Self {
            home: home.clone(),
            case_insensitive,
            existing: BTreeSet::new(),
            symlinks: BTreeMap::new(),
        };
        fs.add(&home);
        fs
    }

    /// 加一个已存在的路径（文件或目录），连同全部上级目录。
    pub fn add(&mut self, path: impl AsRef<Path>) -> &mut Self {
        let mut p = PathBuf::new();
        for c in path.as_ref().components() {
            p.push(c);
            self.existing.insert(p.clone());
        }
        self
    }

    /// 在 `at` 放一个指向 `target` 的符号链接（`at` 的上级目录自动存在）。
    pub fn link(&mut self, at: impl AsRef<Path>, target: impl Into<PathBuf>) -> &mut Self {
        let at = at.as_ref();
        if let Some(parent) = at.parent() {
            self.add(parent);
        }
        self.symlinks.insert(at.to_path_buf(), target.into());
        self
    }

    fn eq_name(&self, a: &Path, b: &Path) -> bool {
        if self.case_insensitive {
            a.to_string_lossy().to_lowercase() == b.to_string_lossy().to_lowercase()
        } else {
            a == b
        }
    }

    fn lstat(&self, p: &Path) -> Entry {
        if let Some((_, target)) = self.symlinks.iter().find(|(k, _)| self.eq_name(k, p)) {
            return Entry::Link(target.clone());
        }
        match self.existing.iter().find(|k| self.eq_name(k, p)) {
            Some(actual) => Entry::Exists(actual.clone()),
            None => Entry::Missing,
        }
    }

    fn components_eq(&self, a: &Path, b: &Path) -> bool {
        let (a, b): (Vec<_>, Vec<_>) = (a.components().collect(), b.components().collect());
        a.len() == b.len()
            && a.iter()
                .zip(&b)
                .all(|(x, y)| self.eq_name(x.as_ref(), y.as_ref()))
    }
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

impl PathResolver for FakeFs {
    fn resolve(&self, cwd: &CanonicalPath, raw: &str) -> Result<CanonicalPath, PathResolveError> {
        let err_raw = || raw.to_owned();
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
                    match self.lstat(&candidate) {
                        Entry::Missing => {
                            missing = true;
                            current = candidate;
                        }
                        Entry::Exists(actual) => current = actual,
                        Entry::Link(target) => {
                            hops += 1;
                            if hops > MAX_SYMLINK_HOPS {
                                return Err(PathResolveError::SymlinkLoop { raw: err_raw() });
                            }
                            // 链接内容排到剩余分量前面；相对链接相对于当前真实前缀，绝对链接由 Root 重置
                            for p in parts(&target).into_iter().rev() {
                                queue.push_front(p);
                            }
                        }
                    }
                }
            }
        }
        Ok(CanonicalPath(current))
    }

    fn same_path(&self, a: &CanonicalPath, b: &CanonicalPath) -> bool {
        self.components_eq(&a.0, &b.0)
    }

    fn is_under(&self, child: &CanonicalPath, dir: &CanonicalPath) -> bool {
        let dir: Vec<_> = dir.0.components().collect();
        let child: Vec<_> = child.0.components().collect();
        child.len() >= dir.len()
            && dir
                .iter()
                .zip(&child)
                .all(|(d, c)| self.eq_name(d.as_ref(), c.as_ref()))
    }
}
