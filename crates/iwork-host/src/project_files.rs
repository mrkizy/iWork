//! `ProjectMarkerStore` 与 `ProjectDirectories` 的本机文件系统实现（技术设计 8.2、9.3；ADR-0006）。

use std::io::ErrorKind;
use std::path::{Path, PathBuf};

use iwork_domain::id::ProjectId;
use iwork_ports::project_files::{ProjectDirectories, ProjectFilesError, ProjectMarkerStore};
use tokio::io::AsyncWriteExt;

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
        let path = marker_path(root);
        let text = match tokio::fs::read_to_string(&path).await {
            Ok(text) => text,
            Err(e) if e.kind() == ErrorKind::NotFound => return Ok(None),
            Err(e) => return Err(io(&path, &e)),
        };
        parse_marker(&text)
            .map(Some)
            .map_err(|message| ProjectFilesError::MarkerMalformed { path, message })
    }

    async fn write(&self, root: &Path, id: ProjectId) -> Result<(), ProjectFilesError> {
        let dir = root.join(MARKER_DIR);
        tokio::fs::create_dir_all(&dir)
            .await
            .map_err(|e| io(&dir, &e))?;
        // 先忽略再写标记：中途失败也不会留下一个会被提交的标记
        ensure_marker_ignored(&dir).await?;
        let path = marker_path(root);
        // create_new：已有文件时失败而不是覆盖（端口约定）
        let file = tokio::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&path)
            .await;
        let mut file = match file {
            Ok(f) => f,
            Err(e) if e.kind() == ErrorKind::AlreadyExists => {
                return Err(ProjectFilesError::MarkerExists { path });
            }
            Err(e) => return Err(io(&path, &e)),
        };
        file.write_all(render_marker(id).as_bytes())
            .await
            .map_err(|e| io(&path, &e))?;
        file.sync_all().await.map_err(|e| io(&path, &e))
    }
}

/// 让版本控制忽略项目标记：项目 ID 只属于本机这份工作副本，不随仓库共享（技术设计 8.2，产品负责人
/// 2026-11-09 决定）。`.iwork/.gitignore` 不存在就创建；已有就保留原内容、在末尾追加一行；
/// 已经有这一行就不动，重复调用结果不变。
///
/// 只写一个忽略文件，不调用任何版本控制命令；不认识 `.gitignore` 的工具不受影响。
async fn ensure_marker_ignored(iwork_dir: &Path) -> Result<(), ProjectFilesError> {
    let path = iwork_dir.join(IGNORE_FILE);
    let existing = match tokio::fs::read_to_string(&path).await {
        Ok(text) => text,
        Err(e) if e.kind() == ErrorKind::NotFound => String::new(),
        Err(e) => return Err(io(&path, &e)),
    };
    let Some(updated) = with_ignore_line(&existing) else {
        return Ok(());
    };
    tokio::fs::write(&path, updated)
        .await
        .map_err(|e| io(&path, &e))
}

/// 在忽略文件内容末尾补上忽略项目标记的一行；已经有了返回 `None`。
fn with_ignore_line(existing: &str) -> Option<String> {
    let line = format!("/{MARKER_FILE}");
    let already = existing
        .lines()
        .map(str::trim)
        .any(|l| l == line || l == MARKER_FILE);
    if already {
        return None;
    }
    let mut out = existing.to_owned();
    if !out.is_empty() && !out.ends_with('\n') {
        out.push('\n');
    }
    out.push_str(&line);
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
    fn ignore_line_is_appended_once_and_keeps_existing_content() {
        assert_eq!(with_ignore_line(""), Some("/project.toml\n".to_owned()));
        assert_eq!(
            with_ignore_line("*.log"),
            Some("*.log\n/project.toml\n".to_owned())
        );
        assert_eq!(
            with_ignore_line("# mine\ncache/\n"),
            Some("# mine\ncache/\n/project.toml\n".to_owned())
        );
        assert_eq!(with_ignore_line("a\n/project.toml\n"), None);
        assert_eq!(
            with_ignore_line("project.toml\n"),
            None,
            "不带斜杠的写法也算"
        );
        // 对照：只是包含这个词的行不算
        assert!(with_ignore_line("old-project.toml\n").is_some());
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
