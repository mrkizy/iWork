//! 项目标记与新建项目目录的文件系统行为（T08 验收 4a 与项目身份的落盘部分）。

use std::path::Path;

use iwork_domain::id::ProjectId;
use iwork_host::project_files::{FsProjectDirectories, FsProjectMarkerStore, marker_path};
use iwork_ports::project_files::{ProjectDirectories, ProjectFilesError, ProjectMarkerStore};

fn id(text: &str) -> ProjectId {
    text.parse().expect("valid ulid")
}

fn tmp() -> tempfile::TempDir {
    tempfile::Builder::new()
        .prefix("iwork-host-")
        .tempdir()
        .expect("temp dir")
}

#[tokio::test]
async fn marker_round_trips_and_is_never_overwritten() {
    let dir = tmp();
    let root = dir.path();
    let store = FsProjectMarkerStore;
    let first = id("01JC0000000000000000000001");

    assert_eq!(store.read(root).await, Ok(None), "没有标记时是 None");
    store.write(root, first).await.expect("write");
    assert_eq!(store.read(root).await, Ok(Some(first)));
    assert!(root.join(".iwork/project.toml").is_file());

    let second = id("01JC0000000000000000000002");
    assert_eq!(
        store.write(root, second).await,
        Err(ProjectFilesError::MarkerExists {
            path: marker_path(root)
        })
    );
    assert_eq!(store.read(root).await, Ok(Some(first)), "原 ID 不变");
    assert_eq!(
        std::fs::read_to_string(root.join(".iwork/.gitignore")).unwrap(),
        "/project.toml\n",
        "写标记时忽略它，第二次写不重复追加"
    );
}

#[tokio::test]
async fn marker_ignore_line_is_appended_to_an_existing_gitignore() {
    let dir = tmp();
    let root = dir.path();
    std::fs::create_dir_all(root.join(".iwork")).unwrap();
    std::fs::write(root.join(".iwork/.gitignore"), "# 用户自己的\ncache/").unwrap();

    FsProjectMarkerStore
        .write(root, id("01JC0000000000000000000003"))
        .await
        .expect("write");
    assert_eq!(
        std::fs::read_to_string(root.join(".iwork/.gitignore")).unwrap(),
        "# 用户自己的\ncache/\n/project.toml\n"
    );
}

#[tokio::test]
async fn malformed_marker_is_an_error_naming_the_file() {
    let dir = tmp();
    let path = marker_path(dir.path());
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(&path, "id = \"garbage\"\n").unwrap();

    match FsProjectMarkerStore.read(dir.path()).await {
        Err(ProjectFilesError::MarkerMalformed { path: p, .. }) => assert_eq!(p, path),
        other => panic!("应报标记损坏，实际 {other:?}"),
    }
}

#[tokio::test]
async fn new_project_dir_creates_missing_parents() {
    let dir = tmp();
    let parent = dir.path().join("home/iWork/projects");
    assert!(!parent.exists());

    let target = FsProjectDirectories
        .create_project_dir(&parent, "demo")
        .await
        .expect("create");
    assert_eq!(target, parent.join("demo"));
    assert!(is_empty_dir(&target));
}

#[tokio::test]
async fn new_project_dir_accepts_existing_empty_dir_and_refuses_non_empty() {
    let dir = tmp();
    let parent = dir.path();
    std::fs::create_dir(parent.join("empty")).unwrap();
    assert_eq!(
        FsProjectDirectories
            .create_project_dir(parent, "empty")
            .await,
        Ok(parent.join("empty"))
    );

    std::fs::create_dir(parent.join("busy")).unwrap();
    std::fs::write(parent.join("busy/keep.txt"), "mine").unwrap();
    assert_eq!(
        FsProjectDirectories
            .create_project_dir(parent, "busy")
            .await,
        Err(ProjectFilesError::TargetNotEmpty {
            path: parent.join("busy")
        })
    );
    assert_eq!(
        std::fs::read_to_string(parent.join("busy/keep.txt")).unwrap(),
        "mine"
    );

    std::fs::write(parent.join("a-file"), "x").unwrap();
    assert_eq!(
        FsProjectDirectories
            .create_project_dir(parent, "a-file")
            .await,
        Err(ProjectFilesError::TargetNotEmpty {
            path: parent.join("a-file")
        })
    );
}

