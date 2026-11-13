//! 交给 gix 之前先查仓库的布局（T10a 方案 2.4a，步 2b）：后台读仓库时不顺着仓库里的东西读到仓库外面去。
//!
//! 每一格在仓库外摆一个带这次运行独有标记的东西（文件内容或目录名里带标记），把仓库里的某一处指过去，
//! 断言：拒绝的原因是那几种固定文字之一，返回值（含错误文字）里没有标记。
//!
//! 只在 unix 上跑：要建符号链接、对拍真实文件系统；M1 不支持 Windows（T03）。
#![cfg(unix)]

#[allow(dead_code, reason = "共用的夹具，这里只用一部分")]
mod common;

use std::os::unix::fs::symlink;
use std::path::{Path, PathBuf};

use common::{Fixture, write};
use iwork_ports::vcs::{
    ChangeKind, DiffRange, PathKind, PathVersion, RepoRoot, VcsError, VcsProvider,
};
use sha2::{Digest, Sha256};

// 拒绝的原因（方案 2.4a「拒绝是什么样」：固定的几种文字，不带链接指向的路径）。这里独立写一份当期望值。
const DOT_GIT_SYMLINK: &str = ".git 是符号链接";
const SUBMODULE_DOT_GIT_DIR: &str =
    "子模块的 .git 是目录，只认指到上层仓库 modules/ 之下的 gitfile";
const GITFILE_OUTSIDE_MODULES: &str = "子模块的 .git 指到上层仓库的 modules/ 之外";
const METADATA_SYMLINK: &str = "git 目录里有符号链接";
const ALTERNATES_UNSUPPORTED: &str = "借用别处的对象库（objects/info/alternates），不支持";
const WORKTREE_NOT_ROOT: &str = "仓库的工作区不是登记的仓库根";

/// 这次运行独有的标记。
fn marker(tag: &str) -> String {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    format!("T10A-2B-{tag}-{}-{nanos}", std::process::id())
}

/// 拒绝的原因是 `reason`，错误里没有标记。
fn assert_refused(err: &VcsError, reason: &str, mark: &str) {
    let text = format!("{err} / {err:?}");
    assert!(
        text.contains(reason),
        "拒绝原因应是「{reason}」，实际：{text}"
    );
    assert!(!text.contains(mark), "错误里不该出现仓库外的标记：{text}");
}

fn assert_no_marker(value: &impl std::fmt::Debug, mark: &str) {
    let text = format!("{value:?}");
    assert!(!text.contains(mark), "返回值里不该出现仓库外的标记：{text}");
}

fn paths(names: &[&str]) -> Vec<PathBuf> {
    names.iter().map(PathBuf::from).collect()
}

fn untracked(status: &iwork_ports::vcs::WorkspaceStatus) -> Vec<PathBuf> {
    status
        .changed
        .iter()
        .filter(|c| c.change == ChangeKind::Untracked)
        .map(|c| c.path.clone())
        .collect()
}

/// 上层仓库 `super`，`deps/lib` 是一个已检出的子模块（git 吸收进 `.git/modules/` 的标准布局）。
fn super_with_submodule(f: &Fixture) -> PathBuf {
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
    sup
}

/// 仓库外一个真仓库，HEAD 指向一个提交信息带标记的提交；返回 (`.git` 的路径, 那个提交)。
fn outside_repo_at_marked_commit(f: &Fixture, mark: &str) -> (PathBuf, String) {
    let dir = f.root().join(format!("outside-{mark}"));
    let repo = f.repo_with_commit(
        dir.file_name().unwrap().to_str().unwrap(),
        &[("m", &format!("{mark}\n"))],
    );
    f.git(&repo, &["commit", "--quiet", "--allow-empty", "-m", mark]);
    let id = f.git(&repo, &["rev-parse", "HEAD"]);
    (repo.join(".git"), id)
}

fn replace_with_symlink(path: &Path, target: &Path) {
    if path.is_dir() && !path.is_symlink() {
        std::fs::remove_dir_all(path).unwrap();
    } else {
        std::fs::remove_file(path).unwrap();
    }
    symlink(target, path).unwrap();
}

