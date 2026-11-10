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

// ---------- 审查第 1 轮回归（F-1、F-5、F-6、F-7） ----------

fn patch_of<'a>(diff: &'a iwork_ports::vcs::Diff, path: &str) -> &'a iwork_ports::vcs::FileDiff {
    diff.files
        .iter()
        .find(|d| d.path == Path::new(path))
        .unwrap_or_else(|| {
            panic!(
                "差异里没有 {path}：{:?}",
                diff.files.iter().map(|d| &d.path).collect::<Vec<_>>()
            )
        })
}

/// F-1：目录被换成指向仓库外的链接，差异里不得出现仓库外文件的内容。
#[cfg(unix)]
#[tokio::test]
async fn diff_never_reads_through_a_directory_symlink_out_of_the_repo() {
    let f = Fixture::new();
    let repo = f.repo_with_commit("leaky", &[("dir/sentinel", "inside\n")]);
    let outside = f.root().join("outside");
    write(&outside.join("sentinel"), "OUTSIDE-SECRET\n");
    write(&outside.join("other"), "OUTSIDE-OTHER\n");
    let root = RepoRoot(repo.clone());
    let base = f.vcs().baseline(&root).await.unwrap();

    std::fs::remove_dir_all(repo.join("dir")).unwrap();
    std::os::unix::fs::symlink(&outside, repo.join("dir")).unwrap();
    // 对照：最后一级是链接时只取链接目标，不取内容
    std::os::unix::fs::symlink(outside.join("other"), repo.join("leaf")).unwrap();

    let diff = f
        .vcs()
        .diff(&root, DiffRange::WorkspaceSince(base))
        .await
        .unwrap();
    for d in &diff.files {
        assert!(
            !d.patch.contains("OUTSIDE-SECRET"),
            "{}: {}",
            d.path.display(),
            d.patch
        );
        assert!(
            !d.patch.contains("OUTSIDE-OTHER"),
            "{}: {}",
            d.path.display(),
            d.patch
        );
    }
    let gone = patch_of(&diff, "dir/sentinel");
    assert!(gone.patch.contains("-inside"), "{}", gone.patch);
    assert!(
        patch_of(&diff, "dir")
            .patch
            .contains("new file mode 120000")
    );
}

/// F-5：改名后差异里要同时有源路径的删除和目标路径的新增。
#[tokio::test]
async fn diff_after_git_mv_keeps_both_the_deletion_and_the_addition() {
    let f = Fixture::new();
    let repo = f.repo_with_commit("mv", &[("a.txt", "same content\nline 2\n")]);
    let root = RepoRoot(repo.clone());
    let base = f.vcs().baseline(&root).await.unwrap();
    f.git(&repo, &["mv", "a.txt", "b.txt"]);

    let ws = f
        .vcs()
        .diff(&root, DiffRange::WorkspaceSince(base.clone()))
        .await
        .unwrap();
    assert_eq!(patch_of(&ws, "a.txt").deletions, 2);
    assert_eq!(patch_of(&ws, "b.txt").insertions, 2);

    f.git(&repo, &["commit", "--quiet", "-m", "mv"]);
    let head = f.vcs().baseline(&root).await.unwrap();
    let between = f
        .vcs()
        .diff(
            &root,
            DiffRange::Between {
                from: base,
                to: head,
            },
        )
        .await
        .unwrap();
    assert_eq!(patch_of(&between, "a.txt").deletions, 2);
    assert_eq!(patch_of(&between, "b.txt").insertions, 2);
}

/// F-6：只改执行位也要出差异（工作区与提交之间两种范围）。
#[cfg(unix)]
#[tokio::test]
async fn diff_reports_mode_only_changes() {
    use std::os::unix::fs::PermissionsExt;
    let f = Fixture::new();
    let repo = f.repo_with_commit("modes", &[("run.sh", "echo hi\n")]);
    let root = RepoRoot(repo.clone());
    let base = f.vcs().baseline(&root).await.unwrap();
    std::fs::set_permissions(repo.join("run.sh"), std::fs::Permissions::from_mode(0o755)).unwrap();

    let ws = f
        .vcs()
        .diff(&root, DiffRange::WorkspaceSince(base.clone()))
        .await
        .unwrap();
    let d = patch_of(&ws, "run.sh");
    assert_eq!(d.patch, "old mode 100644\nnew mode 100755\n");
    assert_eq!((d.insertions, d.deletions), (0, 0));

    f.commit_all(&repo, "chmod");
    let head = f.vcs().baseline(&root).await.unwrap();
    let between = f
        .vcs()
        .diff(
            &root,
            DiffRange::Between {
                from: base,
                to: head,
            },
        )
        .await
        .unwrap();
    assert_eq!(
        patch_of(&between, "run.sh").patch,
        "old mode 100644\nnew mode 100755\n"
    );
}

