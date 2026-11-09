//! 项目身份规则，端口用内存假实现（真实 git 与文件系统的端到端见 `iwork-daemon/tests/project_identity.rs`）。

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};

use iwork_core::project_identity::{
    AddTarget, NewProject, ProjectIdentityError, assign_id, check_path, relocate, resolve_add,
};
use iwork_domain::DomainError;
use iwork_domain::id::{ProjectId, RawUlid};
use iwork_domain::project::{PathStatus, Project, ProjectSettings, VcsKind};
use iwork_ports::clock::IdGenerator;
use iwork_ports::project_files::{ProjectFilesError, ProjectMarkerStore};
use iwork_ports::vcs::{
    BaselineRef, Detection, Diff, DiffRange, ProgressSink, RepoRoot, VcsCapabilities, VcsError,
    VcsProvider, WorkspaceStatus,
};
use pollster::block_on;

/// 路径 → (仓库根, 共享目录)；不在表里的已存在路径不受版本控制，`missing` 里的路径不存在。
#[derive(Default)]
struct FakeVcs {
    repos: HashMap<PathBuf, (PathBuf, PathBuf)>,
    plain: Vec<PathBuf>,
}

impl FakeVcs {
    fn repo(mut self, path: &str, root: &str, shared: &str) -> Self {
        self.repos.insert(path.into(), (root.into(), shared.into()));
        self
    }
    fn plain(mut self, path: &str) -> Self {
        self.plain.push(path.into());
        self
    }
}

fn unused(op: &'static str) -> VcsError {
    VcsError::Failed {
        operation: op,
        message: "测试里不该调用".into(),
    }
}

impl VcsProvider for FakeVcs {
    fn capabilities(&self) -> VcsCapabilities {
        VcsCapabilities {
            cheap_isolated_workspace: false,
        }
    }
    async fn detect(&self, path: &Path) -> Result<Detection, VcsError> {
        if let Some((root, shared)) = self.repos.get(path) {
            return Ok(Detection::Repository {
                root: RepoRoot(root.clone()),
                shared_dir: shared.clone(),
            });
        }
        if self.plain.iter().any(|p| p == path) {
            return Ok(Detection::NotVersionControlled);
        }
        Err(VcsError::PathMissing {
            path: path.to_owned(),
        })
    }
    async fn init(&self, _: &Path) -> Result<(), VcsError> {
        Err(unused("init"))
    }
    async fn clone_repository<S: ProgressSink + 'static>(
        &self,
        _: &str,
        _: &Path,
        _: S,
    ) -> Result<(), VcsError> {
        Err(unused("clone"))
    }
    async fn baseline(&self, _: &RepoRoot) -> Result<BaselineRef, VcsError> {
        Err(unused("baseline"))
    }
    async fn status(&self, _: &RepoRoot) -> Result<WorkspaceStatus, VcsError> {
        Err(unused("status"))
    }
    async fn diff(&self, _: &RepoRoot, _: DiffRange) -> Result<Diff, VcsError> {
        Err(unused("diff"))
    }
}

#[derive(Default)]
struct FakeMarkers(Mutex<HashMap<PathBuf, ProjectId>>);

impl FakeMarkers {
    fn with(self, root: &str, id: ProjectId) -> Self {
        self.0.lock().unwrap().insert(root.into(), id);
        self
    }
    fn get(&self, root: &str) -> Option<ProjectId> {
        self.0.lock().unwrap().get(Path::new(root)).copied()
    }
}

impl ProjectMarkerStore for FakeMarkers {
    async fn read(&self, root: &Path) -> Result<Option<ProjectId>, ProjectFilesError> {
        Ok(self.0.lock().unwrap().get(root).copied())
    }
    async fn write(&self, root: &Path, id: ProjectId) -> Result<(), ProjectFilesError> {
        let mut map = self.0.lock().unwrap();
        if map.contains_key(root) {
            return Err(ProjectFilesError::MarkerExists {
                path: root.to_owned(),
            });
        }
        map.insert(root.to_owned(), id);
        Ok(())
    }
}

struct CountingIds(AtomicU64);

