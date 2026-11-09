//! `GitVcs` 的行为测试：临时目录里的真实 git 仓库（T08 验收 1、2、4、4a、5 的版本控制部分）。
//!
//! 克隆只用本地 `file://` 仓库和连不上的本机地址，不访问外网。

mod common;

use std::path::Path;

use common::{Fixture, Recorder, write};
use iwork_ports::vcs::{
    BaselineRef, ChangeKind, ChangedPath, Detection, DiffRange, RepoRoot, VcsError, VcsProvider,
};

fn repository(d: Detection) -> (std::path::PathBuf, std::path::PathBuf) {
    match d {
        Detection::Repository { root, shared_dir } => (root.0, shared_dir),
        other => panic!("应识别为仓库，实际 {other:?}"),
    }
}

// ---------- 验收 1：detect ----------

#[tokio::test]
async fn detect_plain_repo_and_subdirectory_resolve_to_repo_root() {
    let f = Fixture::new();
    let repo = f.repo_with_commit("plain", &[("src/lib.rs", "x\n")]);
    let vcs = f.vcs();

    let (root, shared) = repository(vcs.detect(&repo).await.expect("detect root"));
    assert_eq!(root, repo);
    assert_eq!(shared, repo.join(".git"));

    let (sub_root, sub_shared) =
        repository(vcs.detect(&repo.join("src")).await.expect("detect subdir"));
    assert_eq!(sub_root, repo, "子目录归到仓库根");
    assert_eq!(sub_shared, shared);
}