/// F-6：子模块引用（gitlink）变化要出差异。
#[tokio::test]
async fn diff_reports_gitlink_changes() {
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
    f.git(&sup, &["commit", "--quiet", "-m", "add submodule"]);
    let root = RepoRoot(sup.clone());
    let base = f.vcs().baseline(&root).await.unwrap();
    let old_id = f.git(&sup.join("deps/lib"), &["rev-parse", "HEAD"]);

    let sub = sup.join("deps/lib");
    f.git(&sub, &["config", "user.name", "T"]);
    f.git(&sub, &["config", "user.email", "t@example.invalid"]);
    write(&sub.join("l"), "2\n");
    f.commit_all(&sub, "bump");
    let new_id = f.git(&sub, &["rev-parse", "HEAD"]);
    let expected = format!("-Subproject commit {old_id}\n+Subproject commit {new_id}\n");

    let ws = f
        .vcs()
        .diff(&root, DiffRange::WorkspaceSince(base.clone()))
        .await
        .unwrap();
    assert!(
        patch_of(&ws, "deps/lib").patch.ends_with(&expected),
        "{}",
        patch_of(&ws, "deps/lib").patch
    );

    f.commit_all(&sup, "bump submodule");
    let head = f.vcs().baseline(&root).await.unwrap();
    let between = f
        .vcs()
        .diff(
            &root,
            DiffRange::Between {
                from: base,
                to: head,
            },
        )
        .await
        .unwrap();
    assert!(patch_of(&between, "deps/lib").patch.ends_with(&expected));
}

/// F-7：`eol=crlf` 的文件改一行，差异就是一增一删，不是整个文件重写。
#[tokio::test]
async fn diff_applies_eol_conversion_before_comparing() {
    let f = Fixture::new();
    let repo = f.repo_with_commit("crlf", &[(".gitattributes", "*.txt eol=crlf\n")]);
    write(&repo.join("a.txt"), "one\r\ntwo\r\nthree\r\n");
    f.commit_all(&repo, "crlf file");
    let root = RepoRoot(repo.clone());
    let base = f.vcs().baseline(&root).await.unwrap();
    write(&repo.join("a.txt"), "one\r\nTWO\r\nthree\r\n");

    let ws = f
        .vcs()
        .diff(&root, DiffRange::WorkspaceSince(base))
        .await
        .unwrap();
    let d = patch_of(&ws, "a.txt");
    assert_eq!((d.insertions, d.deletions), (1, 1), "{}", d.patch);
    assert!(d.patch.contains("-two\n+TWO\n"), "{}", d.patch);
}

// ---------- 审查第 2 轮回归（2/F-1、2/F-3） ----------

