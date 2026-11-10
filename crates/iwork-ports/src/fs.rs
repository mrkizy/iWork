//! 路径事实端口（T07 方案 2.3）：把工具或命令里写的路径变成规范路径。
//!
//! 审批求值是纯函数，但符号链接、大小写是磁盘上的事实，只能经这个端口取来当输入。
//! 同步：它在 hook 的热路径上，一次调用只做几次 lstat。

use std::path::Path;

use iwork_domain::action::CanonicalPath;

/// 一个路径词解析出来的两种视图（T07 方案 2.3）。「末级是链接时算哪里」只在这里定一次：
/// 归一（方案 3.5）和规则路径的别名（方案 4.3）都读这一份，不按程序登记跟不跟随。
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct ResolvedPath {
    /// 完全解析：每个分量（含末级）都跟随符号链接。读到的、写穿到的是这里。
    pub target: CanonicalPath,
    /// 末级不跟随：父路径逐分量解析，末级分量原样接上。替换、删除目录项时动的是这里。
    /// 末级不是符号链接时等于 `target`；`raw` 带尾斜杠、或末级是 `.` / `..`、或是单独的 `~` 时
    /// 也等于 `target`：这几种写法内核都先跟随末级（`rm -rf 链接/` 删的是链接指向的目录，9/F-1）。
    pub entry: CanonicalPath,
}

impl ResolvedPath {
    /// 两种视图相同（末级不是链接）。
    pub fn same(path: CanonicalPath) -> Self {
        Self {
            entry: path.clone(),
            target: path,
        }
    }
}

/// `raw` 的末级分量会不会被单独看待：返回 `(父路径原文, 末级名字)`。
/// 返回 `None` 时 `entry` 等于 `target`（尾斜杠、末级是 `.` / `..`、`~`、根）。
/// 真实实现与测试假实现都用它，「哪些写法有两种视图」只有这一处。
pub fn split_last_component(raw: &str) -> Option<(&str, &str)> {
    if raw.is_empty()
        || raw == "~"
        || raw.ends_with('/')
        || raw.ends_with(std::path::MAIN_SEPARATOR)
    {
        return None;
    }
    let (parent, name) = match raw.rfind(['/', std::path::MAIN_SEPARATOR]) {
        Some(0) => ("/", &raw[1..]),
        Some(i) => (&raw[..i], &raw[i + 1..]),
        None => (".", raw),
    };
    if matches!(name, "." | "..") {
        return None;
    }
    Some((parent, name))
}

/// 路径解析与比较。
/// 路径解析与比较。
///
/// **顺序契约**（第 1 轮审查 F-9）：`raw` 相对 `cwd` 解析，按文件系统自己的顺序**逐个分量**处理：
/// 开头的 `~/`（或单独的 `~`）展开为主目录；每遇到一个分量，先把它接到已解析的真实前缀上，
/// 若它是符号链接就先解析链接（相对链接相对于当前真实前缀），再处理下一个分量；
/// `..` 作用在**已解析的真实前缀**上，不作用在文字上（`link/../config` 里 `link → .git/objects`
/// 得到 `.git/config`，按文字消 `..` 会错得到 `config`）。
/// 遇到第一个不存在的分量后，剩余分量原样接上（目标可能还不存在，如将要写的文件）；
/// 不存在的尾段里再出现 `..` 无从判断，返回错误。
/// 大小写不敏感的文件系统上，已存在的分量取磁盘上的实际大小写。
/// 路径只是不存在不算错误。失败分两类，见 [`PathResolveError::certainty`]。
pub trait PathResolver: Send + Sync {
    /// 一次解析同时给出两种视图（[`ResolvedPath`]）。
    fn resolve(&self, cwd: &CanonicalPath, raw: &str) -> Result<ResolvedPath, PathResolveError>;
    /// 两条规范路径在这个文件系统上是否指同一处（大小写不敏感时忽略大小写）。
    /// 收 `AsRef<Path>`：规则条目里的路径与动作目标都能直接传，求值热路径上不必为比较分配。
    fn same_path<A: AsRef<Path> + ?Sized, B: AsRef<Path> + ?Sized>(&self, a: &A, b: &B) -> bool;
    /// `child` 是否在 `dir` 之下（含相等）；按 `same_path` 的规则逐分量比。
    fn is_under<A: AsRef<Path> + ?Sized, B: AsRef<Path> + ?Sized>(
        &self,
        child: &A,
        dir: &B,
    ) -> bool;
}

/// 解析失败。调用方按 [`PathResolveError::certainty`] 分两类处理（T07 方案 3.5 那张表）。
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum PathResolveError {
    #[error("路径为空")]
    Empty,
    #[error("路径 {raw} 的某个上级分量是普通文件，不是目录")]
    NotADirectory { raw: String },
    #[error("路径 {raw} 以 ~用户名 开头，取决于别的用户的主目录")]
    OtherUserHome { raw: String },
    #[error("路径 {raw} 在不存在的部分里出现 ..，无从判断指向哪里")]
    DotDotAfterMissing { raw: String },
    #[error("路径 {raw} 的符号链接层数过多（可能成环）")]
    SymlinkLoop { raw: String },
    #[error("路径 {raw} 含非 UTF-8 的符号链接目标")]
    NotUtf8 { raw: String },
    #[error("路径 {raw} 无法读取：{message}")]
    Io { raw: String, message: String },
}

/// 解析失败是哪一类（T07 方案 2.3、3.5）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ResolveCertainty {
    /// 照现在的磁盘这条路径走不通：命令沿这条路径走一定失败
    Disproved,
    /// 查不清：同一条调用里前面的命令可能先把它变得走得通（如先 `chmod` 恢复权限），不能当不存在
    Unknown,
}

impl PathResolveError {
    /// 走不通还是查不清：调用方处理失败的唯一依据。
    pub fn certainty(&self) -> ResolveCertainty {
        match self {
            Self::Empty | Self::NotADirectory { .. } | Self::DotDotAfterMissing { .. } => {
                ResolveCertainty::Disproved
            }
            Self::OtherUserHome { .. }
            | Self::SymlinkLoop { .. }
            | Self::NotUtf8 { .. }
            | Self::Io { .. } => ResolveCertainty::Unknown,
        }
    }
}

/// 符号链接最多跟几层（与常见系统的 `MAXSYMLINKS` 同量级）。
pub const MAX_SYMLINK_HOPS: usize = 40;
