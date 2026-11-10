//! `ProjectMarkerStore` 与 `ProjectDirectories` 的本机文件系统实现（技术设计 8.2、9.3；ADR-0010）。

use std::io::ErrorKind;
use std::path::{Path, PathBuf};

use iwork_domain::id::ProjectId;
use iwork_ports::project_files::{ProjectDirectories, ProjectFilesError, ProjectMarkerStore};

/// 项目标记在仓库根下的相对位置。
pub const MARKER_DIR: &str = ".iwork";
pub const MARKER_FILE: &str = "project.toml";
/// 忽略项目标记的 `.gitignore`，与标记同在 `.iwork/` 下。
pub const IGNORE_FILE: &str = ".gitignore";
/// 项目标记里存项目 ID 的键。
const ID_KEY: &str = "id";

/// 新建与克隆的默认父目录：`~/iWork/projects`（Windows 为 `%USERPROFILE%\iWork\projects`）。
/// 全局设置 `projects_root` 的初始值；用户可在设置里改。拿不到家目录时返回 `None`。
pub fn default_projects_root() -> Option<PathBuf> {
    std::env::home_dir().map(|home| home.join("iWork").join("projects"))
}

/// `root` 下项目标记的路径。
pub fn marker_path(root: &Path) -> PathBuf {
    root.join(MARKER_DIR).join(MARKER_FILE)
}

/// 本机文件系统上的项目标记。
#[derive(Debug, Clone, Copy, Default)]
pub struct FsProjectMarkerStore;

impl ProjectMarkerStore for FsProjectMarkerStore {
    async fn read(&self, root: &Path) -> Result<Option<ProjectId>, ProjectFilesError> {
        let root = root.to_owned();
        blocking(move || {
            let rel = marker_rel();
            let Some(bytes) = iwork_rootfs::read_file(&root, &rel).map_err(|e| rooted(&root, e))?
            else {
                return Ok(None);
            };
            let path = root.join(&rel);
            let text =
                String::from_utf8(bytes).map_err(|_| ProjectFilesError::MarkerMalformed {
                    path: path.clone(),
                    message: "不是 UTF-8 文本".to_owned(),
                })?;
            parse_marker(&text)
                .map(Some)
                .map_err(|message| ProjectFilesError::MarkerMalformed { path, message })
        })
        .await
    }

    async fn write(&self, root: &Path, id: ProjectId) -> Result<(), ProjectFilesError> {
        let root = root.to_owned();
        blocking(move || {
            iwork_rootfs::ensure_dir(&root, Path::new(MARKER_DIR)).map_err(|e| rooted(&root, e))?;
            // 先忽略再写标记：中途失败也不会留下一个会被提交的标记
            ensure_marker_ignored(&root)?;
            let rel = marker_rel();
            // 独占新建：已有文件（包括链接）时失败而不是覆盖（端口约定）
            match iwork_rootfs::create_new_file(&root, &rel, render_marker(id).as_bytes()) {
                Ok(()) => Ok(()),
                Err(iwork_rootfs::RootedError::Io { source, .. })
                    if source.kind() == ErrorKind::AlreadyExists =>
                {
                    Err(ProjectFilesError::MarkerExists {
                        path: root.join(rel),
                    })
                }
                Err(e) => Err(rooted(&root, e)),
            }
        })
        .await
    }
}

fn marker_rel() -> PathBuf {
    Path::new(MARKER_DIR).join(MARKER_FILE)
}

/// 文件操作都是小文件的同步读写，放到阻塞线程池里做。
async fn blocking<T: Send + 'static>(
    f: impl FnOnce() -> Result<T, ProjectFilesError> + Send + 'static,
) -> Result<T, ProjectFilesError> {
    tokio::task::spawn_blocking(f)
        .await
        .map_err(|e| ProjectFilesError::Io {
            path: PathBuf::new(),
            message: format!("后台线程异常结束：{e}"),
        })?
}

/// `iwork-rootfs` 的错误 → 端口错误：碰到链接一律是 `SymlinkRefused`，点名是哪个路径。
fn rooted(root: &Path, e: iwork_rootfs::RootedError) -> ProjectFilesError {
    use iwork_rootfs::RootedError as R;
    match e {
        R::ThroughSymlink { link } => ProjectFilesError::SymlinkRefused { path: link },
        R::FinalSymlink { path } => ProjectFilesError::SymlinkRefused { path },
        R::InvalidRelative { path } | R::NotAFile { path } => ProjectFilesError::Io {
            message: format!("{} 不是普通文件或目录", path.display()),
            path: root.join(path),
        },
        R::Io { path, source } => ProjectFilesError::Io {
            path,
            message: source.to_string(),
        },
    }
}