/// 2/F-1：读操作（status、已跟踪与未跟踪文件的 diff）绝不启动仓库配置里的外部过滤程序；
/// 需要外部过滤器的文件在差异里明确标注「不支持」，不生成假差异。
#[cfg(unix)]
#[tokio::test]
async fn reads_never_launch_external_filter_programs() {
    use std::os::unix::fs::PermissionsExt;
    let f = Fixture::new();
    let repo = f.repo_with_commit(
        "no-exec",
        &[(
            ".gitattributes",
            "*.cl filter=cleaner\n*.pr filter=proc\n*.txt eol=crlf\n",
        )],
    );
    write(&repo.join("tracked.cl"), "one\n");
    write(&repo.join("eol.txt"), "a\r\nb\r\nc\r\n");
    f.commit_all(&repo, "files before filters exist");
    let root = RepoRoot(repo.clone());
    let base = f.vcs().baseline(&root).await.unwrap();

    // Agent 能写 .git/config 的前提下配置两种外部过滤器；程序一旦运行就留下哨兵
    let sentinel = f.root().join("FILTER-RAN");
    let script = f.root().join("filter.sh");
    std::fs::write(
        &script,
        format!("#!/bin/sh\ntouch '{}'\ncat\n", sentinel.display()),
    )
    .unwrap();
    std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
    let script = script.to_str().unwrap();
    f.git(&repo, &["config", "filter.cleaner.clean", script]);
    f.git(&repo, &["config", "filter.proc.process", script]);
    f.git(&repo, &["config", "filter.proc.required", "true"]);

    write(&repo.join("tracked.cl"), "two\n");
    write(&repo.join("untracked.pr"), "new\n");
    write(&repo.join("untracked.cl"), "new\n");
    write(&repo.join("eol.txt"), "a\r\nB\r\nc\r\n");

    let status = f.vcs().status(&root).await.unwrap();
    assert!(!sentinel.exists(), "status 启动了外部过滤程序");
    assert!(
        status
            .changed
            .iter()
            .any(|c| c.path == Path::new("tracked.cl")),
        "需要过滤器的已跟踪文件不能报成没有改动：{status:?}"
    );

    let diff = f
        .vcs()
        .diff(&root, DiffRange::WorkspaceSince(base))
        .await
        .unwrap();
    assert!(!sentinel.exists(), "diff 启动了外部过滤程序");
    for path in ["tracked.cl", "untracked.pr", "untracked.cl"] {
        let d = patch_of(&diff, path);
        assert!(d.patch.contains("not supported"), "{path}: {}", d.patch);
        assert_eq!((d.insertions, d.deletions), (0, 0), "{path}");
    }
    // 对照：进程内的换行转换照常
    let eol = patch_of(&diff, "eol.txt");
    assert_eq!((eol.insertions, eol.deletions), (1, 1), "{}", eol.patch);
}

/// 2/F-1：LFS 文件工作区一侧不做指针转换，标注不支持并显示已存的指针；提交之间照常比较指针。
#[tokio::test]
async fn lfs_pointers_are_shown_not_converted() {
    let f = Fixture::new();
    let repo = f.repo_with_commit("lfs", &[(".gitattributes", "*.bin filter=lfs\n")]);
    let pointer = |oid: &str, size: u32| {
        format!("version https://git-lfs.github.com/spec/v1\noid sha256:{oid}\nsize {size}\n")
    };
    // 直接提交指针文本（模拟 LFS 已存的指针；测试里没有配置 lfs 过滤器，不会转换）
    write(&repo.join("big.bin"), &pointer(&"a".repeat(64), 10));
    f.commit_all(&repo, "pointer v1");
    let root = RepoRoot(repo.clone());
    let v1 = f.vcs().baseline(&root).await.unwrap();
    write(&repo.join("big.bin"), &pointer(&"b".repeat(64), 20));
    f.commit_all(&repo, "pointer v2");
    let v2 = f.vcs().baseline(&root).await.unwrap();

    // 用户配置了 lfs（来自全局配置），工作区里是真实内容
    f.git(
        &repo,
        &["config", "filter.lfs.clean", "git-lfs clean -- %f"],
    );
    f.git(&repo, &["config", "filter.lfs.required", "true"]);
    write(&repo.join("big.bin"), "real binary-ish content\n");
    let ws = f
        .vcs()
        .diff(&root, DiffRange::WorkspaceSince(v2.clone()))
        .await
        .unwrap();
    let d = patch_of(&ws, "big.bin");
    assert!(d.patch.contains("not supported"), "{}", d.patch);
    assert!(
        d.patch.contains(&format!("oid sha256:{}", "b".repeat(64))),
        "显示已存的指针：{}",
        d.patch
    );
    assert!(!d.patch.contains("real binary-ish content"));

    let between = f
        .vcs()
        .diff(&root, DiffRange::Between { from: v1, to: v2 })
        .await
        .unwrap();
    let p = &patch_of(&between, "big.bin").patch;
    assert!(p.contains("-size 10\n") && p.contains("+size 20\n"), "{p}");
}