#[tokio::test]
async fn detect_linked_worktree_shares_the_main_repos_common_dir() {
    let f = Fixture::new();
    let repo = f.repo_with_commit("main", &[("a", "1\n")]);
    let wt = f.root().join("linked");
    f.git(
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

    let (root, shared) = repository(f.vcs().detect(&wt).await.expect("detect worktree"));
    assert_eq!(root, wt, "链接工作树的根是它自己");
    assert_eq!(shared, repo.join(".git"), "共享目录是主仓库的 .git");
}

#[tokio::test]
async fn detect_submodule_is_its_own_repository() {
    let f = Fixture::new();
    let lib = f.repo_with_commit("lib", &[("l", "1\n")]);
    let sup = f.repo_with_commit("super", &[("s", "1\n")]);
    let url = format!("file://{}", lib.display());
    f.git(
        &sup,
        &[
            "-c",
            "protocol.file.allow=always",
            "submodule",
            "--quiet",
            "add",
            &url,
            "deps/lib",
        ],
    );

    let sub = sup.join("deps/lib");
    let (root, shared) = repository(f.vcs().detect(&sub).await.expect("detect submodule"));
    assert_eq!(root, sub);
    assert_eq!(shared, sup.join(".git/modules/deps/lib"));
    // 对照：超级项目自己仍是自己
    let (sup_root, _) = repository(f.vcs().detect(&sup).await.expect("detect super"));
    assert_eq!(sup_root, sup);
}

#[tokio::test]
async fn detect_bare_repository_is_rejected_with_reason() {
    let f = Fixture::new();
    let bare = f.root().join("bare.git");
    std::fs::create_dir_all(&bare).unwrap();
    f.git(&bare, &["init", "--quiet", "--bare"]);

    match f.vcs().detect(&bare).await {
        Err(VcsError::Failed { operation, message }) => {
            assert_eq!(operation, "detect");
            assert!(message.contains("裸仓库"), "{message}");
        }
        other => panic!("裸仓库应报错，实际 {other:?}"),
    }
}

#[tokio::test]
async fn detect_plain_folder_and_missing_path() {
    let f = Fixture::new();
    let plain = f.root().join("plain-folder");
    std::fs::create_dir_all(&plain).unwrap();
    assert_eq!(
        f.vcs().detect(&plain).await.unwrap(),
        Detection::NotVersionControlled
    );
    let missing = f.root().join("nope");
    assert_eq!(
        f.vcs().detect(&missing).await,
        Err(VcsError::PathMissing { path: missing })
    );
}

// ---------- 验收 2（版本控制部分）：不同写法的路径得到同一个共享目录 ----------

#[cfg(unix)]
#[tokio::test]
async fn detect_through_symlink_yields_real_paths() {
    let f = Fixture::new();
    let repo = f.repo_with_commit("real", &[("a", "1\n")]);
    let link = f.root().join("alias");
    std::os::unix::fs::symlink(&repo, &link).unwrap();

    let direct = f.vcs().detect(&repo).await.unwrap();
    let via_link = f.vcs().detect(&link).await.unwrap();
    assert_eq!(via_link, direct);
}

/// 大小写不敏感的盘（macOS 默认 APFS）上，不同大小写的路径解析成磁盘上的真实写法。
/// Linux 上不同大小写本来就是两个目录，不测。
#[cfg(target_os = "macos")]
#[tokio::test]
async fn detect_with_different_case_yields_on_disk_spelling() {
    let f = Fixture::new();
    let repo = f.repo_with_commit("CaseRepo", &[("a", "1\n")]);
    let lower = f.root().join("caserepo");
    assert!(lower.exists(), "测试前提：临时目录所在的盘大小写不敏感");

    let (root, shared) = repository(f.vcs().detect(&lower).await.unwrap());
    assert_eq!(root, repo, "根换成磁盘上的写法");
    assert_eq!(shared, repo.join(".git"));
}

// ---------- 验收 4、4a：init ----------

#[tokio::test]
async fn init_existing_folder_commits_its_files() {
    let f = Fixture::new();
    f.set_identity();
    let dir = f.root().join("existing");
    write(&dir.join("README.md"), "hi\n");
    write(&dir.join("src/main.rs"), "fn main() {}\n");

    f.vcs().init(&dir).await.expect("init");

    assert!(matches!(
        f.vcs().detect(&dir).await.unwrap(),
        Detection::Repository { .. }
    ));
    let files = f.git(&dir, &["ls-tree", "-r", "--name-only", "HEAD"]);
    assert_eq!(files, "README.md\nsrc/main.rs");
    assert_eq!(
        f.git(&dir, &["status", "--porcelain"]),
        "",
        "首次提交后工作区干净"
    );
}

#[tokio::test]
async fn init_empty_folder_makes_an_empty_first_commit() {
    let f = Fixture::new();
    f.set_identity();
    let dir = f.root().join("empty");
    std::fs::create_dir_all(&dir).unwrap();

    f.vcs().init(&dir).await.expect("init");

    assert_eq!(f.git(&dir, &["rev-list", "--count", "HEAD"]), "1");
    assert_eq!(f.git(&dir, &["ls-tree", "-r", "--name-only", "HEAD"]), "");
}

#[tokio::test]
async fn init_without_identity_reports_it_and_leaves_folder_untouched() {
    let f = Fixture::new();
    let dir = f.root().join("anon");
    write(&dir.join("a.txt"), "a\n");

    assert_eq!(f.vcs().init(&dir).await, Err(VcsError::IdentityMissing));
    assert!(!dir.join(".git").exists(), "失败后不留下半初始化的仓库");
    assert_eq!(
        f.vcs().detect(&dir).await.unwrap(),
        Detection::NotVersionControlled
    );

    // 配好身份后直接重试成功
    f.set_identity();
    f.vcs().init(&dir).await.expect("retry init");
    assert_eq!(f.git(&dir, &["rev-list", "--count", "HEAD"]), "1");
}

#[tokio::test]
async fn init_refuses_folder_already_inside_a_repository() {
    let f = Fixture::new();
    f.set_identity();
    let repo = f.repo_with_commit("has-repo", &[("sub/x", "1\n")]);
    write(&repo.join("sub/uncommitted"), "keep me out\n");

    for target in [repo.clone(), repo.join("sub")] {
        match f.vcs().init(&target).await {
            Err(VcsError::Failed {
                operation: "init",
                message,
            }) => {
                assert!(message.contains("已在仓库"), "{message}")
            }
            other => panic!("{} 应被拒绝，实际 {other:?}", target.display()),
        }
    }
    assert_eq!(
        f.git(&repo, &["rev-list", "--count", "HEAD"]),
        "1",
        "没有多出提交"
    );
    assert!(!repo.join("sub/.git").exists());
}

#[tokio::test]
async fn init_missing_folder_is_path_missing() {
    let f = Fixture::new();
    let missing = f.root().join("not-created");
    assert_eq!(
        f.vcs().init(&missing).await,
        Err(VcsError::PathMissing { path: missing })
    );
}

// ---------- 验收 5：clone ----------

/// 一个有几百个不易压缩文件的源仓库，克隆时 git 会输出多轮进度。
fn source_repo(f: &Fixture) -> std::path::PathBuf {
    let dir = f.repo_with_commit("source", &[]);
    let mut seed: u64 = 0x2545_F491_4F6C_DD1D;
    for i in 0..300 {
        let mut content = String::with_capacity(4096);
        for _ in 0..256 {
            seed ^= seed << 13;
            seed ^= seed >> 7;
            seed ^= seed << 17;
            content.push_str(&format!("{seed:016x}"));
        }
        write(&dir.join(format!("f{i:03}.txt")), &content);
    }
    f.commit_all(&dir, "many files");
    dir
}

fn file_url(path: &Path) -> String {
    format!("file://{}", path.display())
}

#[tokio::test]
async fn clone_reports_strictly_increasing_progress_ending_at_100() {
    let f = Fixture::new();
    let src = source_repo(&f);
    let dest = f.root().join("projects/cloned");
    let rec = Recorder::default();

    f.vcs()
        .clone_repository(&file_url(&src), &dest, rec.clone())
        .await
        .expect("clone");

    let percents = rec.percents();
    assert!(percents.len() >= 3, "应有多条进度：{percents:?}");
    assert!(percents.windows(2).all(|w| w[0] < w[1]), "{percents:?}");
    assert_eq!(percents.last(), Some(&100));
    assert_eq!(f.git(&dest, &["rev-list", "--count", "HEAD"]), "2");
}

#[tokio::test]
async fn clone_bad_address_fails_retryably_then_retry_succeeds() {
    let f = Fixture::new();
    let src = source_repo(&f);
    let dest = f.root().join("projects/retry");

    let err = f
        .vcs()
        .clone_repository(
            &file_url(&f.root().join("no-such-repo")),
            &dest,
            Recorder::default(),
        )
        .await
        .expect_err("bad address");
    match &err {
        VcsError::CloneFailed { message } => {
            assert!(
                message.contains("does not appear to be a git repository"),
                "{message}"
            )
        }
        other => panic!("应为 CloneFailed，实际 {other:?}"),
    }
    assert!(err.retryable());

    f.vcs()
        .clone_repository(&file_url(&src), &dest, Recorder::default())
        .await
        .expect("retry into the same destination");
    assert!(dest.join("f000.txt").exists());
}

#[tokio::test]
async fn clone_error_never_contains_credentials_from_the_address() {
    let f = Fixture::new();
    // 本机 1 号端口没有服务，连接立即被拒，不出本机
    let url = "http://alice:s3cr3t-token@127.0.0.1:1/repo.git";
    let err = f
        .vcs()
        .clone_repository(url, &f.root().join("cred"), Recorder::default())
        .await
        .expect_err("connection refused");
    let VcsError::CloneFailed { message } = &err else {
        panic!("应为 CloneFailed，实际 {err:?}");
    };
    assert!(!message.contains("s3cr3t-token"), "{message}");
    assert!(!message.contains("alice"), "{message}");
    assert!(!message.is_empty());
    assert!(!err.to_string().contains("s3cr3t-token"));
}

/// 模拟中途断网：服务端打包到一半就断开。
///
/// 用 `uploadpack.packObjectsHook` 截断打包输出。这个配置 git 只认全局或系统配置（本地传输会清掉
/// `GIT_CONFIG_COUNT` 这类环境变量），所以写进夹具的临时“全局”配置，重试前再删掉。
#[cfg(unix)]
#[tokio::test]
async fn clone_interrupted_midway_fails_retryably_then_retry_succeeds() {
    use std::os::unix::fs::PermissionsExt;

    let f = Fixture::new();
    let src = source_repo(&f);
    let hook = f.root().join("truncate-pack.sh");
    std::fs::write(&hook, "#!/bin/sh\n\"$@\" | head -c 20000\nexit 1\n").unwrap();
    std::fs::set_permissions(&hook, std::fs::Permissions::from_mode(0o755)).unwrap();
    let hook_key = "uploadpack.packObjectsHook";
    f.git(
        &f.root(),
        &["config", "--global", hook_key, hook.to_str().unwrap()],
    );
    let dest = f.root().join("projects/flaky");
    let rec = Recorder::default();

    let err = f
        .vcs()
        .clone_repository(&file_url(&src), &dest, rec.clone())
        .await
        .expect_err("truncated transfer");
    assert!(matches!(err, VcsError::CloneFailed { .. }), "{err:?}");
    assert!(err.retryable());
    let percents = rec.percents();
    assert!(!percents.is_empty(), "断开前已经有进度，确实是中途断开");
    assert!(
        !percents.contains(&100),
        "失败的克隆不会报 100%：{percents:?}"
    );
    assert!(
        !dest.exists() || std::fs::read_dir(&dest).unwrap().next().is_none(),
        "失败后目标目录不留残骸"
    );

    f.git(&f.root(), &["config", "--global", "--unset", hook_key]);
    f.vcs()
        .clone_repository(&file_url(&src), &dest, Recorder::default())
        .await
        .expect("retry after network recovers");
    assert_eq!(f.git(&dest, &["rev-list", "--count", "HEAD"]), "2");
}

#[tokio::test]
async fn clone_into_non_empty_target_is_refused_without_touching_it() {
    let f = Fixture::new();
    let src = source_repo(&f);
    let dest = f.root().join("occupied");
    write(&dest.join("mine.txt"), "keep\n");

    assert_eq!(
        f.vcs()
            .clone_repository(&file_url(&src), &dest, Recorder::default())
            .await,
        Err(VcsError::TargetNotEmpty { path: dest.clone() })
    );
    assert_eq!(
        std::fs::read_to_string(dest.join("mine.txt")).unwrap(),
        "keep\n"
    );
}

// ---------- baseline / status / diff ----------

#[tokio::test]
async fn baseline_is_head_commit_and_status_lists_each_change_once() {
    let f = Fixture::new();
    let repo = f.repo_with_commit(
        "work",
        &[
            ("keep.txt", "k\n"),
            ("edit.txt", "1\n"),
            ("gone.txt", "g\n"),
            ("staged-del.txt", "s\n"),
        ],
    );
    let vcs = f.vcs();
    let root = RepoRoot(repo.clone());
    let head = f.git(&repo, &["rev-parse", "HEAD"]);
    assert_eq!(vcs.baseline(&root).await.unwrap(), BaselineRef(head));

    write(&repo.join("edit.txt"), "2\n");
    std::fs::remove_file(repo.join("gone.txt")).unwrap();
    write(&repo.join("new-untracked.txt"), "u\n");
    write(&repo.join("new-staged.txt"), "n\n");
    f.git(&repo, &["add", "new-staged.txt"]);
    f.git(&repo, &["rm", "--quiet", "staged-del.txt"]);
    write(&repo.join(".gitignore"), "ignored.log\n");
    write(&repo.join("ignored.log"), "noise\n");

    let mut changed = vcs.status(&root).await.unwrap().changed;
    changed.sort_by(|a, b| a.path.cmp(&b.path));
    let expect = |p: &str, change| ChangedPath {
        path: p.into(),
        change,
    };
    assert_eq!(
        changed,
        vec![
            expect(".gitignore", ChangeKind::Untracked),
            expect("edit.txt", ChangeKind::Modified),
            expect("gone.txt", ChangeKind::Deleted),
            expect("new-staged.txt", ChangeKind::Added),
            expect("new-untracked.txt", ChangeKind::Untracked),
            expect("staged-del.txt", ChangeKind::Deleted),
        ]
    );
}

#[tokio::test]
async fn baseline_of_repo_without_commits_is_the_empty_tree() {
    let f = Fixture::new();
    let dir = f.root().join("unborn");
    std::fs::create_dir_all(&dir).unwrap();
    f.git(&dir, &["init", "--quiet"]);
    write(&dir.join("a.txt"), "a\n");
    let root = RepoRoot(dir);

    let base = f.vcs().baseline(&root).await.unwrap();
    let diff = f
        .vcs()
        .diff(&root, DiffRange::WorkspaceSince(base))
        .await
        .unwrap();
    assert_eq!(diff.files.len(), 1);
    assert_eq!(diff.files[0].path, Path::new("a.txt"));
    assert_eq!(diff.files[0].insertions, 1);
}

#[tokio::test]
async fn diff_workspace_since_an_older_baseline_includes_committed_and_uncommitted_changes() {
    let f = Fixture::new();
    let repo = f.repo_with_commit(
        "hist",
        &[("a.txt", "one\n"), ("b.txt", "bee\n"), ("c.txt", "same\n")],
    );
    let vcs = f.vcs();
    let root = RepoRoot(repo.clone());
    let base = vcs.baseline(&root).await.unwrap();

    write(&repo.join("a.txt"), "one\ntwo\n");
    f.commit_all(&repo, "c2");
    std::fs::remove_file(repo.join("b.txt")).unwrap();
    write(&repo.join("new.txt"), "fresh\n");

    let diff = vcs
        .diff(&root, DiffRange::WorkspaceSince(base.clone()))
        .await
        .unwrap();
    let mut summary: Vec<(String, u32, u32)> = diff
        .files
        .iter()
        .map(|d| (d.path.display().to_string(), d.insertions, d.deletions))
        .collect();
    summary.sort();
    assert_eq!(
        summary,
        vec![
            ("a.txt".to_owned(), 1, 0),
            ("b.txt".to_owned(), 0, 1),
            ("new.txt".to_owned(), 1, 0),
        ],
        "c.txt 没变不应出现"
    );
    let a = diff
        .files
        .iter()
        .find(|d| d.path == Path::new("a.txt"))
        .unwrap();
    assert_eq!(
        a.patch,
        "--- a/a.txt\n+++ b/a.txt\n@@ -1,1 +1,2 @@\n one\n+two\n"
    );

    // 两个提交之间：只有已提交的那处改动
    let head = vcs.baseline(&root).await.unwrap();
    let between = vcs
        .diff(
            &root,
            DiffRange::Between {
                from: base,
                to: head,
            },
        )
        .await
        .unwrap();
    let paths: Vec<_> = between.files.iter().map(|d| d.path.clone()).collect();
    assert_eq!(paths, vec![std::path::PathBuf::from("a.txt")]);
}

#[tokio::test]
async fn workspace_reads_on_a_moved_repo_report_path_missing() {
    let f = Fixture::new();
    let repo = f.repo_with_commit("moving", &[("a", "1\n")]);
    let root = RepoRoot(repo.clone());
    std::fs::rename(&repo, f.root().join("moved")).unwrap();

    assert_eq!(
        f.vcs().status(&root).await,
        Err(VcsError::PathMissing { path: repo })
    );
}

#[test]
fn capabilities_declare_cheap_isolated_workspaces() {
    assert!(Fixture::new().vcs().capabilities().cheap_isolated_workspace);
}