#[tokio::test]
async fn new_project_dir_rejects_names_that_escape_the_parent() {
    let dir = tmp();
    for name in ["../escape", "..", "a/b"] {
        assert!(matches!(
            FsProjectDirectories
                .create_project_dir(dir.path(), name)
                .await,
            Err(ProjectFilesError::InvalidName { .. })
        ));
    }
    assert!(!dir.path().parent().unwrap().join("escape").exists());
}

#[test]
fn default_projects_root_is_under_home() {
    let root = iwork_host::project_files::default_projects_root().expect("home dir");
    assert!(
        root.ends_with(Path::new("iWork").join("projects")),
        "{}",
        root.display()
    );
}

fn is_empty_dir(p: &Path) -> bool {
    std::fs::read_dir(p)
        .map(|mut d| d.next().is_none())
        .unwrap_or(false)
}

// ---------- 审查第 1 轮回归（F-2）：仓库自带的符号链接不能把读写带出项目根 ----------

#[cfg(unix)]
mod symlinks {
    use super::*;
    use std::os::unix::fs::symlink;

    fn assert_refused<T: std::fmt::Debug>(r: Result<T, ProjectFilesError>) {
        match r {
            Err(ProjectFilesError::SymlinkRefused { .. }) => {}
            other => panic!("应拒绝符号链接，实际 {other:?}"),
        }
    }

    #[tokio::test]
    async fn gitignore_symlink_is_refused_and_outside_file_untouched() {
        let repo = tmp();
        let outside = tmp();
        let target = outside.path().join("victim");
        std::fs::write(&target, "original\n").unwrap();
        std::fs::create_dir(repo.path().join(".iwork")).unwrap();
        symlink(&target, repo.path().join(".iwork/.gitignore")).unwrap();

        assert_refused(
            FsProjectMarkerStore
                .write(repo.path(), id("01JC0000000000000000000004"))
                .await,
        );
        assert_eq!(std::fs::read_to_string(&target).unwrap(), "original\n");
        assert!(!repo.path().join(".iwork/project.toml").exists());
    }

    #[tokio::test]
    async fn marker_file_symlink_is_refused_for_read_and_write() {
        let repo = tmp();
        let outside = tmp();
        let target = outside.path().join("marker");
        std::fs::write(&target, "id = \"01JC0000000000000000000005\"\n").unwrap();
        std::fs::create_dir(repo.path().join(".iwork")).unwrap();
        symlink(&target, marker_path(repo.path())).unwrap();

        assert_refused(FsProjectMarkerStore.read(repo.path()).await);
        assert_refused(
            FsProjectMarkerStore
                .write(repo.path(), id("01JC0000000000000000000006"))
                .await,
        );
        assert_eq!(
            std::fs::read_to_string(&target).unwrap(),
            "id = \"01JC0000000000000000000005\"\n"
        );
    }

    #[tokio::test]
    async fn marker_dir_symlink_is_refused_and_nothing_is_written_outside() {
        let repo = tmp();
        let outside = tmp();
        symlink(outside.path(), repo.path().join(".iwork")).unwrap();
        std::fs::write(
            outside.path().join("project.toml"),
            "id = \"01JC0000000000000000000007\"\n",
        )
        .unwrap();

        assert_refused(FsProjectMarkerStore.read(repo.path()).await);
        assert_refused(
            FsProjectMarkerStore
                .write(repo.path(), id("01JC0000000000000000000008"))
                .await,
        );
        assert!(!outside.path().join(".gitignore").exists());
    }
}