/// 让版本控制忽略项目标记：项目 ID 只属于本机这份工作副本，不随仓库共享（技术设计 8.2，产品负责人
/// 2026-11-09 决定）。`.iwork/.gitignore` 不存在就创建；已有就保留原内容，必要时在末尾追加一行
/// （见 [`with_ignore_line`]）；重复调用结果不变。
///
/// 只写一个忽略文件，不调用任何版本控制命令；不跟随符号链接。
fn ensure_marker_ignored(root: &Path) -> Result<(), ProjectFilesError> {
    let rel = Path::new(MARKER_DIR).join(IGNORE_FILE);
    let existing = iwork_rootfs::read_file(root, &rel)
        .map_err(|e| rooted(root, e))?
        .unwrap_or_default();
    let existing = String::from_utf8(existing).map_err(|_| ProjectFilesError::Io {
        path: root.join(&rel),
        message: "不是 UTF-8 文本，不敢改它".to_owned(),
    })?;
    let Some(updated) = with_ignore_line(&existing) else {
        return Ok(());
    };
    iwork_rootfs::replace_file(root, &rel, updated.as_bytes()).map_err(|e| rooted(root, e))
}

/// 忽略项目标记的那一行（与 [`MARKER_FILE`] 对应，见测试 `ignore_line_names_the_marker_file`）。
const IGNORE_LINE: &str = "/project.toml";

/// 保证忽略文件里**最后一条生效的规则**就是 [`IGNORE_LINE`]；已经是了返回 `None`，否则在末尾追加一行。
///
/// 只看最后一条规则，不去判断前面的规则会不会匹配：gitignore 后面的规则覆盖前面的（例如 `!*.toml`
/// 会把前面的忽略取消），最后一条是我们的就一定生效，不用实现一遍 glob 匹配。空行与 `#` 注释不算规则。
/// 原有内容一字不改（前导空格是模式的一部分，不 trim）。
fn with_ignore_line(existing: &str) -> Option<String> {
    let last_rule = existing
        .lines()
        .rev()
        .find(|l| !l.trim_end_matches(' ').is_empty() && !l.starts_with('#'));
    if last_rule == Some(IGNORE_LINE) {
        return None;
    }
    let mut out = existing.to_owned();
    if !out.is_empty() && !out.ends_with('\n') {
        out.push('\n');
    }
    out.push_str(IGNORE_LINE);
    out.push('\n');
    Some(out)
}

/// 项目标记的文本。ULID 文本只有大写字母与数字，不用转义。
pub fn render_marker(id: ProjectId) -> String {
    format!(
        "# iWork 项目身份：iWork 靠这个 ID 认出本仓库，仓库移动后也能重新关联。请勿修改。\n\
         {ID_KEY} = \"{id}\"\n"
    )
}

/// 从项目标记文本里取项目 ID；其他键忽略，留给以后的版本。
fn parse_marker(text: &str) -> Result<ProjectId, String> {
    let table: toml::Table = text.parse().map_err(|e| format!("不是合法的 TOML：{e}"))?;
    let value = table
        .get(ID_KEY)
        .ok_or_else(|| format!("缺少键 `{ID_KEY}`"))?;
    let text = value
        .as_str()
        .ok_or_else(|| format!("键 `{ID_KEY}` 不是字符串"))?;
    text.parse::<ProjectId>().map_err(|e| e.to_string())
}

/// 本机文件系统上的新建项目目录。
#[derive(Debug, Clone, Copy, Default)]
pub struct FsProjectDirectories;

impl ProjectDirectories for FsProjectDirectories {
    async fn create_project_dir(
        &self,
        parent: &Path,
        name: &str,
    ) -> Result<PathBuf, ProjectFilesError> {
        validate_dir_name(name)?;
        tokio::fs::create_dir_all(parent)
            .await
            .map_err(|e| io(parent, &e))?;
        let target = parent.join(name);
        match tokio::fs::create_dir(&target).await {
            Ok(()) => return Ok(target),
            Err(e) if e.kind() == ErrorKind::AlreadyExists => {}
            Err(e) => return Err(io(&target, &e)),
        }
        // 已存在：只有空目录可以用
        let mut entries = match tokio::fs::read_dir(&target).await {
            Ok(entries) => entries,
            // 是文件（或别的不能当目录读的东西）
            Err(_) => return Err(ProjectFilesError::TargetNotEmpty { path: target }),
        };
        match entries.next_entry().await {
            Ok(None) => Ok(target),
            Ok(Some(_)) => Err(ProjectFilesError::TargetNotEmpty { path: target }),
            Err(e) => Err(io(&target, &e)),
        }
    }
}