/// 7/F-2 第一格：`sub/.git` 是指向仓库外带标记的普通文件的链接。
#[tokio::test]
async fn submodule_dot_git_symlink_to_marked_file_is_refused() {
    let f = Fixture::new();
    let sup = super_with_submodule(&f);
    let mark = marker("subfile");
    let outside = f.root().join(format!("outside-{mark}"));
    write(&outside, &format!("{mark}\n"));
    replace_with_symlink(&sup.join("deps/lib/.git"), &outside);
    write(&sup.join("s"), "2\n");
    let root = RepoRoot(sup.clone());

    let err = f
        .vcs()
        .diff(
            &root,
            DiffRange::WorkspaceSince(f.vcs().baseline(&root).await.unwrap()),
        )
        .await
        .unwrap_err();
    assert_refused(&err, DOT_GIT_SYMLINK, &mark);

    // 上层仓库别的条目照常
    let status = f.vcs().status(&root).await.unwrap();
    assert_no_marker(&status, &mark);
    assert!(
        status
            .changed
            .iter()
            .any(|c| c.path == Path::new("s") && c.change == ChangeKind::Modified),
        "{status:?}"
    );
    let versions = f
        .vcs()
        .worktree_versions(&root, &paths(&["deps/lib", "s"]))
        .await
        .unwrap();
    assert_eq!(versions[0], PathVersion::Unknown);
    assert_eq!(
        versions[1],
        PathVersion::Present {
            kind: PathKind::File,
            digest: Sha256::digest(b"2\n").into(),
        }
    );
}

/// 7/F-2 第二格：`sub/.git` 链到仓库外一个合法的 git 目录，它的 HEAD 是带标记的提交。只断言「版本是
/// Unknown」不够（7.2a 的 K18）：要断言拒绝的原因，并且读出来的不是那个提交。
#[tokio::test]
async fn submodule_dot_git_symlink_to_git_dir_is_refused() {
    let f = Fixture::new();
    let sup = super_with_submodule(&f);
    let mark = marker("subdir");
    let (outside_git, marked) = outside_repo_at_marked_commit(&f, &mark);
    replace_with_symlink(&sup.join("deps/lib/.git"), &outside_git);
    let root = RepoRoot(sup.clone());
    let base = f.vcs().baseline(&root).await.unwrap();

    let err = f
        .vcs()
        .diff(&root, DiffRange::WorkspaceSince(base))
        .await
        .unwrap_err();
    assert_refused(&err, DOT_GIT_SYMLINK, &marked);
    assert_refused(&err, DOT_GIT_SYMLINK, &mark);

    let versions = f
        .vcs()
        .worktree_versions(&root, &paths(&["deps/lib"]))
        .await
        .unwrap();
    let marked_version = PathVersion::Present {
        kind: PathKind::Submodule,
        digest: Sha256::digest(format!("Subproject commit {marked}\n")).into(),
    };
    assert_ne!(versions[0], marked_version, "不该读出仓库外那个提交");
    let status = f.vcs().status(&root).await.unwrap();
    assert_no_marker(&status, &marked);
}