/// 2/F-3：子模块引用从 A 更新到 B 后，未初始化 / 已 deinit / 目录不存在都按索引里的 B 比较；
/// 只有索引里也没有这个条目才算删除。
#[tokio::test]
async fn gitlink_without_checkout_uses_the_index_value() {
    struct Setup {
        sup: std::path::PathBuf,
        base: BaselineRef,
        expected_update: String,
        old_id: String,
    }
    async fn setup(f: &Fixture, name: &str) -> Setup {
        let lib = f.repo_with_commit(&format!("{name}-lib"), &[("l", "1\n")]);
        let sup = f.repo_with_commit(name, &[("s", "1\n")]);
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
        f.git(&sup, &["commit", "--quiet", "-m", "add submodule"]);
        let base = f.vcs().baseline(&RepoRoot(sup.clone())).await.unwrap();
        let sub = sup.join("deps/lib");
        let old_id = f.git(&sub, &["rev-parse", "HEAD"]);
        f.git(&sub, &["config", "user.name", "T"]);
        f.git(&sub, &["config", "user.email", "t@example.invalid"]);
        write(&sub.join("l"), "2\n");
        f.commit_all(&sub, "bump");
        let new_id = f.git(&sub, &["rev-parse", "HEAD"]);
        f.commit_all(&sup, "bump submodule");
        Setup {
            expected_update: format!("-Subproject commit {old_id}\n+Subproject commit {new_id}\n"),
            sup,
            base,
            old_id,
        }
    }
    let f = Fixture::new();

    // 已 deinit：只剩空目录
    let s = setup(&f, "deinit").await;
    f.git(
        &s.sup,
        &["submodule", "--quiet", "deinit", "--force", "deps/lib"],
    );
    let d = f
        .vcs()
        .diff(
            &RepoRoot(s.sup.clone()),
            DiffRange::WorkspaceSince(s.base.clone()),
        )
        .await
        .unwrap();
    assert!(
        patch_of(&d, "deps/lib").patch.ends_with(&s.expected_update),
        "deinit: {}",
        patch_of(&d, "deps/lib").patch
    );

    // 目录不存在
    std::fs::remove_dir(s.sup.join("deps/lib")).unwrap();
    let d = f
        .vcs()
        .diff(
            &RepoRoot(s.sup.clone()),
            DiffRange::WorkspaceSince(s.base.clone()),
        )
        .await
        .unwrap();
    assert!(
        patch_of(&d, "deps/lib").patch.ends_with(&s.expected_update),
        "missing dir: {}",
        patch_of(&d, "deps/lib").patch
    );

    // 未初始化：不带子模块地克隆超级项目
    let s2 = setup(&f, "uninit").await;
    let clone = f.root().join("uninit-clone");
    f.git(
        &f.root(),
        &[
            "clone",
            "--quiet",
            s2.sup.to_str().unwrap(),
            clone.to_str().unwrap(),
        ],
    );
    let d = f
        .vcs()
        .diff(
            &RepoRoot(clone.clone()),
            DiffRange::WorkspaceSince(s2.base.clone()),
        )
        .await
        .unwrap();
    assert!(
        patch_of(&d, "deps/lib")
            .patch
            .ends_with(&s2.expected_update),
        "uninit: {}",
        patch_of(&d, "deps/lib").patch
    );

    // 真的删除：索引里也没有了
    let s3 = setup(&f, "removed").await;
    f.git(&s3.sup, &["rm", "--quiet", "deps/lib"]);
    let d = f
        .vcs()
        .diff(
            &RepoRoot(s3.sup.clone()),
            DiffRange::WorkspaceSince(s3.base.clone()),
        )
        .await
        .unwrap();
    let p = &patch_of(&d, "deps/lib").patch;
    assert!(p.starts_with("deleted file mode 160000\n"), "{p}");
    assert!(
        p.ends_with(&format!("-Subproject commit {}\n", s3.old_id)),
        "{p}"
    );
}

// ---------- 审查第 3 轮回归（2/F-1 子模块、3/F-1 外部过滤的「没改动」判断） ----------

