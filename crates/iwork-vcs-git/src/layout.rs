//! 交给 gix 之前先查仓库的布局（T10a 方案 2.4a）。
//!
//! gix 打开仓库、读状态时会顺着仓库里的东西去读别处：`.git` 是链接或 gitfile、git 目录里的元数据是链接、
//! `config` 里的 include 与指向文件的键、`objects/info/alternates`。仓库内容来自用户或 Agent，这些都能被指到
//! 凭据目录或数据目录，后台在沙箱外读到就违反了底线。这里在打开之前查布局、打开时关掉 include、打开后删掉仓库
//! 自己配置里指向文件的键并核对工作区；有一处不对就拒绝，不交给 gix。
//!
//! **挡不住的**（已知限制 KA2）：对象库深处单个松散对象文件是链接（数量可以很大，不逐个查）；检查和打开是两步
//! （`iwork-rootfs` 的时间窗）。项目根的 gitfile 与 `commondir` 指到哪个目录不限制（链接的工作树本来就在别处）。

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
    /// `.git` 文件不是一行 `gitdir: <路径>`
    GitfileMalformed,
    /// 子模块的 gitfile 指到上层仓库的 `modules/` 之外
    GitfileOutsideModules,
    /// git 目录或共享目录里的一个元数据是符号链接；`entry` 是它在那个目录里的相对路径
    MetadataSymlink { entry: PathBuf },
    /// `objects/info/alternates` 不空
    AlternatesUnsupported,
    /// gix 认的工作区不是登记的仓库根（`core.worktree` 指到别处），或没有工作区（裸仓库）
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
            Self::GitfileMalformed => ".git 文件不是一行 gitdir: <路径>",
            Self::GitfileOutsideModules => "子模块的 .git 指到上层仓库的 modules/ 之外",
            Self::MetadataSymlink { .. } => "git 目录里有符号链接",
            Self::AlternatesUnsupported => "借用别处的对象库（objects/info/alternates），不支持",
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

/// gitfile 与 `commondir` 这种一行路径的小文件，超过这么大就不是合法的。
const POINTER_FILE_LIMIT: u64 = 4096;

