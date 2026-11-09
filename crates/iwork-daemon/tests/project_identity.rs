//! 项目身份端到端：真实 git 仓库 + 真实文件系统上的项目标记（T08 验收 2、3、4、4a）。
//!
//! 组合方式与后台服务一致：`GitVcs` + `FsProjectMarkerStore` + `FsProjectDirectories`，规则来自
//! `iwork-core::project_identity`。git 子进程隔离用户配置（`HOME` 与全局配置都指到临时目录）。

use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicU64, Ordering};

use iwork_core::project_identity::{
    AddTarget, ProjectIdentityError, assign_id, check_path, relocate, resolve_add,
};
use iwork_domain::DomainError;
use iwork_domain::id::RawUlid;
use iwork_domain::project::{PathStatus, Project, ProjectSettings, VcsKind};
use iwork_host::project_files::{FsProjectDirectories, FsProjectMarkerStore};
use iwork_ports::clock::IdGenerator;
use iwork_ports::project_files::{ProjectDirectories, ProjectFilesError, ProjectMarkerStore};
use iwork_ports::vcs::{Detection, VcsProvider};
use iwork_vcs_git::{GitCommand, GitVcs};

struct Env {
    tmp: tempfile::TempDir,
    ids: Ids,
}

struct Ids(AtomicU64);

impl IdGenerator for Ids {
    fn next(&self) -> RawUlid {
        RawUlid::from_u128(u128::from(self.0.fetch_add(1, Ordering::SeqCst)) + 1)
    }
}

impl Env {
    fn new() -> Self {
        let tmp = tempfile::Builder::new()
            .prefix("iwork-identity-")
            .tempdir()
            .unwrap();
        let env = Self {
            tmp,
            ids: Ids(AtomicU64::new(0)),
        };
        std::fs::create_dir_all(env.home()).unwrap();
        std::fs::write(
            env.home().join(".gitconfig"),
            "[user]\n\tname = T\n\temail = t@example.invalid\n",
        )
        .unwrap();
        env
    }

    /// 真实路径（macOS 的 /var → /private/var）。
    fn root(&self) -> PathBuf {
        std::fs::canonicalize(self.tmp.path()).unwrap()
    }

    fn home(&self) -> PathBuf {
        self.tmp.path().join("home")
    }

