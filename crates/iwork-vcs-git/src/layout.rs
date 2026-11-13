//! 交给 gix 之前先查仓库的布局，打开时只让 gix 读查过的东西（T10a 方案 2.4a）。
//!
//! gix 打开仓库、读状态时，会顺着仓库里的东西去读别处。仓库内容来自用户或 Agent，任何一处都能被指到凭据目录或
//! 数据目录，后台在沙箱外读到就违反了底线。gix 找到要读的文件只有三种途径，各用一条规则管住：
//!
//! 1. **打开的目标**：`.git` 是目录或 gitfile，gitfile 与 `commondir` 再指到别的目录。预检用 gix 自己的解析函数
//!    算出 git 目录与共享目录（[`check_before_open`]），交给 gix 的路径是 `<根>/.git` 本身（不给 gix 退回去开根目录
//!    的机会），开完再核 gix 用的就是预检的那两个目录（[`check_after_open`]）。
//! 2. **磁盘布局**：git 目录、共享目录整棵树走一遍，不跟随，有链接就拒绝；只排除当前读操作根本不读的几处
//!    （[`Skip`]）。gix 按引用名、文件名在这两棵树里找到的东西（`refs/`、`worktrees/<名>/`、`packed-refs`、包、
//!    索引……）都在里面，不用逐个列。
//! 3. **配置里写的路径**：gix 把「会让它去读别处或运行程序」的配置值（`core.excludesFile`、`core.attributesFile`、
//!    `gitoxide.core.indexFile`、过滤与差异驱动、`core.worktree`……）都经同一道信任闸读；打开时让这道闸对仓库
//!    来源的配置一律回 false（[`open_options`]）。闸外的设置（`core.fileMode`、`core.autocrlf` 等）照常生效。
//!
//! 文件**内容**里写着路径的，逐个对过 gix 0.90 源码：gitfile、`commondir` 归第 1 条；符号引用归第 2 条（引用名
//! 带 `..` 或是绝对路径 gix 自己拒绝）；`objects/info/alternates` 不空就拒绝；multi-pack-index 里的包名能写成
//! `../` 穿出 `objects/pack/`，打开时关掉（`core.multiPackIndex=false`）；commit-graph 链、split index、`shallow`
//! 里只有十六进制的名字或提交编号。
//!
//! **挡不住的**（已知限制 KA2）：对象库深处单个松散对象文件是链接（数量可以很大，不逐个查）；检查和打开是两步
//! （`iwork-rootfs` 的时间窗）；项目根的 gitfile 与 `commondir` 指到哪个目录不限制（链接的工作树本来就在别处）。
//! 彻底封住这一类要让读操作跑在只能读仓库目录的受限子进程里，留到 M2。

use std::fmt;
use std::path::{Path, PathBuf};

use gix::Repository;

/// 拒绝打开的原因。文字固定，不带链接或 gitfile 指向的路径（那是仓库内容能摆布的、可能是仓库外的路径）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Refusal {
    /// 仓库根下没有 `.git`，或它不是目录、文件、链接
    NoDotGit,
    /// `.git` 本身是符号链接
    DotGitSymlink,
    /// 子模块的 `.git` 是目录：它在工作区里，Agent 写得进去，对象库里的链接挡不住（KA2 第 7 点）
    SubmoduleDotGitDir,
    /// `.git` 文件或 `commondir` 解析不出路径
    PointerMalformed,
    /// 子模块的 gitfile 指到上层仓库的 `modules/` 之外
    GitfileOutsideModules,
    /// git 目录或共享目录里的一个东西是符号链接；`entry` 是它在那个目录里的相对路径
    MetadataSymlink { entry: PathBuf },
    /// `objects/info/alternates` 不空
    AlternatesUnsupported,
    /// gix 打开的 git 目录或共享目录和预检查过的不是同一个
    OpenedOtherDirectory,
    /// 工作区不是登记的仓库根：`core.worktree` 指到别处，或没有工作区（裸仓库）
    WorktreeNotRoot,
}