/// 打开之前查：`.git` 是什么、它指到的 git 目录与共享目录里有没有链接、有没有借用别处的对象库。
pub(crate) fn check_before_open(worktree: &Path, opening: &Opening<'_>) -> Result<(), LayoutError> {
    use iwork_rootfs::EntryKind;
    let dot_git = Path::new(".git");
    let git_dir = match iwork_rootfs::inspect(worktree, dot_git) {
        Ok(EntryKind::Symlink) => return Err(Refusal::DotGitSymlink.into()),
        Ok(EntryKind::Dir) => match opening {
            Opening::Project => worktree.join(dot_git),
            Opening::Submodule { .. } => return Err(Refusal::SubmoduleDotGitDir.into()),
        },
        Ok(EntryKind::File { .. }) => {
            let target = follow_pointer_file(worktree, dot_git, PointerFormat::Gitfile)?;
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
    check_git_dir(&git_dir)?;
    // `commondir` 已在上面作为直属条目查过不是链接
    if git_dir.join("commondir").exists() {
        let common = follow_pointer_file(&git_dir, Path::new("commondir"), PointerFormat::Plain)?;
        if common != git_dir {
            check_git_dir(&common)?;
        }
    }
    Ok(())
}

/// 打开仓库用的选项：和 `gix::open` 的默认一样，只关掉配置的 include（`include.path`、`includeIf`）。
/// 这一位管所有来源的配置，用户全局配置里的 include 也不跟（KA2 第 2 点）。
pub(crate) fn open_options() -> gix::open::Options {
    let mut permissions = gix::open::Permissions::default();
    permissions.config.includes = false;
    gix::open::Options::default().permissions(permissions)
}

/// 仓库自己配置里指向文件的键：gix 读状态时会去读这些文件（跟链接），而 Agent 用 `git config` 就能改它们。
const REPO_FILE_KEYS: [&str; 2] = ["excludesFile", "attributesFile"];

/// 在内存配置上删掉仓库来源（`Local`、`Worktree`）的 [`REPO_FILE_KEYS`]；用户全局、系统配置里的照留
/// （KA2 第 5 点）。不改磁盘上的配置。
pub(crate) fn drop_repo_file_keys(config: &mut gix::config::File) {
    use gix::config::Source;
    let ids: Vec<_> = config
        .sections_and_ids_by_name("core")
        .into_iter()
        .flatten()
        .filter(|(section, _)| {
            section.header().subsection_name().is_none()
                && matches!(section.meta().source, Source::Local | Source::Worktree)
        })
        .map(|(_, id)| id)
        .collect();
    for id in ids {
        let Some(mut section) = config.section_mut_by_id(id) else {
            continue;
        };
        for key in REPO_FILE_KEYS {
            while section.remove(key).is_some() {}
        }
    }
}

/// 打开之后核：gix 认的工作区的真实路径等于登记的仓库根。这一步读的只是仓库自己的配置，是为了后面按登记的根读
/// 工作区文件时两边说的是同一个地方。
pub(crate) fn check_after_open(repo: &Repository, worktree: &Path) -> Result<(), LayoutError> {
    let Some(workdir) = repo.workdir() else {
        return Err(Refusal::WorktreeNotRoot.into());
    };
    let expected = dunce::canonicalize(worktree)
        .map_err(|e| LayoutError::Io(format!("取仓库根的真实路径失败：{e}")))?;
    match dunce::canonicalize(workdir) {
        Ok(actual) if actual == expected => Ok(()),
        // 工作区指到一个不存在的地方也是「不是登记的根」；不把那个路径写进错误
        _ => Err(Refusal::WorktreeNotRoot.into()),
    }
}

#[derive(Clone, Copy)]
enum PointerFormat {
    /// `.git` 文件：`gitdir: <路径>`
    Gitfile,
    /// `commondir`：整行就是路径
    Plain,
}

/// 读 `dir` 里的一行路径小文件（不跟链接），相对路径按 `dir` 解析，返回目标的真实路径。
fn follow_pointer_file(
    dir: &Path,
    name: &Path,
    format: PointerFormat,
) -> Result<PathBuf, LayoutError> {
    let io =
        |what: &str| LayoutError::Io(format!("{} 的 {}：{what}", dir.display(), name.display()));
    let len = std::fs::symlink_metadata(dir.join(name))
        .map_err(|e| io(&e.to_string()))?
        .len();
    if len > POINTER_FILE_LIMIT {
        return Err(Refusal::GitfileMalformed.into());
    }
    let bytes = iwork_rootfs::read_file(dir, name)
        .map_err(|e| io(&e.to_string()))?
        .unwrap_or_default();
    let line = bytes.strip_suffix(b"\n").unwrap_or(&bytes);
    let line = line.strip_suffix(b"\r").unwrap_or(line);
    let path = match format {
        PointerFormat::Gitfile => line
            .strip_prefix(b"gitdir: ")
            .ok_or(Refusal::GitfileMalformed)?,
        PointerFormat::Plain => line,
    };
    if path.is_empty() {
        return Err(Refusal::GitfileMalformed.into());
    }
    let path = gix::path::from_byte_slice(path).map_err(|_| Refusal::GitfileMalformed)?;
    // 指到的地方不存在，或取真实路径失败：不把 gitfile 里写的路径带进错误
    dunce::canonicalize(dir.join(path)).map_err(|_| io("指到的目录取不到真实路径"))
}

/// git 目录（或共享目录）`dir` 里不能有链接的地方：直属条目，`info/`、`objects/`、`objects/info/`、
/// `objects/pack/` 的直属条目，`refs/` 整棵树；`objects/info/alternates` 不能有内容。
///
/// 先查上一层再进下一层：进 `objects/` 之前它已作为直属条目确认不是链接，所以 `read_dir` 不会被带到别处。
fn check_git_dir(dir: &Path) -> Result<(), LayoutError> {
    for sub in ["", "info", "objects", "objects/info", "objects/pack"] {
        refuse_links_among(dir, Path::new(sub))?;
    }
    refuse_links_in_tree(dir, Path::new("refs"))?;
    let alternates = dir.join("objects/info/alternates");
    match std::fs::symlink_metadata(&alternates) {
        Ok(meta) if meta.len() > 0 => Err(Refusal::AlternatesUnsupported.into()),
        Ok(_) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(LayoutError::Io(format!("{}：{e}", alternates.display()))),
    }
}

/// `dir/sub` 的直属条目里有链接就拒绝；`dir/sub` 不存在不算错。
fn refuse_links_among(dir: &Path, sub: &Path) -> Result<(), LayoutError> {
    for_each_entry(dir, sub, |rel, file_type| {
        if file_type.is_symlink() {
            return Err(Refusal::MetadataSymlink { entry: rel }.into());
        }
        Ok(())
    })
}

/// `dir/sub` 整棵树里有链接就拒绝（不跟随：只往真目录里走）。
fn refuse_links_in_tree(dir: &Path, sub: &Path) -> Result<(), LayoutError> {
    for_each_entry(dir, sub, |rel, file_type| {
        if file_type.is_symlink() {
            return Err(Refusal::MetadataSymlink { entry: rel }.into());
        }
        if file_type.is_dir() {
            refuse_links_in_tree(dir, &rel)?;
        }
        Ok(())
    })
}

/// 对 `dir/sub` 的每个直属条目调 `f(相对 dir 的路径, 不跟随链接的类型)`。
fn for_each_entry(
    dir: &Path,
    sub: &Path,
    mut f: impl FnMut(PathBuf, std::fs::FileType) -> Result<(), LayoutError>,
) -> Result<(), LayoutError> {
    let io = |e: std::io::Error| LayoutError::Io(format!("{}：{e}", dir.join(sub).display()));
    let entries = match std::fs::read_dir(dir.join(sub)) {
        Ok(entries) => entries,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(e) => return Err(io(e)),
    };
    for entry in entries {
        let entry = entry.map_err(io)?;
        // DirEntry::file_type 不跟随链接
        let file_type = entry.file_type().map_err(io)?;
        f(sub.join(entry.file_name()), file_type)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use gix::config::{File, Source, file::Metadata};

    fn file(source: Source, text: &str) -> File {
        File::from_bytes_owned(
            &mut text.as_bytes().to_vec(),
            Metadata::from(source),
            Default::default(),
        )
        .unwrap()
    }

    /// 仓库来源的两个键（不分大小写、可以出现多次）删掉；用户全局与系统配置里的、别的键、带子节名的照留。
    #[test]
    fn drops_only_repo_sourced_file_keys() {
        let mut config = file(
            Source::User,
            "[core]\n\texcludesFile = /user/ignore\n\tattributesFile = /user/attrs\n",
        );
        config
            .append(file(
                Source::System,
                "[core]\n\texcludesFile = /system/ignore\n",
            ))
            .unwrap();
        config.append(file(
            Source::Local,
            "[core]\n\texcludesfile = /repo/a\n\tEXCLUDESFILE = /repo/b\n\tattributesFile = /repo/c\n\tfileMode = false\n[core \"x\"]\n\texcludesFile = /repo/sub\n",
        ))
        .unwrap();
        config
            .append(file(
                Source::Worktree,
                "[core]\n\tattributesFile = /wt/attrs\n",
            ))
            .unwrap();

        drop_repo_file_keys(&mut config);

        let values = |key: &str| -> Vec<String> {
            config
                .strings(key)
                .unwrap_or_default()
                .into_iter()
                .map(|v| v.to_string())
                .collect()
        };
        assert_eq!(
            values("core.excludesFile"),
            ["/user/ignore", "/system/ignore"]
        );
        assert_eq!(values("core.attributesFile"), ["/user/attrs"]);
        assert_eq!(values("core.fileMode"), ["false"]);
        assert_eq!(values("core.x.excludesFile"), ["/repo/sub"]);
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