/// 子模块的 `.git` 是 gitfile、指到上层 `modules/` 之外的一个 git 目录 → 拒绝；对照：标准布局照常读出检出的提交。
#[tokio::test]
async fn submodule_gitfile_outside_modules_is_refused() {
    let f = Fixture::new();
    let sup = super_with_submodule(&f);
    let root = RepoRoot(sup.clone());
    let base = f.vcs().baseline(&root).await.unwrap();

    // 对照：标准布局（gitfile 指到 .git/modules/deps/lib）读得出检出的提交
    let sub = sup.join("deps/lib");
    f.git(&sub, &["config", "user.name", "T"]);
    f.git(&sub, &["config", "user.email", "t@example.invalid"]);
    f.git(&sub, &["commit", "--quiet", "--allow-empty", "-m", "bump"]);
    let bumped = f.git(&sub, &["rev-parse", "HEAD"]);
    let ok = f
        .vcs()
        .diff(&root, DiffRange::WorkspaceSince(base.clone()))
        .await
        .unwrap();
    assert!(
        ok.files.iter().any(|d| d.patch.contains(&bumped)),
        "对照：标准布局的子模块照常读出检出的提交：{ok:?}"
    );

    let mark = marker("gitfile");
    let (outside_git, marked) = outside_repo_at_marked_commit(&f, &mark);
    // 外面那个 git 目录的 config 里也带上标记
    let cfg = outside_git.join("config");
    let mut text = std::fs::read_to_string(&cfg).unwrap();
    text.push_str(&format!("# {mark}\n"));
    std::fs::write(&cfg, text).unwrap();
    std::fs::remove_file(sub.join(".git")).unwrap();
    write(
        &sub.join(".git"),
        &format!("gitdir: {}\n", outside_git.display()),
    );

    let err = f
        .vcs()
        .diff(&root, DiffRange::WorkspaceSince(base))
        .await
        .unwrap_err();
    assert_refused(&err, GITFILE_OUTSIDE_MODULES, &marked);
    assert_refused(&err, GITFILE_OUTSIDE_MODULES, &mark);
    let status = f.vcs().status(&root).await.unwrap();
    assert_no_marker(&status, &marked);
}

/// 项目根的 `.git` 是链接（指到仓库外一个 HEAD 带标记提交的 git 目录）→ 拒绝。
#[tokio::test]
async fn root_dot_git_symlink_is_refused() {
    let f = Fixture::new();
    let repo = f.repo_with_commit("proj", &[("a", "1\n")]);
    let mark = marker("root");
    let (outside_git, marked) = outside_repo_at_marked_commit(&f, &mark);
    replace_with_symlink(&repo.join(".git"), &outside_git);
    let root = RepoRoot(repo);

    let err = f.vcs().baseline(&root).await.unwrap_err();
    assert_refused(&err, DOT_GIT_SYMLINK, &marked);
    assert_refused(&err, DOT_GIT_SYMLINK, &mark);
    let err = f.vcs().status(&root).await.unwrap_err();
    assert_refused(&err, DOT_GIT_SYMLINK, &mark);
}

/// git 目录里的元数据被换成链接：把真东西挪到仓库外（目录名带标记）再链回来。gix 会照常跟过去读，所以没有这一道
/// 检查时这几格都是「读成功」。
#[tokio::test]
async fn git_dir_entry_symlink_is_refused() {
    // 每格都跑完再一起断言，红灯时能看到每一格各自怎样
    let mut wrong = Vec::new();
    for entry in [
        "HEAD",
        "config",
        "index",
        "packed-refs",
        "objects",
        "refs",
        "objects/pack/*.pack",
        "info/exclude",
    ] {
        let f = Fixture::new();
        let repo = f.repo_with_commit("proj", &[("a", "1\n")]);
        // packed-refs、pack 文件要先有
        f.git(&repo, &["pack-refs", "--all"]);
        f.git(&repo, &["gc", "--quiet"]);
        let git = repo.join(".git");
        let rel = if let Some(dir) = entry.strip_suffix("/*.pack") {
            let pack = std::fs::read_dir(git.join(dir))
                .unwrap()
                .map(|e| e.unwrap().path())
                .find(|p| p.extension().is_some_and(|x| x == "pack"))
                .expect("gc 之后有 pack 文件");
            pack.strip_prefix(&git).unwrap().to_owned()
        } else {
            PathBuf::from(entry)
        };
        let mark = marker("entry");
        let outside = f.root().join(format!("outside-{mark}"));
        std::fs::create_dir_all(&outside).unwrap();
        let moved = outside.join(rel.file_name().unwrap());
        std::fs::rename(git.join(&rel), &moved).unwrap();
        symlink(&moved, git.join(&rel)).unwrap();
        let root = RepoRoot(repo);

        match f.vcs().status(&root).await {
            Err(err) => {
                assert_refused(&err, METADATA_SYMLINK, &mark);
            }
            Ok(status) => wrong.push(format!("{entry}：读成功了 {status:?}")),
        }
    }
    assert!(wrong.is_empty(), "这些链接没被拒绝：{wrong:#?}");
}