impl IdGenerator for CountingIds {
    fn next(&self) -> RawUlid {
        RawUlid::from_u128(u128::from(self.0.fetch_add(1, Ordering::SeqCst)) + 1000)
    }
}

fn pid(n: u128) -> ProjectId {
    ProjectId::from_raw(RawUlid::from_u128(n))
}

fn project(n: u128, root: &str, status: PathStatus) -> Project {
    Project {
        id: pid(n),
        root_path: root.into(),
        vcs_kind: VcsKind("v".into()),
        name: "p".into(),
        pinned: false,
        last_active_at: None,
        path_status: status,
        settings: ProjectSettings::default(),
    }
}

// ---------- 添加 ----------

#[test]
fn plain_folder_is_not_version_controlled() {
    let vcs = FakeVcs::default().plain("/plain");
    assert_eq!(
        block_on(resolve_add(
            &vcs,
            &FakeMarkers::default(),
            &[],
            Path::new("/plain")
        )),
        Ok(AddTarget::NotVersionControlled)
    );
}

#[test]
fn any_path_into_a_registered_repo_jumps_to_that_project() {
    let vcs = FakeVcs::default()
        .repo("/repo", "/repo", "/repo/.git")
        .repo("/repo/sub", "/repo", "/repo/.git")
        .repo("/wt", "/wt", "/repo/.git")
        .repo("/other", "/other", "/other/.git");
    let known = [
        project(2, "/other", PathStatus::Ok),
        project(1, "/repo", PathStatus::Ok),
    ];
    for path in ["/repo", "/repo/sub", "/wt"] {
        assert_eq!(
            block_on(resolve_add(
                &vcs,
                &FakeMarkers::default(),
                &known,
                Path::new(path)
            )),
            Ok(AddTarget::Existing(pid(1))),
            "{path}"
        );
    }
}

#[test]
fn unregistered_repo_is_new_and_reuses_its_marker_id() {
    let vcs = FakeVcs::default().repo("/repo/sub", "/repo", "/repo/.git");
    let fresh = block_on(resolve_add(
        &vcs,
        &FakeMarkers::default(),
        &[],
        Path::new("/repo/sub"),
    ));
    assert_eq!(
        fresh,
        Ok(AddTarget::New(NewProject {
            root: RepoRoot("/repo".into()),
            existing_id: None,
        })),
        "从子目录添加时归到仓库根"
    );

    // 移除后重新添加：仓库里的标记还在，ID 沿用，历史随之恢复
    let markers = FakeMarkers::default().with("/repo", pid(7));
    assert_eq!(
        block_on(resolve_add(&vcs, &markers, &[], Path::new("/repo/sub"))),
        Ok(AddTarget::New(NewProject {
            root: RepoRoot("/repo".into()),
            existing_id: Some(pid(7)),
        }))
    );
}

#[test]
fn marker_id_registered_at_another_location_is_a_conflict() {
    // 提交了 .iwork/ 的仓库另克隆一份：标记相同，共享目录不同
    let vcs = FakeVcs::default()
        .repo("/a", "/a", "/a/.git")
        .repo("/copy", "/copy", "/copy/.git");
    let markers = FakeMarkers::default()
        .with("/a", pid(1))
        .with("/copy", pid(1));
    let known = [project(1, "/a", PathStatus::Ok)];

    assert_eq!(
        block_on(resolve_add(&vcs, &markers, &known, Path::new("/copy"))),
        Err(ProjectIdentityError::IdConflict {
            id: pid(1),
            root: "/copy".into(),
            registered_path: "/a".into(),
            registered_status: PathStatus::Ok,
        })
    );
    assert_eq!(markers.get("/copy"), Some(pid(1)), "不改写标记");
}

#[test]
fn moved_repo_added_as_new_path_is_a_conflict_not_a_silent_relocation() {
    let vcs = FakeVcs::default().repo("/moved", "/moved", "/moved/.git");
    let markers = FakeMarkers::default().with("/moved", pid(1));
    let known = [project(1, "/old", PathStatus::Missing)];

    match block_on(resolve_add(&vcs, &markers, &known, Path::new("/moved"))) {
        Err(ProjectIdentityError::IdConflict {
            registered_status, ..
        }) => assert_eq!(registered_status, PathStatus::Missing),
        other => panic!("应报冲突，实际 {other:?}"),
    }
}