impl Refusal {
    fn reason(&self) -> &'static str {
        match self {
            Self::NoDotGit => "仓库根下没有可用的 .git",
            Self::DotGitSymlink => ".git 是符号链接",
            Self::SubmoduleDotGitDir => {
                "子模块的 .git 是目录，只认指到上层仓库 modules/ 之下的 gitfile"
            }
            Self::PointerMalformed => ".git 文件或 commondir 解析不出路径",
            Self::GitfileOutsideModules => "子模块的 .git 指到上层仓库的 modules/ 之外",
            Self::MetadataSymlink { .. } => "git 目录里有符号链接",
            Self::AlternatesUnsupported => "借用别处的对象库（objects/info/alternates），不支持",
            Self::OpenedOtherDirectory => "打开的 git 目录和检查过的不是同一个",
            Self::WorktreeNotRoot => "仓库的工作区不是登记的仓库根",
        }
    }
}

impl fmt::Display for Refusal {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "仓库布局不支持：{}", self.reason())?;
        if let Self::MetadataSymlink { entry } = self {
            write!(f, "（{}）", entry.display())?;
        }
        Ok(())
    }
}

/// 查布局时出的错：拒绝，或读元数据本身失败。
#[derive(Debug)]
pub(crate) enum LayoutError {
    Refused(Refusal),
    /// 读失败；文字只含仓库里的位置，不含 gitfile 写的目标路径
    Io(String),
}

impl From<Refusal> for LayoutError {
    fn from(r: Refusal) -> Self {
        Self::Refused(r)
    }
}

/// 打开的是哪种仓库。
pub(crate) enum Opening<'a> {
    /// 登记的项目仓库：`.git` 是目录，或是链接的工作树的 gitfile（指到哪都行，接着查它指的目录）
    Project,
    /// 上层仓库里一个已检出的子模块：`.git` 只能是 gitfile，且真实路径落在 `modules` 里某一个目录之下
    /// （上层仓库 git 目录与共享目录下的 `modules/`，已取真实路径）
    Submodule { modules: &'a [PathBuf] },
}

/// 预检查过的两个目录（真实路径）。打开之后用它核对 gix 用的是不是这两个。
#[derive(Debug)]
pub(crate) struct Checked {
    git_dir: PathBuf,
    common_dir: PathBuf,
}

/// 交给 gix 打开的路径：`<根>/.git` 本身。路径以 `.git` 结尾时 gix 只认它，认不出就报错，不会退回去把根目录
/// 当成 git 目录打开（gix 0.90 `open/repository.rs` 的 `open_opts`）；根目录下的 `HEAD`、`config` 都没查过。
pub(crate) fn dot_git(worktree: &Path) -> PathBuf {
    worktree.join(".git")
}

/// 打开之前查：`.git` 是什么、它指到的 git 目录与共享目录里有没有链接、有没有借用别处的对象库。
///
/// gitfile 与 `commondir` 用 gix 自己的函数解析（`gix::discover::path`，gix 打开时用的就是这两个，末尾空白的
/// 处理一样），所以这里查的目录就是 gix 将要读的目录。
pub(crate) fn check_before_open(
    worktree: &Path,
    opening: &Opening<'_>,
) -> Result<Checked, LayoutError> {
    use iwork_rootfs::EntryKind;
    let dot_git = dot_git(worktree);
    let git_dir = match iwork_rootfs::inspect(worktree, Path::new(".git")) {
        Ok(EntryKind::Symlink) => return Err(Refusal::DotGitSymlink.into()),
        Ok(EntryKind::Dir) => match opening {
            Opening::Project => real_path(&dot_git)?,
            Opening::Submodule { .. } => return Err(Refusal::SubmoduleDotGitDir.into()),
        },
        Ok(EntryKind::File { .. }) => {
            // 上面已确认 `.git` 是普通文件、不是链接
            let target = gix::discover::path::from_gitdir_file(&dot_git)
                .map_err(|_| Refusal::PointerMalformed)?;
            let target = real_path(&target)?;
            if let Opening::Submodule { modules } = opening
                && !modules.iter().any(|m| target.starts_with(m))
            {
                return Err(Refusal::GitfileOutsideModules.into());
            }
            target
        }
        Ok(EntryKind::Missing | EntryKind::Other) => return Err(Refusal::NoDotGit.into()),
        Err(e) => return Err(LayoutError::Io(e.to_string())),
    };
    check_tree(&git_dir)?;
    // `commondir` 在上面走树时已确认不是链接
    let common_dir = match gix::discover::path::from_plain_file(&git_dir.join("commondir")) {
        None => git_dir.clone(),
        Some(Ok(rel)) => real_path(&git_dir.join(rel))?,
        Some(Err(_)) => return Err(Refusal::PointerMalformed.into()),
    };
    if common_dir != git_dir {
        check_tree(&common_dir)?;
    }
    Ok(Checked {
        git_dir,
        common_dir,
    })
}