/// 2/F-1：子模块自己配置的 clean 过滤器，在顶层 status 与 diff 里都不执行；子模块提交变化仍显示。
#[cfg(unix)]
#[tokio::test]
async fn submodule_filters_never_run_and_commit_changes_still_show() {
    use std::os::unix::fs::PermissionsExt;
    let f = Fixture::new();
    let lib = f.repo_with_commit(
        "sm-lib",
        &[(".gitattributes", "*.t filter=x\n"), ("a.t", "1\n")],
    );
    let sup = f.repo_with_commit("sm-super", &[("s", "1\n")]);
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
    f.git(&sup, &["commit", "--quiet", "-m", "add submodule"]);
    let root = RepoRoot(sup.clone());
    let base = f.vcs().baseline(&root).await.unwrap();

    let sub = sup.join("deps/lib");
    f.git(&sub, &["config", "user.name", "T"]);
    f.git(&sub, &["config", "user.email", "t@example.invalid"]);
    let old_id = f.git(&sub, &["rev-parse", "HEAD"]);
    write(&sub.join("b"), "x\n");
    f.commit_all(&sub, "bump");
    let new_id = f.git(&sub, &["rev-parse", "HEAD"]);

    // 子模块里配一个会留哨兵的 clean 过滤器，再改动受它管的文件
    let sentinel = f.root().join("SUBMODULE-FILTER-RAN");
    let script = f.root().join("sm-filter.sh");
    std::fs::write(
        &script,
        format!("#!/bin/sh\ntouch '{}'\ncat\n", sentinel.display()),
    )
    .unwrap();
    std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
    f.git(
        &sub,
        &["config", "filter.x.clean", script.to_str().unwrap()],
    );
    write(&sub.join("a.t"), "2\n");

    let status = f.vcs().status(&root).await.unwrap();
    assert!(!sentinel.exists(), "status 执行了子模块的过滤器");
    assert!(
        status
            .changed
            .iter()
            .any(|c| c.path == Path::new("deps/lib") && c.change == ChangeKind::Modified),
        "子模块提交变化要显示：{status:?}"
    );

    let diff = f
        .vcs()
        .diff(&root, DiffRange::WorkspaceSince(base))
        .await
        .unwrap();
    assert!(!sentinel.exists(), "diff 执行了子模块的过滤器");
    let p = &patch_of(&diff, "deps/lib").patch;
    assert!(
        p.ends_with(&format!(
            "-Subproject commit {old_id}\n+Subproject commit {new_id}\n"
        )),
        "{p}"
    );
}

/// 3/F-1：需要外部过滤器的文件，工作区内容恰好与存储内容等长等值（rot13 反例），git 报 M；
/// iWork 不能凭原始字节相同判成没改动，要列为「可能有改动，内容比较不支持」。
#[tokio::test]
async fn externally_filtered_file_is_never_judged_unchanged_by_raw_bytes() {
    let f = Fixture::new();
    let repo = f.repo_with_commit("rot", &[(".gitattributes", "*.r filter=rot\n")]);
    f.git(
        &repo,
        &["config", "filter.rot.clean", "tr a-zA-Z n-za-mN-ZA-M"],
    );
    f.git(
        &repo,
        &["config", "filter.rot.smudge", "tr a-zA-Z n-za-mN-ZA-M"],
    );
    write(&repo.join("x.r"), "hello\n");
    write(&repo.join("calm.r"), "quiet\n");
    // 对照文件的 mtime 早于随后写出的索引：否则同一时间粒度内它是 racy 的，按规则本就该保守列出
    set_mtime(
        &repo.join("calm.r"),
        std::time::SystemTime::now() - std::time::Duration::from_secs(3600),
    );
    f.commit_all(&repo, "rot13 stored"); // 存储内容是 "uryyb\n"
    assert_eq!(f.git(&repo, &["cat-file", "-p", "HEAD:x.r"]), "uryyb");
    let root = RepoRoot(repo.clone());
    let base = f.vcs().baseline(&root).await.unwrap();

    // 工作区改成与存储内容逐字节相同（git 过滤后是 "hello" → 不同 → M）
    write(&repo.join("x.r"), "uryyb\n");
    let later = std::time::SystemTime::now() + std::time::Duration::from_secs(5);
    std::fs::File::options()
        .write(true)
        .open(repo.join("x.r"))
        .unwrap()
        .set_modified(later)
        .unwrap();
    assert!(
        f.git(&repo, &["status", "--porcelain"]).contains("M x.r"),
        "前提：git 报 M"
    );

    let status = f.vcs().status(&root).await.unwrap();
    assert!(
        status
            .changed
            .iter()
            .any(|c| c.path == Path::new("x.r") && c.change == ChangeKind::Modified),
        "{status:?}"
    );
    assert!(
        !status.changed.iter().any(|c| c.path == Path::new("calm.r")),
        "没动过的不列：{status:?}"
    );

    let diff = f
        .vcs()
        .diff(&root, DiffRange::WorkspaceSince(base))
        .await
        .unwrap();
    assert!(
        patch_of(&diff, "x.r").patch.contains("not supported"),
        "{}",
        patch_of(&diff, "x.r").patch
    );
    assert!(!diff.files.iter().any(|d| d.path == Path::new("calm.r")));
}