/// `refs/` 深处的一个引用是链接（HEAD 指向它，它链到仓库外一个带标记提交编号的文件）→ 拒绝。
#[tokio::test]
async fn ref_symlink_deep_in_tree_is_refused() {
    let f = Fixture::new();
    let repo = f.repo_with_commit("proj", &[("a", "1\n")]);
    f.git(
        &repo,
        &["commit", "--quiet", "--allow-empty", "-m", "second"],
    );
    let second = f.git(&repo, &["rev-parse", "HEAD"]);
    f.git(&repo, &["reset", "--quiet", "--hard", "HEAD~1"]);
    let mark = marker("ref");
    let outside = f.root().join(format!("outside-{mark}"));
    write(&outside, &format!("{second}\n"));
    let deep = repo.join(".git/refs/heads/deep/er/x");
    std::fs::create_dir_all(deep.parent().unwrap()).unwrap();
    symlink(&outside, &deep).unwrap();
    std::fs::write(repo.join(".git/HEAD"), "ref: refs/heads/deep/er/x\n").unwrap();
    let root = RepoRoot(repo);

    let err = f.vcs().baseline(&root).await.unwrap_err();
    assert_refused(&err, METADATA_SYMLINK, &second);
    assert_refused(&err, METADATA_SYMLINK, &mark);
}

/// `config` 的 `include.path` 指到仓库外带标记的文件，文件里设一个看得出效果的配置（`core.fileMode = false`：
/// 执行位的变化就不算改动）→ 打开成功，那个配置没生效。
#[tokio::test]
async fn config_include_is_not_followed() {
    let f = Fixture::new();
    let repo = f.repo_with_commit("proj", &[("a", "1\n")]);
    let mark = marker("include");
    let included = f.root().join(format!("outside-{mark}.cfg"));
    write(
        &included,
        &format!("# {mark}\n[core]\n\tfileMode = false\n"),
    );
    f.git(
        &repo,
        &["config", "include.path", included.to_str().unwrap()],
    );
    let a = repo.join("a");
    let mut perm = std::fs::metadata(&a).unwrap().permissions();
    std::os::unix::fs::PermissionsExt::set_mode(&mut perm, 0o755);
    std::fs::set_permissions(&a, perm).unwrap();

    let status = f.vcs().status(&RepoRoot(repo)).await.unwrap();
    assert_no_marker(&status, &mark);
    assert!(
        status
            .changed
            .iter()
            .any(|c| c.path == Path::new("a") && c.change == ChangeKind::Modified),
        "include 进来的 fileMode = false 不该生效，执行位变化照常算改动：{status:?}"
    );
}

/// 仓库自己 `config` 里的 `core.excludesFile` 指到仓库外带标记的文件（里面一条忽略规则）→ 不读，规则不生效。
#[tokio::test]
async fn repo_excludes_file_is_not_read() {
    let f = Fixture::new();
    let repo = f.repo_with_commit("proj", &[("a", "1\n")]);
    let mark = marker("excludes");
    let outside = f.root().join(format!("outside-{mark}"));
    write(&outside, &format!("# {mark}\nhidden.txt\n"));
    f.git(
        &repo,
        &["config", "core.excludesFile", outside.to_str().unwrap()],
    );
    write(&repo.join("hidden.txt"), "x\n");

    let status = f.vcs().status(&RepoRoot(repo)).await.unwrap();
    assert_no_marker(&status, &mark);
    assert_eq!(untracked(&status), paths(&["hidden.txt"]));
}