/// 仓库来源的配置一律不过 gix 的信任闸。gix 把「会让它去读别处或运行程序」的值都经这道闸读：
/// `core.excludesFile`、`core.attributesFile`、`gitoxide.core.indexFile`（打开时就算好）、过滤与差异驱动、
/// `core.worktree`、`core.useReplaceRefs`、url 改写与网络、对象缓存大小等。闸外的设置（`core.fileMode`、
/// `core.ignoreCase`、`core.symlinks`、`core.precomposeUnicode`、`core.autocrlf`、`core.eol`、`core.safecrlf`）
/// gix 读的是不过闸的整份配置，照常生效。用户全局、系统配置照常过闸。代价记在 KA2 第 5 点。
fn trusted_section(meta: &gix::config::file::Metadata) -> bool {
    meta.source.kind() != gix::config::source::Kind::Repository
}

/// 打开仓库用的选项：在 `gix::open` 的默认之上
/// - 关掉配置的 include（`include.path`、`includeIf`；这一位管所有来源，KA2 第 2 点）；
/// - 仓库来源的配置不过信任闸（[`trusted_section`]）；
/// - 关掉 multi-pack-index：它里面的包名能写成 `../` 穿出 `objects/pack/`（gix 拿它直接拼路径），gix 在建对象库
///   之前读这个开关。关掉后改用每个包旁边的 `.idx`（git 一直保留），结果不变，包多时慢一些（KA2）。Api 来源排在
///   仓库配置之后，仓库里写 `true` 盖不过它。
pub(crate) fn open_options() -> gix::open::Options {
    let mut permissions = gix::open::Permissions::default();
    permissions.config.includes = false;
    gix::open::Options::default()
        .permissions(permissions)
        .filter_config_section(trusted_section)
        .config_overrides(["core.multiPackIndex=false"])
}

/// 打开之后核：gix 用的 git 目录与共享目录就是预检查过的；工作区是登记的仓库根。
///
/// `core.worktree` 不过信任闸，gix 打开时不用它；但它指到别处时，按登记的根读工作区和 git 自己看到的不是同一个
/// 地方，所以照旧拒绝（和 T09 添加项目时的口径一致）。这里只把它的值解析成真实路径来比，不读它指的东西。
pub(crate) fn check_after_open(
    repo: &Repository,
    worktree: &Path,
    checked: &Checked,
) -> Result<(), LayoutError> {
    let same = |a: &Path, b: &Path| dunce::canonicalize(a).is_ok_and(|a| a == b);
    if !same(repo.git_dir(), &checked.git_dir) || !same(repo.common_dir(), &checked.common_dir) {
        return Err(Refusal::OpenedOtherDirectory.into());
    }
    let root = real_path(worktree)?;
    let Some(workdir) = repo.workdir() else {
        return Err(Refusal::WorktreeNotRoot.into());
    };
    if !same(workdir, &root) {
        return Err(Refusal::WorktreeNotRoot.into());
    }
    let configured = repo
        .config_snapshot()
        .plumbing()
        .raw_value_filter("core.worktree", |meta| !trusted_section(meta))
        .ok();
    if let Some(value) = configured {
        let path = gix::path::from_bstring(value).map_err(|_| Refusal::WorktreeNotRoot)?;
        // git 按 git 目录解析相对的 core.worktree；绝对路径 join 之后不变
        if !same(&checked.git_dir.join(path), &root) {
            return Err(Refusal::WorktreeNotRoot.into());
        }
    }
    Ok(())
}

/// 取真实路径；失败时不把路径写进错误（它可能是 gitfile 里写的、仓库外的路径）。
fn real_path(path: &Path) -> Result<PathBuf, LayoutError> {
    dunce::canonicalize(path)
        .map_err(|e| LayoutError::Io(format!("取 git 目录的真实路径失败：{e}")))
}