/// 项目名要能直接当一级目录名用。不替换字符：改了名字用户找不到目录，让界面提示用户改。
fn validate_dir_name(name: &str) -> Result<(), ProjectFilesError> {
    let reason = if name.trim().is_empty() {
        Some("为空")
    } else if name == "." || name == ".." {
        Some("是 . 或 ..")
    } else if name.contains(['/', '\\']) {
        Some("含路径分隔符")
    } else if name.contains(['\0', ':', '*', '?', '"', '<', '>', '|'])
        || name.chars().any(char::is_control)
    {
        // Windows 不允许这些字符；三个平台统一拒绝，同一个名字在哪都能用
        Some("含文件名不允许的字符")
    } else if name.ends_with(['.', ' ']) {
        Some("以点或空格结尾（Windows 不允许）")
    } else {
        None
    };
    match reason {
        Some(reason) => Err(ProjectFilesError::InvalidName {
            name: name.to_owned(),
            reason: reason.to_owned(),
        }),
        None => Ok(()),
    }
}

fn io(path: &Path, e: &std::io::Error) -> ProjectFilesError {
    ProjectFilesError::Io {
        path: path.to_owned(),
        message: e.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rendered_marker_parses_back_to_the_same_id() {
        let id: ProjectId = "01JC0000000000000000000000".parse().unwrap();
        assert_eq!(parse_marker(&render_marker(id)), Ok(id));
    }

    #[test]
    fn ignore_line_is_appended_unless_it_is_already_the_last_rule() {
        assert_eq!(with_ignore_line(""), Some("/project.toml\n".to_owned()));
        assert_eq!(
            with_ignore_line("*.log"),
            Some("*.log\n/project.toml\n".to_owned())
        );
        assert_eq!(
            with_ignore_line("# mine\ncache/\n"),
            Some("# mine\ncache/\n/project.toml\n".to_owned())
        );
        assert_eq!(
            with_ignore_line("a\n/project.toml\n"),
            None,
            "已是最后一条规则"
        );
        assert_eq!(
            with_ignore_line("/project.toml\n\n# note\n"),
            None,
            "后面只有空行和注释也算最后一条"
        );
    }

    #[test]
    fn ignore_line_names_the_marker_file() {
        assert_eq!(IGNORE_LINE, format!("/{MARKER_FILE}"));
    }

    /// F-4：后面有否定规则时，前面的忽略不生效，要在末尾再加一行。
    #[test]
    fn later_negation_forces_a_new_trailing_ignore_line() {
        assert_eq!(
            with_ignore_line("/project.toml\n!*.toml\n"),
            Some("/project.toml\n!*.toml\n/project.toml\n".to_owned())
        );
    }

    /// F-4：前导空格是模式的一部分（` /project.toml` 不匹配标记），不能 trim 掉。
    #[test]
    fn leading_space_is_significant() {
        assert_eq!(
            with_ignore_line(" /project.toml\n"),
            Some(" /project.toml\n/project.toml\n".to_owned())
        );
    }

    #[test]
    fn marker_parsing_names_what_is_wrong() {
        assert!(parse_marker("id = ").unwrap_err().contains("TOML"));
        assert!(parse_marker("name = \"x\"").unwrap_err().contains("缺少键"));
        assert!(parse_marker("id = 3").unwrap_err().contains("不是字符串"));
        assert!(parse_marker("id = \"not-a-ulid\"").is_err());
        // 对照：多出来的键不影响
        assert!(parse_marker("id = \"01JC0000000000000000000000\"\nextra = 1").is_ok());
    }

    #[test]
    fn directory_names_must_be_a_single_portable_component() {
        for bad in [
            "", "  ", ".", "..", "a/b", "a\\b", "a:b", "x?", "trail.", "trail ",
        ] {
            assert!(validate_dir_name(bad).is_err(), "{bad:?} 应被拒绝");
        }
        for good in ["demo", "我的项目", "my-app_2", ".hidden-ok"] {
            assert!(validate_dir_name(good).is_ok(), "{good:?} 应可用");
        }
    }
}