/// 仓库自己 `config` 里的 `core.attributesFile` 指到仓库外带标记的文件（给 `f.txt` 配外部过滤器）→ 不读，
/// `f.txt` 照常比较内容，不会被标成「要外部过滤程序」。
#[tokio::test]
async fn repo_attributes_file_is_not_read() {
    let f = Fixture::new();
    let repo = f.repo_with_commit("proj", &[("f.txt", "1\n")]);
    let mark = marker("attrs");
    let outside = f.root().join(format!("outside-{mark}"));
    write(&outside, &format!("# {mark}\nf.txt filter=mk\n"));
    f.git(&repo, &["config", "filter.mk.clean", "cat"]);
    f.git(
        &repo,
        &["config", "core.attributesFile", outside.to_str().unwrap()],
    );
    let root = RepoRoot(repo.clone());
    let base = f.vcs().baseline(&root).await.unwrap();
    write(&repo.join("f.txt"), "2\n");

    let diff = f
        .vcs()
        .diff(&root, DiffRange::WorkspaceSince(base))
        .await
        .unwrap();
    assert_no_marker(&diff, &mark);
    let patch = &diff
        .files
        .iter()
        .find(|d| d.path == Path::new("f.txt"))
        .unwrap()
        .patch;
    assert!(patch.contains("-1\n+2\n"), "照常比较内容：{patch}");
}

/// `info/exclude` 被换成指向仓库外带标记文件的链接 → 拒绝（git 目录 `info/` 的直属条目）。
#[tokio::test]
async fn info_exclude_symlink_is_refused() {
    let f = Fixture::new();
    let repo = f.repo_with_commit("proj", &[("a", "1\n")]);
    let mark = marker("info");
    let outside = f.root().join(format!("outside-{mark}"));
    write(&outside, &format!("# {mark}\nhidden.txt\n"));
    write(&repo.join("hidden.txt"), "x\n");
    let exclude = repo.join(".git/info/exclude");
    std::fs::create_dir_all(exclude.parent().unwrap()).unwrap();
    let _ = std::fs::remove_file(&exclude);
    symlink(&outside, &exclude).unwrap();

    let err = f.vcs().status(&RepoRoot(repo)).await.unwrap_err();
    assert_refused(&err, METADATA_SYMLINK, &mark);
}

/// `objects/info/alternates` 指到另一个仓库（`git clone --shared`）→ 拒绝，原因是「不支持」。
#[tokio::test]
async fn alternates_is_unsupported() {
    let f = Fixture::new();
    let mark = marker("alt");
    let src = f.repo_with_commit(&format!("outside-{mark}"), &[("a", "1\n")]);
    let dst = f.root().join("proj");
    f.git(
        &f.root(),
        &[
            "clone",
            "--quiet",
            "--shared",
            src.to_str().unwrap(),
            dst.to_str().unwrap(),
        ],
    );
    assert!(dst.join(".git/objects/info/alternates").is_file());
    let root = RepoRoot(dst);

    let err = f.vcs().baseline(&root).await.unwrap_err();
    assert_refused(&err, ALTERNATES_UNSUPPORTED, &mark);
    let err = f.vcs().status(&root).await.unwrap_err();
    assert_refused(&err, ALTERNATES_UNSUPPORTED, &mark);
}

/// `core.worktree` 把工作区指到别处一格；`.git` 本身是裸仓库一格 → 拒绝。
#[tokio::test]
async fn worktree_elsewhere_or_bare_is_refused() {
    let f = Fixture::new();
    let mark = marker("wt");
    let elsewhere = f.root().join(format!("outside-{mark}"));
    write(&elsewhere.join("a"), "1\n");
    let repo = f.repo_with_commit("proj", &[("a", "1\n")]);
    f.git(
        &repo,
        &["config", "core.worktree", elsewhere.to_str().unwrap()],
    );
    let err = f.vcs().status(&RepoRoot(repo)).await.unwrap_err();
    assert_refused(&err, WORKTREE_NOT_ROOT, &mark);

    let repo = f.repo_with_commit("bare", &[("a", "1\n")]);
    f.git(&repo, &["config", "core.bare", "true"]);
    let err = f.vcs().status(&RepoRoot(repo)).await.unwrap_err();
    assert_refused(&err, WORKTREE_NOT_ROOT, &mark);
}