#[test]
fn registered_project_with_missing_path_does_not_block_adding_others() {
    let vcs = FakeVcs::default().repo("/new", "/new", "/new/.git");
    let known = [project(1, "/gone", PathStatus::Missing)];
    assert!(matches!(
        block_on(resolve_add(
            &vcs,
            &FakeMarkers::default(),
            &known,
            Path::new("/new")
        )),
        Ok(AddTarget::New(_))
    ));
}

#[test]
fn assign_id_writes_a_fresh_marker_or_keeps_the_existing_one() {
    let markers = FakeMarkers::default();
    let ids = CountingIds(AtomicU64::new(0));
    let new = NewProject {
        root: RepoRoot("/r".into()),
        existing_id: None,
    };
    let id = block_on(assign_id(&markers, &ids, &new)).unwrap();
    assert_eq!(markers.get("/r"), Some(id));

    let again = NewProject {
        root: RepoRoot("/s".into()),
        existing_id: Some(pid(5)),
    };
    assert_eq!(block_on(assign_id(&markers, &ids, &again)), Ok(pid(5)));
    assert_eq!(markers.get("/s"), None, "已有标记时不再写");
}

// ---------- 路径检查 ----------

#[test]
fn check_path_marks_lost_and_found() {
    let markers = FakeMarkers::default().with("/r", pid(1));
    let present = FakeVcs::default().repo("/r", "/r", "/r/.git");
    let absent = FakeVcs::default();

    let ok = project(1, "/r", PathStatus::Ok);
    let missing = project(1, "/r", PathStatus::Missing);
    assert_eq!(
        block_on(check_path(&absent, &markers, &ok)),
        Ok(Some(PathStatus::Missing))
    );
    assert_eq!(block_on(check_path(&present, &markers, &ok)), Ok(None));
    assert_eq!(block_on(check_path(&absent, &markers, &missing)), Ok(None));
    assert_eq!(
        block_on(check_path(&present, &markers, &missing)),
        Ok(Some(PathStatus::Ok))
    );

    // 原路径上出现的是别的仓库：仍然找不到
    let other = FakeMarkers::default().with("/r", pid(2));
    assert_eq!(block_on(check_path(&present, &other, &missing)), Ok(None));
}

// ---------- 重新定位 ----------

#[test]
fn relocate_links_only_when_marker_id_matches() {
    let vcs = FakeVcs::default()
        .repo("/new/sub", "/new", "/new/.git")
        .repo("/foreign", "/foreign", "/foreign/.git")
        .repo("/unmarked", "/unmarked", "/unmarked/.git")
        .plain("/plain");
    let markers = FakeMarkers::default()
        .with("/new", pid(1))
        .with("/foreign", pid(9));
    let before = project(1, "/old", PathStatus::Missing);

    let mut p = before.clone();
    assert_eq!(
        block_on(relocate(&vcs, &markers, &mut p, Path::new("/foreign"))),
        Err(ProjectIdentityError::Domain(
            DomainError::ProjectIdMismatch {
                expected: pid(1),
                found: pid(9),
            }
        ))
    );
    assert_eq!(
        block_on(relocate(&vcs, &markers, &mut p, Path::new("/unmarked"))),
        Err(ProjectIdentityError::MarkerMissing {
            root: "/unmarked".into()
        })
    );
    assert_eq!(
        block_on(relocate(&vcs, &markers, &mut p, Path::new("/plain"))),
        Err(ProjectIdentityError::Vcs(VcsError::NotVersionControlled {
            path: "/plain".into()
        }))
    );
    assert_eq!(p, before, "失败时项目不变");

    block_on(relocate(&vcs, &markers, &mut p, Path::new("/new/sub"))).expect("relocate");
    assert_eq!(p.root_path, PathBuf::from("/new"), "关联到仓库根");
    assert_eq!(p.path_status, PathStatus::Ok);
}