    fn envs(&self) -> [(&'static str, PathBuf); 3] {
        [
            ("HOME", self.home()),
            ("GIT_CONFIG_GLOBAL", self.home().join(".gitconfig")),
            ("GIT_CONFIG_NOSYSTEM", "1".into()),
        ]
    }

    fn vcs(&self) -> GitVcs {
        GitVcs::with_command(
            self.envs()
                .into_iter()
                .fold(GitCommand::default(), |c, (k, v)| c.env(k, v)),
        )
    }

    fn git(&self, cwd: &Path, args: &[&str]) -> String {
        let out = Command::new("git")
            .current_dir(cwd)
            .envs(self.envs())
            .args(args)
            .output()
            .unwrap();
        assert!(
            out.status.success(),
            "git {args:?}: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        String::from_utf8_lossy(&out.stdout).trim().to_owned()
    }

    fn repo(&self, name: &str) -> PathBuf {
        let dir = self.root().join(name);
        std::fs::create_dir_all(dir.join("src")).unwrap();
        std::fs::write(dir.join("src/lib.rs"), "x\n").unwrap();
        self.git(&dir, &["init", "--quiet"]);
        self.git(&dir, &["add", "--all"]);
        self.git(&dir, &["commit", "--quiet", "-m", "c1"]);
        dir
    }

    /// 走一遍添加：解析 → 新项目就确定 ID（写标记）→ 登记成项目实体。返回登记的项目。
    async fn add(&self, known: &[Project], path: &Path) -> Project {
        let new = match resolve_add(&self.vcs(), &FsProjectMarkerStore, known, path).await {
            Ok(AddTarget::New(new)) => new,
            other => panic!("{} 应是新项目，实际 {other:?}", path.display()),
        };
        let id = assign_id(&FsProjectMarkerStore, &self.ids, &new)
            .await
            .unwrap();
        Project {
            id,
            root_path: new.root.0,
            vcs_kind: VcsKind("git".into()),
            name: "p".into(),
            pinned: false,
            last_active_at: None,
            path_status: PathStatus::Ok,
            settings: ProjectSettings::default(),
        }
    }
}

// ---------- 验收 2：同一仓库换个路径添加 → 已有项目 ----------

#[tokio::test]
async fn same_repo_via_subdir_worktree_and_symlink_is_the_existing_project() {
    let env = Env::new();
    let repo = env.repo("app");
    let p = env.add(&[], &repo).await;
    assert_eq!(
        FsProjectMarkerStore.read(&repo).await,
        Ok(Some(p.id)),
        "首次添加写入标记"
    );
    let known = [p.clone()];

    let wt = env.root().join("app-wt");
    env.git(
        &repo,
        &[
            "worktree",
            "add",
            "--quiet",
            wt.to_str().unwrap(),
            "-b",
            "side",
        ],
    );
    let mut aliases = vec![repo.join("src"), wt];
    #[cfg(unix)]
    {
        let link = env.root().join("app-link");
        std::os::unix::fs::symlink(&repo, &link).unwrap();
        aliases.push(link);
    }
    for path in aliases {
        assert_eq!(
            resolve_add(&env.vcs(), &FsProjectMarkerStore, &known, &path).await,
            Ok(AddTarget::Existing(p.id)),
            "{}",
            path.display()
        );
    }
}

/// macOS 默认的盘大小写不敏感，不同大小写是同一个目录，要认成已有项目。
/// Linux 上不同大小写是两个目录，本来就不是同一仓库，不测。
#[cfg(target_os = "macos")]
#[tokio::test]
async fn same_repo_via_different_case_is_the_existing_project() {
    let env = Env::new();
    let repo = env.repo("CaseApp");
    let p = env.add(&[], &repo).await;
    let other_case = env.root().join("caseapp");
    assert!(other_case.exists(), "测试前提：大小写不敏感的盘");

    assert_eq!(
        resolve_add(
            &env.vcs(),
            &FsProjectMarkerStore,
            std::slice::from_ref(&p),
            &other_case
        )
        .await,
        Ok(AddTarget::Existing(p.id))
    );
}

// ---------- 验收 3a：项目 ID 不进版本控制 ----------

#[tokio::test]
async fn added_project_marker_is_ignored_by_git_and_existing_ignore_rules_are_kept() {
    let env = Env::new();
    let repo = env.repo("quiet");
    std::fs::create_dir_all(repo.join(".iwork")).unwrap();
    std::fs::write(repo.join(".iwork/.gitignore"), "scratch/\n").unwrap();
    std::fs::write(repo.join(".iwork/shared.md"), "随仓库共享的内容\n").unwrap();
    env.git(&repo, &["add", "--all"]);
    env.git(&repo, &["commit", "--quiet", "-m", "shared iwork content"]);

    env.add(&[], &repo).await;

    let status = env.git(&repo, &["status", "--porcelain", "--untracked-files=all"]);
    assert!(
        !status.contains("project.toml"),
        "git status 看不到项目标记：{status:?}"
    );
    assert_eq!(status, "M .iwork/.gitignore", "只多出忽略规则的改动");
    assert_eq!(
        std::fs::read_to_string(repo.join(".iwork/.gitignore")).unwrap(),
        "scratch/\n/project.toml\n"
    );
    // 对照：连被忽略的文件一起列时它在
    let ignored = env.git(
        &repo,
        &[
            "status",
            "--porcelain",
            "--ignored",
            "--untracked-files=all",
        ],
    );
    assert!(ignored.contains(".iwork/project.toml"), "{ignored:?}");
}

#[tokio::test]
async fn fresh_repo_gets_ignore_file_and_marker_stays_out_of_git_status() {
    let env = Env::new();
    let repo = env.repo("fresh");
    env.add(&[], &repo).await;
    let status = env.git(&repo, &["status", "--porcelain", "--untracked-files=all"]);
    assert_eq!(status, "?? .iwork/.gitignore", "{status:?}");
}

// ---------- 审查第 1 轮回归（F-3、F-4） ----------

/// F-3：项目 A 搬走后在原路径新建仓库 B：A 标为找不到路径，B 不能接上 A 的身份。
#[tokio::test]
async fn new_repo_at_a_moved_projects_old_path_is_not_that_project() {
    let env = Env::new();
    let repo = env.repo("spot");
    let a = env.add(&[], &repo).await;
    std::fs::rename(&repo, env.root().join("spot-moved")).unwrap();
    let b = env.repo("spot");
    assert_eq!(b, a.root_path, "前提：B 就在 A 的原路径");

    assert_eq!(
        check_path(&env.vcs(), &FsProjectMarkerStore, &a).await,
        Ok(Some(PathStatus::Missing))
    );
    match resolve_add(
        &env.vcs(),
        &FsProjectMarkerStore,
        std::slice::from_ref(&a),
        &b,
    )
    .await
    {
        Ok(AddTarget::New(new)) => assert_eq!(new.existing_id, None),
        other => panic!("B 应是新项目，实际 {other:?}"),
    }
}

/// F-3：仓库失去版本控制（.git 被移走）算找不到路径；放回去后恢复。
#[tokio::test]
async fn project_that_lost_version_control_is_missing_until_restored() {
    let env = Env::new();
    let repo = env.repo("vanishing-git");
    let mut p = env.add(&[], &repo).await;
    let stash = env.root().join("stashed-git");
    std::fs::rename(repo.join(".git"), &stash).unwrap();

    assert_eq!(
        check_path(&env.vcs(), &FsProjectMarkerStore, &p).await,
        Ok(Some(PathStatus::Missing))
    );
    p.path_status = PathStatus::Missing;
    assert_eq!(
        check_path(&env.vcs(), &FsProjectMarkerStore, &p).await,
        Ok(None)
    );

    std::fs::rename(&stash, repo.join(".git")).unwrap();
    assert_eq!(
        check_path(&env.vcs(), &FsProjectMarkerStore, &p).await,
        Ok(Some(PathStatus::Ok))
    );
}

/// F-4：仓库自带的忽略规则里有后置否定规则或带前导空格的写法时，添加后标记仍被 git 忽略。
#[tokio::test]
async fn marker_stays_ignored_despite_tricky_existing_ignore_rules() {
    for (name, rules) in [
        ("negated", "/project.toml\n!*.toml\n"),
        ("spaced", " /project.toml\n"),
    ] {
        let env = Env::new();
        let repo = env.repo(name);
        std::fs::create_dir_all(repo.join(".iwork")).unwrap();
        std::fs::write(repo.join(".iwork/.gitignore"), rules).unwrap();
        env.git(&repo, &["add", "--all"]);
        env.git(&repo, &["commit", "--quiet", "-m", "tricky rules"]);

        env.add(&[], &repo).await;

        // check-ignore 退出码 0 才表示被忽略（env.git 要求成功）
        env.git(&repo, &["check-ignore", "--quiet", ".iwork/project.toml"]);
        let status = env.git(&repo, &["status", "--porcelain", "--untracked-files=all"]);
        assert!(!status.contains("project.toml"), "{name}: {status:?}");
        assert!(
            std::fs::read_to_string(repo.join(".iwork/.gitignore"))
                .unwrap()
                .starts_with(rules),
            "{name}: 原内容保留"
        );
    }
}

// ---------- 验收 3：移动后找不到路径；重新定位按项目 ID ----------

#[tokio::test]
async fn moved_repo_is_missing_and_relocates_only_to_the_same_project_id() {
    let env = Env::new();
    let repo = env.repo("mover");
    let mut p = env.add(&[], &repo).await;
    let stranger_repo = env.repo("stranger");
    let stranger = env.add(std::slice::from_ref(&p), &stranger_repo).await;

    let moved = env.root().join("moved-here");
    std::fs::rename(&repo, &moved).unwrap();
    let status = check_path(&env.vcs(), &FsProjectMarkerStore, &p)
        .await
        .unwrap();
    assert_eq!(status, Some(PathStatus::Missing), "移动后标为找不到路径");
    p.path_status = PathStatus::Missing;

    let before = p.clone();
    assert_eq!(
        relocate(&env.vcs(), &FsProjectMarkerStore, &mut p, &stranger_repo).await,
        Err(ProjectIdentityError::Domain(
            DomainError::ProjectIdMismatch {
                expected: p.id,
                found: stranger.id,
            }
        )),
        "ID 不一致拒绝并说明"
    );
    assert_eq!(p, before);

    relocate(
        &env.vcs(),
        &FsProjectMarkerStore,
        &mut p,
        &moved.join("src"),
    )
    .await
    .unwrap();
    assert_eq!(
        (p.root_path.clone(), p.path_status),
        (moved, PathStatus::Ok)
    );
}

#[tokio::test]
async fn copy_of_a_repo_with_a_committed_marker_is_refused_as_id_conflict() {
    let env = Env::new();
    let repo = env.repo("shared");
    let p = env.add(&[], &repo).await;
    // 标记默认被忽略；这里模拟用户强制提交了它
    env.git(&repo, &["add", "--force", ".iwork/project.toml"]);
    env.git(&repo, &["commit", "--quiet", "-m", "track iwork marker"]);

    let copy = env.root().join("shared-copy");
    env.git(
        &env.root(),
        &[
            "clone",
            "--quiet",
            repo.to_str().unwrap(),
            copy.to_str().unwrap(),
        ],
    );
    assert_eq!(
        FsProjectMarkerStore.read(&copy).await,
        Ok(Some(p.id)),
        "前提：副本带着同一个 ID"
    );

    assert_eq!(
        resolve_add(
            &env.vcs(),
            &FsProjectMarkerStore,
            std::slice::from_ref(&p),
            &copy
        )
        .await,
        Err(ProjectIdentityError::IdConflict {
            id: p.id,
            root: copy.clone(),
            registered_path: repo,
            registered_status: PathStatus::Ok,
        })
    );
    assert_eq!(
        FsProjectMarkerStore.read(&copy).await,
        Ok(Some(p.id)),
        "不改写副本的标记"
    );
}

// ---------- 验收 4、4a：初始化与新建 ----------

#[tokio::test]
async fn plain_folder_needs_init_then_can_be_added_with_a_first_commit() {
    let env = Env::new();
    let dir = env.root().join("plain");
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("notes.md"), "hi\n").unwrap();

    assert_eq!(
        resolve_add(&env.vcs(), &FsProjectMarkerStore, &[], &dir).await,
        Ok(AddTarget::NotVersionControlled)
    );
    env.vcs().init(&dir).await.unwrap();
    let p = env.add(&[], &dir).await;
    assert_eq!(p.root_path, dir);
    assert_eq!(env.git(&dir, &["log", "--format=%s"]), "Initial commit");
    assert_eq!(
        env.git(&dir, &["ls-tree", "-r", "--name-only", "HEAD"]),
        "notes.md"
    );
}

#[tokio::test]
async fn new_project_under_missing_default_root_gets_an_empty_first_commit() {
    let env = Env::new();
    let projects_root = env.home().join("iWork/projects");
    assert!(!projects_root.exists());

    let dir = FsProjectDirectories
        .create_project_dir(&projects_root, "demo")
        .await
        .unwrap();
    env.vcs().init(&dir).await.unwrap();
    assert!(matches!(
        env.vcs().detect(&dir).await,
        Ok(Detection::Repository { .. })
    ));
    assert_eq!(env.git(&dir, &["rev-list", "--count", "HEAD"]), "1");
    assert_eq!(
        env.git(&dir, &["ls-tree", "-r", "--name-only", "HEAD"]),
        "",
        "空提交"
    );
    let p = env.add(&[], &dir).await;
    assert_eq!(FsProjectMarkerStore.read(&dir).await, Ok(Some(p.id)));

    // 同名目录已有内容：拒绝
    assert_eq!(
        FsProjectDirectories
            .create_project_dir(&projects_root, "demo")
            .await,
        Err(ProjectFilesError::TargetNotEmpty { path: dir })
    );
}