/// 正常的 `git worktree add` 出来的工作树照常 `status`、`diff`（别把好的也拦了）。
#[tokio::test]
async fn linked_worktree_still_works() {
    let f = Fixture::new();
    let repo = f.repo_with_commit("proj", &[("a", "1\n")]);
    let wt = f.root().join("linked");
    f.git(
        &repo,
        &[
            "worktree",
            "add",
            "--quiet",
            "-b",
            "side",
            wt.to_str().unwrap(),
        ],
    );
    let root = RepoRoot(wt.clone());
    let base = f.vcs().baseline(&root).await.unwrap();
    write(&wt.join("a"), "2\n");
    write(&wt.join("new"), "n\n");

    let status = f.vcs().status(&root).await.unwrap();
    assert!(
        status
            .changed
            .iter()
            .any(|c| c.path == Path::new("a") && c.change == ChangeKind::Modified),
        "{status:?}"
    );
    assert_eq!(untracked(&status), paths(&["new"]));
    let diff = f
        .vcs()
        .diff(&root, DiffRange::WorkspaceSince(base))
        .await
        .unwrap();
    assert!(diff.files.iter().any(|d| d.patch.contains("-1\n+2\n")));
}

/// 子模块的 `.git` 是目录（没吸收进上层 `modules/` 的老式布局）→ 拒绝（T10a 方案 KA2 第 7 点，supervisor 定 (a)）。
/// 这个目录在工作区里，Agent 写得进去：在它的对象库里摆一个链接，HEAD 指向那个对象，gix 读 HEAD 剥标签时会跟过去读。
/// 外面是一个真的标签对象和它指向的带标记的提交，读穿了就能从版本与差异里认出那个提交。
#[tokio::test]
async fn submodule_dot_git_dir_is_refused() {
    let f = Fixture::new();
    let sup = super_with_submodule(&f);
    let sub = sup.join("deps/lib");
    let mark = marker("subgitdir");
    let (outside_git, marked) = outside_repo_at_marked_commit(&f, &mark);
    let outside = outside_git.parent().unwrap();
    f.git(outside, &["tag", "-a", "-m", &mark, "t"]);
    let tag = f.git(outside, &["rev-parse", "t"]);
    let loose = |git: &Path, id: &str| git.join("objects").join(&id[..2]).join(&id[2..]);
    for id in [&tag, &marked] {
        assert!(loose(&outside_git, id).is_file(), "外面的 {id} 是松散对象");
    }

    // 把吸收进上层的 git 目录挪回子模块目录里，变成老式布局
    let absorbed = sup.join(".git/modules/deps/lib");
    std::fs::remove_file(sub.join(".git")).unwrap();
    std::fs::rename(&absorbed, sub.join(".git")).unwrap();
    let sub_config = sub.join(".git/config");
    f.git(
        &sup,
        &[
            "config",
            "--file",
            sub_config.to_str().unwrap(),
            "--unset",
            "core.worktree",
        ],
    );
    // 标签和它指向的提交都链到外面：读穿了，剥标签就得到那个带标记的提交
    for id in [&tag, &marked] {
        let planted = loose(&sub.join(".git"), id);
        std::fs::create_dir_all(planted.parent().unwrap()).unwrap();
        symlink(loose(&outside_git, id), &planted).unwrap();
    }
    std::fs::write(sub.join(".git/HEAD"), format!("{tag}\n")).unwrap();
    let root = RepoRoot(sup.clone());
    let base = f.vcs().baseline(&root).await.unwrap();

    let versions = f
        .vcs()
        .worktree_versions(&root, &paths(&["deps/lib"]))
        .await
        .unwrap();
    assert_eq!(versions[0], PathVersion::Unknown, "不该读出仓库外那个提交");
    let err = f
        .vcs()
        .diff(&root, DiffRange::WorkspaceSince(base))
        .await
        .unwrap_err();
    assert_refused(&err, SUBMODULE_DOT_GIT_DIR, &marked);
    let status = f.vcs().status(&root).await.unwrap();
    assert_no_marker(&status, &marked);
}
