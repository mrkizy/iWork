//! 路径事实端口（T07 方案 2.3）：把工具或命令里写的路径变成规范路径。
//!
//! 审批求值是纯函数，但符号链接、大小写是磁盘上的事实，只能经这个端口取来当输入。
//! 同步：它在 hook 的热路径上，一次调用只做几次 lstat。

use iwork_domain::action::CanonicalPath;

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
pub trait PathResolver: Send + Sync {
    fn resolve(&self, cwd: &CanonicalPath, raw: &str) -> Result<CanonicalPath, PathResolveError>;
    /// 两条规范路径在这个文件系统上是否指同一处（大小写不敏感时忽略大小写）。
    fn same_path(&self, a: &CanonicalPath, b: &CanonicalPath) -> bool;
    /// `child` 是否在 `dir` 之下（含相等）；按 `same_path` 的规则逐分量比。
    fn is_under(&self, child: &CanonicalPath, dir: &CanonicalPath) -> bool;
}

/// 解析失败。调用方一律按「解析不了」处理（询问，不猜）。
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum PathResolveError {
    #[error("路径为空")]
    Empty,
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

/// 符号链接最多跟几层（与常见系统的 `MAXSYMLINKS` 同量级）。
pub const MAX_SYMLINK_HOPS: usize = 40;