/// 走树时怎么对待 git 目录顶层的某些条目。只排除当前读操作（`baseline`、`status`、`diff`、`worktree_versions`、
/// 读子模块 `HEAD`）根本不读的地方。**以后加了会读它们的操作（例如读 reflog 要读 `logs/`），要回来重核这张表。**
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Skip {
    /// 照常查，往下走
    No,
    /// 本身不能是链接，但不往下走：
    /// - `objects/<两位十六进制>/`：松散对象，数量可以很大，单个文件是链接不逐个查（KA2 第 3 点）；目录本身是链接
    ///   就等于整个扇区都在别处，照样拒绝。
    /// - `modules/`：子模块的 git 目录，打开子模块时查它自己那一份；它本身是链接的话，子模块 gitfile 的「落在
    ///   `modules/` 之下」就会被带到别处，所以本身要查。
    Descend,
    /// 整个不看（本身是链接也行）：gix 的读操作不碰它们。
    /// - `hooks/`：只有写操作运行钩子；常见做法是把它整个链到共享的钩子目录。
    /// - `logs/`：reflog，读状态与差异不用。
    /// - `lfs/`：git-lfs 自己的缓存，gix 不读。
    Entirely,
}

fn skip(rel: &Path) -> Skip {
    let mut parts = rel.iter().map(|p| p.to_str());
    match (parts.next(), parts.next(), parts.next()) {
        (Some(Some("hooks" | "logs" | "lfs")), None, _) => Skip::Entirely,
        (Some(Some("modules")), None, _) => Skip::Descend,
        (Some(Some("objects")), Some(Some(fan)), None)
            if fan.len() == 2 && fan.bytes().all(|b| b.is_ascii_hexdigit()) =>
        {
            Skip::Descend
        }
        _ => Skip::No,
    }
}

/// git 目录（或共享目录）`dir` 整棵树里有链接就拒绝（不跟随，排除项见 [`Skip`]）；`objects/info/alternates`
/// 不能有内容。
fn check_tree(dir: &Path) -> Result<(), LayoutError> {
    refuse_links_in_tree(dir, Path::new(""))?;
    let alternates = dir.join("objects/info/alternates");
    match std::fs::symlink_metadata(&alternates) {
        Ok(meta) if meta.len() > 0 => Err(Refusal::AlternatesUnsupported.into()),
        Ok(_) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(LayoutError::Io(format!("{}：{e}", alternates.display()))),
    }
}

/// `dir/sub` 下面的每一项：先看它本身是不是链接，是目录再往下走。往下走之前它已确认不是链接，所以 `read_dir`
/// 不会被带到别处。
fn refuse_links_in_tree(dir: &Path, sub: &Path) -> Result<(), LayoutError> {
    let io = |e: std::io::Error| LayoutError::Io(format!("{}：{e}", dir.join(sub).display()));
    let entries = match std::fs::read_dir(dir.join(sub)) {
        Ok(entries) => entries,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(e) => return Err(io(e)),
    };
    for entry in entries {
        let entry = entry.map_err(io)?;
        let rel = sub.join(entry.file_name());
        let rule = skip(&rel);
        if rule == Skip::Entirely {
            continue;
        }
        // DirEntry::file_type 不跟随链接
        let file_type = entry.file_type().map_err(io)?;
        if file_type.is_symlink() {
            return Err(Refusal::MetadataSymlink { entry: rel }.into());
        }
        if file_type.is_dir() && rule == Skip::No {
            refuse_links_in_tree(dir, &rel)?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn skip_table() {
        assert_eq!(skip(Path::new("hooks")), Skip::Entirely);
        assert_eq!(skip(Path::new("logs")), Skip::Entirely);
        assert_eq!(skip(Path::new("lfs")), Skip::Entirely);
        assert_eq!(skip(Path::new("modules")), Skip::Descend);
        assert_eq!(skip(Path::new("objects/ab")), Skip::Descend);
        // 只排除顶层与对象扇区本身，同名的深处条目照常查
        assert_eq!(skip(Path::new("refs/heads/hooks")), Skip::No);
        assert_eq!(skip(Path::new("worktrees/w/logs")), Skip::No);
        assert_eq!(skip(Path::new("objects/pack")), Skip::No);
        assert_eq!(skip(Path::new("objects/info")), Skip::No);
        assert_eq!(skip(Path::new("objects/abc")), Skip::No);
        assert_eq!(skip(Path::new("objects/zz")), Skip::No);
        assert_eq!(skip(Path::new("refs")), Skip::No);
    }

    #[test]
    fn refusal_text_names_reason_without_target() {
        let r = Refusal::MetadataSymlink {
            entry: PathBuf::from("refs/heads/x"),
        };
        assert_eq!(
            r.to_string(),
            "仓库布局不支持：git 目录里有符号链接（refs/heads/x）"
        );
    }
}