// ---------- 第 5 轮：外部过滤文件的 racy 条目 ----------

/// 建一个 rot13 过滤文件已提交的仓库；`x.r` 的 mtime 在加入暂存区前设成 `mtime`，存储内容是 "uryyb\n"。
/// 关掉 `core.trustctime`：改写文件必然改变 ctime，而 ctime 设不回去；这样改写后 stat 能与暂存区记录完全相同。
fn rot13_repo_with_mtime(
    f: &Fixture,
    name: &str,
    mtime: std::time::SystemTime,
) -> (std::path::PathBuf, BaselineRef) {
    let repo = f.repo_with_commit(name, &[(".gitattributes", "*.r filter=rot\n")]);
    f.git(
        &repo,
        &["config", "filter.rot.clean", "tr a-zA-Z n-za-mN-ZA-M"],
    );
    f.git(
        &repo,
        &["config", "filter.rot.smudge", "tr a-zA-Z n-za-mN-ZA-M"],
    );
    f.git(&repo, &["config", "core.trustctime", "false"]);
    write(&repo.join("x.r"), "hello\n");
    set_mtime(&repo.join("x.r"), mtime);
    f.commit_all(&repo, "rot13 stored");
    let head = f.git(&repo, &["rev-parse", "HEAD"]);
    (repo, BaselineRef(head))
}

fn set_mtime(path: &Path, t: std::time::SystemTime) {
    std::fs::File::options()
        .write(true)
        .open(path)
        .unwrap()
        .set_modified(t)
        .unwrap();
}

/// 在同一时间粒度内改写成与存储内容等长等值的内容，并把 mtime 改回原值：stat 与暂存区记录完全相同。
fn rewrite_keeping_stat(path: &Path, mtime: std::time::SystemTime) {
    std::fs::write(path, "uryyb\n").unwrap();
    set_mtime(path, mtime);
}

/// 条目 mtime 不早于索引文件自身的时间戳（racy）：stat 相同也说明不了没改动，要列为可能有改动。
#[tokio::test]
async fn racy_externally_filtered_file_with_identical_stat_is_listed() {
    let f = Fixture::new();
    // 未来的时间：一定晚于随后写出的索引文件时间戳
    let racy_mtime = std::time::SystemTime::now() + std::time::Duration::from_secs(3600);
    let (repo, base) = rot13_repo_with_mtime(&f, "racy", racy_mtime);
    rewrite_keeping_stat(&repo.join("x.r"), racy_mtime);
    let root = RepoRoot(repo.clone());

    let status = f.vcs().status(&root).await.unwrap();
    assert!(
        status
            .changed
            .iter()
            .any(|c| c.path == Path::new("x.r") && c.change == ChangeKind::Modified),
        "{status:?}"
    );
    let diff = f
        .vcs()
        .diff(&root, DiffRange::WorkspaceSince(base))
        .await
        .unwrap();
    assert!(
        patch_of(&diff, "x.r").patch.contains("not supported"),
        "{}",
        patch_of(&diff, "x.r").patch
    );
}

/// 对照：条目不是 racy（mtime 早于索引时间戳）且 stat 与暂存区记录相同时不列出。
#[tokio::test]
async fn non_racy_externally_filtered_file_with_identical_stat_is_not_listed() {
    let f = Fixture::new();
    let past_mtime = std::time::SystemTime::now() - std::time::Duration::from_secs(3600);
    let (repo, base) = rot13_repo_with_mtime(&f, "settled", past_mtime);
    rewrite_keeping_stat(&repo.join("x.r"), past_mtime);
    let root = RepoRoot(repo.clone());

    let status = f.vcs().status(&root).await.unwrap();
    assert!(
        !status.changed.iter().any(|c| c.path == Path::new("x.r")),
        "{status:?}"
    );
    let diff = f
        .vcs()
        .diff(&root, DiffRange::WorkspaceSince(base))
        .await
        .unwrap();
    assert!(
        !diff.files.iter().any(|d| d.path == Path::new("x.r")),
        "{:?}",
        diff.files
    );
}
