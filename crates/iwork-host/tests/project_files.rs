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
