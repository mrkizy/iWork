//! `PathResolver` 的内存假实现（T07 方案 2.3）。别的 crate 的测试经 `#[path]` 引入同一份。
//!
//! 解析顺序与真实实现（`iwork-host::fs::OsPathResolver`）相同：逐分量、先解析链接、`..` 作用在
//! 已解析的真实前缀上、第一个不存在的分量之后原样接上。`iwork-host` 的对拍测试在真实临时目录上
//! 搭同样的结构，断言两者结果相同，所以这里的行为不是凭空设想的。

#![allow(dead_code, reason = "各测试各用一部分")]

use std::collections::{BTreeMap, VecDeque};
use std::ffi::OsString;
use std::path::{Component, Path, PathBuf};

use iwork_domain::action::CanonicalPath;
use iwork_ports::fs::{
    MAX_SYMLINK_HOPS, PathResolveError, PathResolver, ResolvedPath, split_last_component,
};

/// 节点种类。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FakeNode {
    File,
    Dir,
    Symlink(PathBuf),
    /// 没有执行（搜索）权限的目录：它自己能 lstat，要穿过它查里面的东西时报权限错误
    Denied,
}

/// 内存文件系统：路径 → 节点。`add` 的上级目录自动存在。
#[derive(Debug, Clone)]
pub struct FakeFs {
    pub home: PathBuf,
    pub case_insensitive: bool,
    nodes: BTreeMap<PathBuf, FakeNode>,
}

enum Entry {
    Missing,
    Link(PathBuf),
    Exists(PathBuf, FakeNode),
}

impl FakeFs {
    pub fn new(home: impl Into<PathBuf>, case_insensitive: bool) -> Self {
        let home = home.into();
        let mut fs = Self {
            home: home.clone(),
            case_insensitive,
            nodes: BTreeMap::new(),
        };
        fs.add(&home);
        fs
    }

    /// 加一个已存在的目录，连同全部上级目录。
    pub fn add(&mut self, path: impl AsRef<Path>) -> &mut Self {
        self.ancestors(path.as_ref());
        self.nodes
            .entry(path.as_ref().to_path_buf())
            .or_insert(FakeNode::Dir);
        self
    }

    /// 加一个普通文件（上级目录自动存在）：前缀分量是它时解析报 `NotADirectory`。
    pub fn file(&mut self, path: impl AsRef<Path>) -> &mut Self {
        self.ancestors(path.as_ref());
        self.nodes
            .insert(path.as_ref().to_path_buf(), FakeNode::File);
        self
    }

    /// 把一个目录标成没有搜索权限。
    pub fn deny(&mut self, path: impl AsRef<Path>) -> &mut Self {
        self.ancestors(path.as_ref());
        self.nodes
            .insert(path.as_ref().to_path_buf(), FakeNode::Denied);
        self
    }

    /// 在 `at` 放一个指向 `target` 的符号链接（`at` 的上级目录自动存在；已有的节点被替换）。
    pub fn link(&mut self, at: impl AsRef<Path>, target: impl Into<PathBuf>) -> &mut Self {
        let at = at.as_ref();
        self.ancestors(at);
        self.nodes
            .insert(at.to_path_buf(), FakeNode::Symlink(target.into()));
        self
    }

    /// 删掉一个节点（换链接、改回普通文件的测试用）。
    pub fn remove(&mut self, path: impl AsRef<Path>) -> &mut Self {
        self.nodes.remove(path.as_ref());
        self
    }

    fn ancestors(&mut self, path: &Path) {
        let mut p = PathBuf::new();
        let comps: Vec<_> = path.components().collect();
        for c in comps.iter().take(comps.len().saturating_sub(1)) {
            p.push(c);
            self.nodes.entry(p.clone()).or_insert(FakeNode::Dir);
        }
    }

    fn eq_name(&self, a: &Path, b: &Path) -> bool {
        if self.case_insensitive {
            a.to_string_lossy().to_lowercase() == b.to_string_lossy().to_lowercase()
        } else {
            a == b
        }
    }

    /// 还不存在的分量：大小写不敏感时统一成小写，同一个将创建的文件只有一种写法（审查 1/F-10）。
    fn missing_name(&self, name: &std::ffi::OsStr) -> OsString {
        if self.case_insensitive {
            OsString::from(name.to_string_lossy().to_lowercase())
        } else {
            name.to_owned()
        }
    }

    fn lstat(&self, p: &Path) -> Entry {
        let found = if self.case_insensitive {
            self.nodes.iter().find(|(k, _)| self.eq_name(k, p))
        } else {
            self.nodes.get_key_value(p)
        };
        match found {
            Some((_, FakeNode::Symlink(target))) => Entry::Link(target.clone()),
            Some((actual, node)) => Entry::Exists(actual.clone(), node.clone()),
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

    fn resolve_target(&self, cwd: &CanonicalPath, raw: &str) -> Result<PathBuf, PathResolveError> {
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
                    let candidate = current.join(&name);
                    if missing {
                        current = current.join(self.missing_name(&name));
                        continue;
                    }
                    // 查 current 里的东西：current 自己必须是能进去的目录
                    match self.lstat(&current) {
                        Entry::Exists(_, FakeNode::File) => {
                            return Err(PathResolveError::NotADirectory { raw: err_raw() });
                        }
                        Entry::Exists(_, FakeNode::Denied) => {
                            return Err(PathResolveError::Io {
                                raw: err_raw(),
                                message: "Permission denied (os error 13)".into(),
                            });
                        }
                        _ => {}
                    }
                    match self.lstat(&candidate) {
                        Entry::Missing => {
                            missing = true;
                            current = current.join(self.missing_name(&name));
                        }
                        Entry::Exists(actual, _) => current = actual,
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
        Ok(current)
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
    fn resolve(&self, cwd: &CanonicalPath, raw: &str) -> Result<ResolvedPath, PathResolveError> {
        let target = self.resolve_target(cwd, raw)?;
        let entry = match split_last_component(raw) {
            None => target.clone(),
            Some((parent_raw, name)) => {
                let parent = self.resolve_target(cwd, parent_raw)?;
                match self.lstat(&parent.join(name)) {
                    Entry::Link(_) => {
                        let actual = self
                            .nodes
                            .keys()
                            .find(|k| self.eq_name(k, &parent.join(name)))
                            .and_then(|k| k.file_name().map(OsString::from))
                            .unwrap_or_else(|| OsString::from(name));
                        parent.join(actual)
                    }
                    _ => target.clone(),
                }
            }
        };
        Ok(ResolvedPath {
            entry: CanonicalPath(entry),
            target: CanonicalPath(target),
        })
    }

    fn same_path<A: AsRef<Path> + ?Sized, B: AsRef<Path> + ?Sized>(&self, a: &A, b: &B) -> bool {
        self.components_eq(a.as_ref(), b.as_ref())
    }

    fn is_under<A: AsRef<Path> + ?Sized, B: AsRef<Path> + ?Sized>(
        &self,
        child: &A,
        dir: &B,
    ) -> bool {
        let dir: Vec<_> = dir.as_ref().components().collect();
        let child: Vec<_> = child.as_ref().components().collect();
        child.len() >= dir.len()
            && dir
                .iter()
                .zip(&child)
                .all(|(d, c)| self.eq_name(d.as_ref(), c.as_ref()))
    }
}
