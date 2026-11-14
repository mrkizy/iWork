//! 交给 gix 之前先查仓库的布局（T10a 方案 2.4a，步 2b）：后台读仓库时不顺着仓库里的东西读到仓库外面去。
//!
//! 每一格在仓库外摆一个东西，把仓库里的某一处指过去，各跑两遍（[`Outside`]）：
//! - **带标记**：外面的内容或名字里带这次运行独有的标记。断言拒绝的原因是那几种固定文字之一，返回值（含错误
//!   文字）里没有标记。
//! - **不可读**：外面那个东西改成没有读权限。拒绝必须发生在 gix 读它之前：先读了的实现会先撞上权限错误，报出来
//!   的就不是固定的拒绝原因（第 1 轮 F-6：只看「标记没出现」证明不了没读）。
//!
//! 只在 unix 上跑：要建符号链接、改权限、对拍真实文件系统；M1 不支持 Windows（T03）。
#![cfg(unix)]

#[allow(dead_code, reason = "共用的夹具，这里只用一部分")]
mod common;

use std::os::unix::fs::{PermissionsExt, symlink};
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
const POINTER_AMBIGUOUS: &str =
    ".git 文件或 commondir 的路径里链接后面跟着 ..，按字面和按实际解析不是同一个目录";

/// 仓库外那个东西的两种摆法，见文件头。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Outside {
    Marked,
    Unreadable,
}
const BOTH: [Outside; 2] = [Outside::Marked, Outside::Unreadable];

/// 这次运行独有的标记。
fn marker(tag: &str) -> String {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    format!("T10A-2B-{tag}-{}-{nanos}", std::process::id())
}

/// 把 `path` 的权限改成 000，离开作用域时改回来（不然临时目录删不掉）。
struct Hidden(PathBuf);

impl Drop for Hidden {
    fn drop(&mut self) {
        let _ = std::fs::set_permissions(&self.0, std::fs::Permissions::from_mode(0o755));
    }
}

/// `outside` 是 [`Outside::Unreadable`] 时把 `path` 改成不可读并确认真的读不到；否则什么也不做。
fn hide_if(outside: Outside, path: &Path) -> Option<Hidden> {
    if outside != Outside::Unreadable {
        return None;
    }
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o000)).unwrap();
    let guard = Hidden(path.to_owned());
    // 以 root 跑时权限挡不住读，这一格就证明不了顺序：让它失败，不悄悄跳过
    let readable = if path.is_dir() {
        std::fs::read_dir(path).is_ok()
    } else {
        std::fs::read(path).is_ok()
    };
    assert!(
        !readable,
        "{} 改成 000 后仍读得到（是不是以 root 跑？）",
        path.display()
    );
    Some(guard)
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

/// 有两个提交、HEAD 退回第一个的仓库；返回 (仓库, 第二个提交)。第二个提交的对象在仓库里，但 HEAD 不指它：
/// 读出它就说明后台跟着某个指到仓库外的东西走了。
fn repo_with_unreferenced_commit(f: &Fixture, name: &str) -> (PathBuf, String) {
    let repo = f.repo_with_commit(name, &[("a", "1\n")]);
    f.git(
        &repo,
        &["commit", "--quiet", "--allow-empty", "-m", "second"],
    );
    let second = f.git(&repo, &["rev-parse", "HEAD"]);
    f.git(&repo, &["reset", "--quiet", "--hard", "HEAD~1"]);
    (repo, second)
}

fn replace_with_symlink(path: &Path, target: &Path) {
    if path.is_dir() && !path.is_symlink() {
        std::fs::remove_dir_all(path).unwrap();
    } else {
        std::fs::remove_file(path).unwrap();
    }
    symlink(target, path).unwrap();
}

/// 7/F-2 第一格：`sub/.git` 是指向仓库外普通文件的链接。
#[tokio::test]
async fn submodule_dot_git_symlink_to_marked_file_is_refused() {
    for outside_kind in BOTH {
        let f = Fixture::new();
        let sup = super_with_submodule(&f);
        let mark = marker("subfile");
        let outside = f.root().join(format!("outside-{mark}"));
        write(&outside, &format!("{mark}\n"));
        replace_with_symlink(&sup.join("deps/lib/.git"), &outside);
        write(&sup.join("s"), "2\n");
        let root = RepoRoot(sup.clone());
        let base = f.vcs().baseline(&root).await.unwrap();
        let _hidden = hide_if(outside_kind, &outside);

        let err = f
            .vcs()
            .diff(&root, DiffRange::WorkspaceSince(base))
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
}

/// 7/F-2 第二格：`sub/.git` 链到仓库外一个合法的 git 目录，它的 HEAD 是带标记的提交。只断言「版本是
/// Unknown」不够（7.2a 的 K18）：要断言拒绝的原因，并且读出来的不是那个提交。
#[tokio::test]
async fn submodule_dot_git_symlink_to_git_dir_is_refused() {
    for outside_kind in BOTH {
        let f = Fixture::new();
        let sup = super_with_submodule(&f);
        let mark = marker("subdir");
        let (outside_git, marked) = outside_repo_at_marked_commit(&f, &mark);
        replace_with_symlink(&sup.join("deps/lib/.git"), &outside_git);
        let root = RepoRoot(sup.clone());
        let base = f.vcs().baseline(&root).await.unwrap();
        let _hidden = hide_if(outside_kind, &outside_git);

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
}

/// 子模块的 `.git` 是 gitfile、指到上层 `modules/` 之外的一个 git 目录 → 拒绝；对照：标准布局照常读出检出的提交。
#[tokio::test]
async fn submodule_gitfile_outside_modules_is_refused() {
    for outside_kind in BOTH {
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
        let _hidden = hide_if(outside_kind, &outside_git);

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
}

/// 项目根的 `.git` 是链接（指到仓库外一个 HEAD 带标记提交的 git 目录）→ 拒绝。
#[tokio::test]
async fn root_dot_git_symlink_is_refused() {
    for outside_kind in BOTH {
        let f = Fixture::new();
        let repo = f.repo_with_commit("proj", &[("a", "1\n")]);
        let mark = marker("root");
        let (outside_git, marked) = outside_repo_at_marked_commit(&f, &mark);
        replace_with_symlink(&repo.join(".git"), &outside_git);
        let root = RepoRoot(repo);
        let _hidden = hide_if(outside_kind, &outside_git);

        let err = f.vcs().baseline(&root).await.unwrap_err();
        assert_refused(&err, DOT_GIT_SYMLINK, &marked);
        assert_refused(&err, DOT_GIT_SYMLINK, &mark);
        let err = f.vcs().status(&root).await.unwrap_err();
        assert_refused(&err, DOT_GIT_SYMLINK, &mark);
    }
}

/// git 目录里的东西被换成链接：把真东西挪到仓库外（目录名带标记）再链回来。gix 会照常跟过去读，所以没有这一道
/// 检查时这几格都是「读成功」。
#[tokio::test]
async fn git_dir_entry_symlink_is_refused() {
    // 每格都跑完再一起断言，红灯时能看到每一格各自怎样
    let mut wrong = Vec::new();
    for outside_kind in BOTH {
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
            let _hidden = hide_if(outside_kind, &moved);

            match f.vcs().status(&root).await {
                Err(err) => {
                    let text = format!("{err}");
                    if !text.contains(METADATA_SYMLINK) || text.contains(&mark) {
                        wrong.push(format!(
                            "{outside_kind:?} {entry}：报的不是固定原因：{text}"
                        ));
                    }
                }
                Ok(status) => wrong.push(format!("{outside_kind:?} {entry}：读成功了 {status:?}")),
            }
        }
    }
    assert!(wrong.is_empty(), "这些链接没被拒绝：{wrong:#?}");
}

/// `refs/` 深处的一个引用是链接（HEAD 指向它，它链到仓库外一个带提交编号的文件）→ 拒绝。
#[tokio::test]
async fn ref_symlink_deep_in_tree_is_refused() {
    for outside_kind in BOTH {
        let f = Fixture::new();
        let (repo, second) = repo_with_unreferenced_commit(&f, "proj");
        let mark = marker("ref");
        let outside = f.root().join(format!("outside-{mark}"));
        write(&outside, &format!("{second}\n"));
        let deep = repo.join(".git/refs/heads/deep/er/x");
        std::fs::create_dir_all(deep.parent().unwrap()).unwrap();
        symlink(&outside, &deep).unwrap();
        std::fs::write(repo.join(".git/HEAD"), "ref: refs/heads/deep/er/x\n").unwrap();
        let root = RepoRoot(repo);
        let _hidden = hide_if(outside_kind, &outside);

        let err = f.vcs().baseline(&root).await.unwrap_err();
        assert_refused(&err, METADATA_SYMLINK, &second);
        assert_refused(&err, METADATA_SYMLINK, &mark);
    }
}

/// 第 1 轮 F-4：HEAD 写 `ref: worktrees/evil/HEAD`，gix-ref 把它映到共享目录的 `worktrees/evil/HEAD`（不在
/// `refs/` 里），那里是指到仓库外的链接 → 拒绝。
#[tokio::test]
async fn symbolic_head_through_worktrees_namespace_is_refused() {
    for outside_kind in BOTH {
        let f = Fixture::new();
        let (repo, second) = repo_with_unreferenced_commit(&f, "proj");
        let mark = marker("wtref");
        let outside = f.root().join(format!("outside-{mark}"));
        write(&outside, &format!("{second}\n"));
        let evil = repo.join(".git/worktrees/evil");
        std::fs::create_dir_all(&evil).unwrap();
        symlink(&outside, evil.join("HEAD")).unwrap();
        std::fs::write(repo.join(".git/HEAD"), "ref: worktrees/evil/HEAD\n").unwrap();
        let root = RepoRoot(repo);
        let _hidden = hide_if(outside_kind, &outside);

        let err = f.vcs().baseline(&root).await.unwrap_err();
        assert_refused(&err, METADATA_SYMLINK, &second);
        assert_refused(&err, METADATA_SYMLINK, &mark);
    }
}

/// 第 1 轮 F-1：gitfile 写「实际目录 + 空格」。gix 解析时去掉末尾空白、打开实际目录；旁边另放一个名字末尾带空格
/// 的空目录。预检和 gix 用同一种解析，查的就是实际目录，那里的 HEAD 是指到仓库外的链接 → 拒绝。
#[tokio::test]
async fn gitfile_trailing_space_is_parsed_like_gix() {
    for outside_kind in BOTH {
        let f = Fixture::new();
        let (repo, second) = repo_with_unreferenced_commit(&f, "proj");
        let actual = f.root().join("actual-gitdir");
        std::fs::rename(repo.join(".git"), &actual).unwrap();
        std::fs::create_dir_all(f.root().join("actual-gitdir ")).unwrap();
        write(
            &repo.join(".git"),
            &format!("gitdir: {} \n", actual.display()),
        );
        let mark = marker("gitfile-ws");
        let outside = f.root().join(format!("outside-{mark}"));
        write(&outside, &format!("{second}\n"));
        replace_with_symlink(&actual.join("HEAD"), &outside);
        let root = RepoRoot(repo);
        let _hidden = hide_if(outside_kind, &outside);

        let err = f.vcs().baseline(&root).await.unwrap_err();
        assert_refused(&err, METADATA_SYMLINK, &second);
        assert_refused(&err, METADATA_SYMLINK, &mark);
    }
}

/// 第 1 轮 F-1 的 `commondir` 那一半：链接的工作树里 `commondir` 写「共享目录 + 空格」，旁边另放名字末尾带空格的
/// 空目录。共享目录里工作树的分支引用是指到仓库外的链接 → 拒绝。
#[tokio::test]
async fn commondir_trailing_space_is_parsed_like_gix() {
    for outside_kind in BOTH {
        let f = Fixture::new();
        let (main, second) = repo_with_unreferenced_commit(&f, "proj");
        let wt = f.root().join("linked");
        f.git(
            &main,
            &[
                "worktree",
                "add",
                "--quiet",
                "-b",
                "side",
                wt.to_str().unwrap(),
            ],
        );
        let common = main.join(".git");
        std::fs::write(
            common.join("worktrees/linked/commondir"),
            format!("{} \n", common.display()),
        )
        .unwrap();
        std::fs::create_dir_all(main.join(".git ")).unwrap();
        let mark = marker("commondir-ws");
        let outside = f.root().join(format!("outside-{mark}"));
        write(&outside, &format!("{second}\n"));
        replace_with_symlink(&common.join("refs/heads/side"), &outside);
        let root = RepoRoot(wt);
        let _hidden = hide_if(outside_kind, &outside);

        let err = f.vcs().baseline(&root).await.unwrap_err();
        assert_refused(&err, METADATA_SYMLINK, &second);
        assert_refused(&err, METADATA_SYMLINK, &mark);
    }
}

/// 第 1 轮 F-2：`.git` 是空目录（gix 认不出），项目根自己摆成 git 目录的样子，根下的 `HEAD` 链到仓库外。gix 不得
/// 退回去把根目录当 git 目录打开（根下的东西没查过）。
#[tokio::test]
async fn empty_dot_git_does_not_fall_back_to_root() {
    for outside_kind in BOTH {
        let f = Fixture::new();
        let (src, second) = repo_with_unreferenced_commit(&f, "src");
        let proj = f.root().join("proj");
        std::fs::create_dir_all(&proj).unwrap();
        let status = std::process::Command::new("cp")
            .arg("-R")
            .arg(format!("{}/.", src.join(".git").display()))
            .arg(&proj)
            .status()
            .unwrap();
        assert!(status.success());
        std::fs::create_dir_all(proj.join(".git")).unwrap();
        f.git(
            &proj,
            &[
                "--git-dir",
                proj.to_str().unwrap(),
                "config",
                "core.worktree",
                proj.to_str().unwrap(),
            ],
        );
        let mark = marker("fallback");
        let outside = f.root().join(format!("outside-{mark}"));
        write(&outside, &format!("{second}\n"));
        replace_with_symlink(&proj.join("HEAD"), &outside);
        let root = RepoRoot(proj);
        let _hidden = hide_if(outside_kind, &outside);

        let result = f.vcs().baseline(&root).await;
        let err = result.expect_err("空的 .git 不是仓库，不能退回去开根目录");
        let text = format!("{err} / {err:?}");
        assert!(
            !text.contains(&second),
            "不该读出根下 HEAD 指的提交：{text}"
        );
        assert!(!text.contains(&mark), "{text}");
        // 被「打开的不是查过的那个目录」挡下，说明 gix 已经退回去开了根目录、读过根下的 HEAD：开完再核只是
        // 额外断言，挡不住读（变异「打开根目录而不是 <根>/.git」）
        assert!(
            !text.contains("打开的 git 目录和检查过的不是同一个"),
            "gix 不该退回去开没查过的根目录：{text}"
        );
        // 先读了根下 HEAD 的实现在不可读那一格会撞上权限错误
        assert!(
            !text.contains("ermission denied"),
            "不该去读根下的 HEAD：{text}"
        );
    }
}

/// 第 1 轮 F-3：仓库配置里 `gitoxide.core.indexFile` 指到仓库外另一个仓库的索引（里面记着一个带标记名字的文件）
/// → 不读，照常用自己的索引。
#[tokio::test]
async fn configured_external_index_is_not_read() {
    for outside_kind in BOTH {
        let f = Fixture::new();
        let mark = marker("index");
        let other = f.repo_with_commit("other", &[(&format!("OUTSIDE-{mark}"), "x\n")]);
        let outside = f.root().join(format!("outside-{mark}.index"));
        std::fs::copy(other.join(".git/index"), &outside).unwrap();
        let repo = f.repo_with_commit("proj", &[("a", "1\n")]);
        f.git(
            &repo,
            &[
                "config",
                "gitoxide.core.indexFile",
                outside.to_str().unwrap(),
            ],
        );
        let _hidden = hide_if(outside_kind, &outside);

        let status = f.vcs().status(&RepoRoot(repo)).await.unwrap();
        assert_no_marker(&status, &mark);
        assert!(
            status.changed.is_empty(),
            "用自己的索引，没有改动：{status:?}"
        );
    }
}

/// 第 1 轮 F-5：multi-pack-index 里的包名改成等长的 `../../../../OUTSIDE_MIDX_MARKER_…idx`（穿出
/// `objects/pack/`），外面放一份同名的 `.pack`。
/// - 带标记：照 reviewer 的造法，把仓库里的包挪到外面（`.idx` 留着）。用 MIDX 的实现读外面的包、读得出提交；
///   不用 MIDX 的实现在仓库里找不到包，读不出。
/// - 不可读：仓库里的包留着，外面放一份不可读的副本。用 MIDX 的实现撞上权限错误；不用的照常读出提交。
#[tokio::test]
async fn midx_pack_name_cannot_escape_objects_pack() {
    const ESCAPED: &str = "../../../../OUTSIDE_MIDX_MARKER_xxxxxxxxxxxxx.idx";
    for outside_kind in BOTH {
        let f = Fixture::new();
        let repo = f.repo_with_commit("proj", &[("a", "1\n")]);
        let head = f.git(&repo, &["rev-parse", "HEAD"]);
        f.git(&repo, &["gc", "--quiet"]);
        f.git(&repo, &["multi-pack-index", "write"]);
        let pack_dir = repo.join(".git/objects/pack");
        let midx = pack_dir.join("multi-pack-index");
        let pack = std::fs::read_dir(&pack_dir)
            .unwrap()
            .map(|e| e.unwrap().path())
            .find(|p| p.extension().is_some_and(|x| x == "pack"))
            .expect("gc 之后有 pack 文件");
        let idx_name = pack.with_extension("idx");
        let idx_name = idx_name.file_name().unwrap().to_str().unwrap();
        assert_eq!(idx_name.len(), ESCAPED.len(), "等长替换");
        let mut bytes = std::fs::read(&midx).unwrap();
        let at = bytes
            .windows(idx_name.len())
            .position(|w| w == idx_name.as_bytes())
            .expect("MIDX 的 PNAM 里有包名");
        bytes[at..at + ESCAPED.len()].copy_from_slice(ESCAPED.as_bytes());
        std::fs::write(&midx, bytes).unwrap();
        // pack_dir 往上四级就是夹具根
        let outside = f.root().join("OUTSIDE_MIDX_MARKER_xxxxxxxxxxxxx.pack");
        let root = RepoRoot(repo);

        match outside_kind {
            Outside::Marked => {
                std::fs::rename(&pack, &outside).unwrap();
                let got = f.vcs().baseline(&root).await;
                assert!(
                    !matches!(&got, Ok(b) if b.0 == head),
                    "不该从 objects/pack 之外的包里读出提交：{got:?}"
                );
            }
            Outside::Unreadable => {
                std::fs::copy(&pack, &outside).unwrap();
                let _hidden = hide_if(outside_kind, &outside);
                let got = f.vcs().baseline(&root).await.unwrap();
                assert_eq!(got.0, head, "用仓库里的包照常读出提交");
            }
        }
    }
}

/// `config` 的 `include.path` 指到仓库外的文件，文件里设一个看得出效果的配置（`core.fileMode = false`：
/// 执行位的变化就不算改动）→ 打开成功，那个配置没生效。
#[tokio::test]
async fn config_include_is_not_followed() {
    for outside_kind in BOTH {
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
        std::fs::set_permissions(&a, std::fs::Permissions::from_mode(0o755)).unwrap();
        let _hidden = hide_if(outside_kind, &included);

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
}

/// 对照：仓库自己 `config` 里影响状态的设置（`core.fileMode = false`）照常生效。仓库来源的配置只是不过 gix 的
/// 信任闸，不是整份不加载。
#[tokio::test]
async fn repo_local_status_settings_still_apply() {
    let f = Fixture::new();
    let repo = f.repo_with_commit("proj", &[("a", "1\n")]);
    f.git(&repo, &["config", "core.fileMode", "false"]);
    let a = repo.join("a");
    std::fs::set_permissions(&a, std::fs::Permissions::from_mode(0o755)).unwrap();

    let status = f.vcs().status(&RepoRoot(repo)).await.unwrap();
    assert!(
        status.changed.is_empty(),
        "fileMode = false 时执行位变化不算改动：{status:?}"
    );
}

/// 仓库自己 `config` 里的 `core.excludesFile` 指到仓库外的文件（里面一条忽略规则）→ 不读，规则不生效。
#[tokio::test]
async fn repo_excludes_file_is_not_read() {
    for outside_kind in BOTH {
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
        let _hidden = hide_if(outside_kind, &outside);

        let status = f.vcs().status(&RepoRoot(repo)).await.unwrap();
        assert_no_marker(&status, &mark);
        assert_eq!(untracked(&status), paths(&["hidden.txt"]));
    }
}

/// 仓库自己 `config` 里的 `core.attributesFile` 指到仓库外的文件（给 `f.txt` 配外部过滤器）→ 不读，
/// `f.txt` 照常比较内容，不会被标成「要外部过滤程序」。
#[tokio::test]
async fn repo_attributes_file_is_not_read() {
    for outside_kind in BOTH {
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
        let _hidden = hide_if(outside_kind, &outside);

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
}

/// `info/exclude` 被换成指向仓库外文件的链接 → 拒绝。
#[tokio::test]
async fn info_exclude_symlink_is_refused() {
    for outside_kind in BOTH {
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
        let _hidden = hide_if(outside_kind, &outside);

        let err = f.vcs().status(&RepoRoot(repo)).await.unwrap_err();
        assert_refused(&err, METADATA_SYMLINK, &mark);
    }
}

/// 第 2 轮 F-1：走树时不往下走的 `hooks`、`logs`、`lfs` 本身是链接 → 拒绝，不管指向文件还是目录。
/// 指向文件时，大小写不敏感的文件系统上 `ref: LFS` 这种全大写伪引用会被 gix 开到 `.git/lfs`、顺着链接读出去
/// （gix-ref name.rs 的伪引用规则，find.rs 落在 git 目录下）；指向目录（常见：钩子目录链到共享的地方）也拒，
/// 代价记在 KA2。
#[tokio::test]
async fn skipped_entry_symlink_is_refused() {
    for entry in ["lfs", "hooks", "logs"] {
        for outside_kind in BOTH {
            let f = Fixture::new();
            let (repo, second) = repo_with_unreferenced_commit(&f, "proj");
            let mark = marker(entry);
            let outside = f.root().join(format!("outside-{mark}"));
            write(&outside, &format!("{second}\n"));
            let link = repo.join(".git").join(entry);
            let _ = std::fs::remove_dir_all(&link);
            symlink(&outside, &link).unwrap();
            std::fs::write(
                repo.join(".git/HEAD"),
                format!("ref: {}\n", entry.to_uppercase()),
            )
            .unwrap();
            let _hidden = hide_if(outside_kind, &outside);

            let err = f.vcs().baseline(&RepoRoot(repo)).await.unwrap_err();
            assert_refused(&err, METADATA_SYMLINK, &second);
            assert_refused(&err, METADATA_SYMLINK, &mark);
        }
    }

    // 只用 git 命令也摆得出来：模板里的 lfs 是链接，`git init --template` 把它抄进 .git，分支指到 LFS
    let f = Fixture::new();
    let (repo, second) = repo_with_unreferenced_commit(&f, "proj");
    let outside = f.root().join("outside-template");
    write(&outside, &format!("{second}\n"));
    let tpl = repo.join("tpl");
    std::fs::create_dir_all(&tpl).unwrap();
    symlink(&outside, tpl.join("lfs")).unwrap();
    f.git(
        &repo,
        &["init", "--quiet", &format!("--template={}", tpl.display())],
    );
    assert!(
        repo.join(".git/lfs").is_symlink(),
        "git init --template 把链接抄进了 .git"
    );
    f.git(&repo, &["symbolic-ref", "refs/heads/foo", "LFS"]);
    f.git(&repo, &["symbolic-ref", "HEAD", "refs/heads/foo"]);
    let err = f.vcs().baseline(&RepoRoot(repo)).await.unwrap_err();
    assert_refused(&err, METADATA_SYMLINK, &second);

    // 链到目录的也拒
    let f = Fixture::new();
    let repo = f.repo_with_commit("proj", &[("a", "1\n")]);
    let shared = f.root().join("shared-hooks");
    std::fs::create_dir_all(&shared).unwrap();
    replace_with_symlink(&repo.join(".git/hooks"), &shared);
    let err = f.vcs().status(&RepoRoot(repo)).await.unwrap_err();
    assert_refused(&err, METADATA_SYMLINK, "shared-hooks");
}

/// 第 2 轮 F-2：gitfile 写 `<基>/link/../lib`，`link` 是链接。取真实路径是顺着 link 走再 `..`，gix 对没有
/// commondir 的 gitfile 目标按字面消掉 `link/..`，两边指的不是同一个目录 → 拒绝，不交给 gix。
/// 字面那一头是工作区里摆的假 git 目录，它的 `config` 链到仓库外；仓库外的内容写成读了就会改变结果的配置
/// （`repositoryformatversion = 7` 让打开失败），读没读一看报错就知道。
#[tokio::test]
async fn gitfile_dotdot_after_symlink_is_refused() {
    for outside_kind in BOTH {
        for at in ["submodule", "root"] {
            let f = Fixture::new();
            let mark = marker("dotdot");
            let outside = f.root().join(format!("outside-{mark}"));
            write(
                &outside,
                &format!("# {mark}\n[core]\n\trepositoryformatversion = 7\n"),
            );
            // 摆一个没查过的 git 目录：HEAD、objects/、refs/ 是真的，config 链到仓库外
            let fake = |dir: &Path| {
                std::fs::create_dir_all(dir.join("objects")).unwrap();
                std::fs::create_dir_all(dir.join("refs")).unwrap();
                write(&dir.join("HEAD"), "ref: refs/heads/main\n");
                symlink(&outside, dir.join("config")).unwrap();
            };
            let (root, op_is_diff) = match at {
                "submodule" => {
                    let sup = super_with_submodule(&f);
                    let base = sup.join("evilbase");
                    std::fs::create_dir_all(&base).unwrap();
                    symlink(sup.join(".git/modules/deps/lib"), base.join("link")).unwrap();
                    fake(&base.join("lib"));
                    std::fs::write(
                        sup.join("deps/lib/.git"),
                        "gitdir: ../../evilbase/link/../lib\n",
                    )
                    .unwrap();
                    (sup, true)
                }
                _ => {
                    let repo = f.repo_with_commit("proj", &[("a", "1\n")]);
                    // 真 git 目录挪到 <夹具>/actual/.git；base/link 链到 actual/x，取真实路径走 link/.. 回到 actual
                    let actual = f.root().join("actual");
                    std::fs::create_dir_all(actual.join("x")).unwrap();
                    std::fs::rename(repo.join(".git"), actual.join(".git")).unwrap();
                    let base = repo.join("base");
                    std::fs::create_dir_all(&base).unwrap();
                    symlink(actual.join("x"), base.join("link")).unwrap();
                    fake(&base.join(".git"));
                    write(&repo.join(".git"), "gitdir: base/link/../.git\n");
                    (repo, false)
                }
            };
            let root = RepoRoot(root);
            let base = if op_is_diff {
                Some(f.vcs().baseline(&root).await.unwrap())
            } else {
                None
            };
            let _hidden = hide_if(outside_kind, &outside);

            let err = match base {
                Some(base) => f
                    .vcs()
                    .diff(&root, DiffRange::WorkspaceSince(base))
                    .await
                    .unwrap_err(),
                None => f.vcs().baseline(&root).await.unwrap_err(),
            };
            assert_refused(&err, POINTER_AMBIGUOUS, &mark);
        }
    }
}

/// 第 2 轮 F-4：子模块里 `git worktree add` 出的链接工作树。共享 config 里有 git 给子模块写的
/// `core.worktree`，git 与 gix 对链接工作树都不认它；只比配置文件在这个 git 目录之下的那一份，不误拒。
#[tokio::test]
async fn linked_worktree_of_submodule_works() {
    let f = Fixture::new();
    let sup = super_with_submodule(&f);
    let sub = sup.join("deps/lib");
    let wt = f.root().join("sub-linked");
    f.git(
        &sub,
        &[
            "worktree",
            "add",
            "--quiet",
            "-b",
            "side",
            wt.to_str().unwrap(),
        ],
    );
    write(&wt.join("l"), "2\n");

    let status = f.vcs().status(&RepoRoot(wt)).await.unwrap();
    assert!(
        status
            .changed
            .iter()
            .any(|c| c.path == Path::new("l") && c.change == ChangeKind::Modified),
        "{status:?}"
    );
}

/// `objects/info/alternates` 指到另一个仓库（`git clone --shared`）→ 拒绝，原因是「不支持」。
#[tokio::test]
async fn alternates_is_unsupported() {
    for outside_kind in BOTH {
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
        let _hidden = hide_if(outside_kind, &src.join(".git/objects"));

        let err = f.vcs().baseline(&root).await.unwrap_err();
        assert_refused(&err, ALTERNATES_UNSUPPORTED, &mark);
        let err = f.vcs().status(&root).await.unwrap_err();
        assert_refused(&err, ALTERNATES_UNSUPPORTED, &mark);
    }
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
    for outside_kind in BOTH {
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
        let _hidden: Vec<_> = [&tag, &marked]
            .into_iter()
            .filter_map(|id| hide_if(outside_kind, &loose(&outside_git, id)))
            .collect();

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
}

fn changed(status: &iwork_ports::vcs::WorkspaceStatus) -> Vec<String> {
    let mut out: Vec<String> = status
        .changed
        .iter()
        .map(|c| format!("{}:{:?}", c.path.display(), c.change))
        .collect();
    out.sort();
    out
}

/// 第 2 轮 F-3：status 列未跟踪文件时，gix 不再对未跟踪目录探 `<目录>/.git`（它跟着链接与 gitfile 读到仓库外）。
/// 四种摆法：真目录里的 HEAD 链到仓库外；`.git` 链到仓库外的目录；gitfile 指到仓库外；`.git` 链到仓库外的文件。
/// 下面有名叫 `.git` 的目录一律收成一行 `x`，结果不随仓库外读不读得到而变。
#[tokio::test]
async fn untracked_dir_dot_git_is_not_probed() {
    let mut wrong = Vec::new();
    for layout in [
        "dir-head-link",
        "dotgit-link-to-dir",
        "gitfile",
        "dotgit-link-to-file",
    ] {
        for outside_kind in BOTH {
            let f = Fixture::new();
            let repo = f.repo_with_commit("proj", &[("a", "1\n")]);
            let x = repo.join("x");
            write(&x.join("f"), "f\n");
            let mark = marker("probe");
            let outside = f.root().join(format!("outside-{mark}"));
            let outside_file = outside.join("file");
            write(&outside_file, "ref: refs/heads/main\n");
            let outside_gitdir = outside.join("gitdir");
            std::fs::create_dir_all(outside_gitdir.join("objects")).unwrap();
            std::fs::create_dir_all(outside_gitdir.join("refs")).unwrap();
            write(&outside_gitdir.join("HEAD"), "ref: refs/heads/main\n");
            let probed = match layout {
                "dir-head-link" => {
                    std::fs::create_dir_all(x.join(".git/objects")).unwrap();
                    std::fs::create_dir_all(x.join(".git/refs")).unwrap();
                    symlink(&outside_file, x.join(".git/HEAD")).unwrap();
                    outside_file.clone()
                }
                "dotgit-link-to-dir" => {
                    symlink(&outside_gitdir, x.join(".git")).unwrap();
                    outside_gitdir.join("HEAD")
                }
                "gitfile" => {
                    write(
                        &x.join(".git"),
                        &format!("gitdir: {}\n", outside_gitdir.display()),
                    );
                    outside_gitdir.join("HEAD")
                }
                _ => {
                    write(
                        &outside_file,
                        &format!("gitdir: {}\n", outside_gitdir.display()),
                    );
                    symlink(&outside_file, x.join(".git")).unwrap();
                    outside_file.clone()
                }
            };
            let _hidden = hide_if(outside_kind, &probed);

            match f.vcs().status(&RepoRoot(repo)).await {
                Ok(status) => {
                    let got = changed(&status);
                    if got != ["x:Untracked"] || format!("{status:?}").contains(&mark) {
                        wrong.push(format!("{layout} {outside_kind:?}：{got:?}"));
                    }
                }
                Err(e) => wrong.push(format!("{layout} {outside_kind:?}：{e}")),
            }
        }
    }
    assert!(
        wrong.is_empty(),
        "结果随仓库外变了，或没收成一行：{wrong:#?}"
    );
}

/// 对照：子模块（暂存区里是 gitlink）里的文件不列成未跟踪；检出的提交变了照常报改动。
#[tokio::test]
async fn submodule_contents_are_not_listed_as_untracked() {
    let f = Fixture::new();
    let sup = super_with_submodule(&f);
    let sub = sup.join("deps/lib");
    f.git(
        &sub,
        &[
            "-c",
            "user.name=T",
            "-c",
            "user.email=t@example.invalid",
            "commit",
            "--quiet",
            "--allow-empty",
            "-m",
            "bump",
        ],
    );
    write(&sub.join("dirty"), "x\n");

    let status = f.vcs().status(&RepoRoot(sup)).await.unwrap();
    assert_eq!(changed(&status), ["deps/lib:Modified"]);
}

/// 对照：工作区里未跟踪的嵌套仓库收成一行，和 `git status` 的 `?? vendor/dep/` 一致。
#[tokio::test]
async fn nested_untracked_repository_is_one_entry() {
    let f = Fixture::new();
    let repo = f.repo_with_commit("proj", &[("a", "1\n")]);
    let nested = repo.join("vendor/dep");
    std::fs::create_dir_all(&nested).unwrap();
    f.git(&nested, &["init", "--quiet"]);
    write(&nested.join("n1"), "x\n");
    write(&nested.join("sub/n2"), "x\n");
    write(&repo.join("plain/p"), "x\n");

    let status = f.vcs().status(&RepoRoot(repo.clone())).await.unwrap();
    assert_eq!(
        changed(&status),
        ["plain/p:Untracked", "vendor/dep:Untracked"]
    );
    let git = f.git(&repo, &["status", "--porcelain", "--untracked-files=all"]);
    assert!(git.contains("?? vendor/dep/"), "git 自己也只列一行：{git}");
}

/// 上一格只看结果：收成一行之后，探没探结果都一样，抓不到「还在探」。这一格把 gix 探的时候会打开的那个文件换成
/// 命名管道：一打开就卡住。status 在时限内回来，就说明没去开它。
#[tokio::test]
async fn untracked_dir_dot_git_is_not_opened() {
    let mut wrong = Vec::new();
    for layout in ["dir-head-link", "dotgit-link-to-dir", "gitfile"] {
        let f = Fixture::new();
        let repo = f.repo_with_commit("proj", &[("a", "1\n")]);
        let x = repo.join("x");
        write(&x.join("f"), "f\n");
        let outside = f.root().join("outside");
        let outside_gitdir = outside.join("gitdir");
        std::fs::create_dir_all(outside_gitdir.join("objects")).unwrap();
        std::fs::create_dir_all(outside_gitdir.join("refs")).unwrap();
        let fifo = match layout {
            "dir-head-link" => {
                std::fs::create_dir_all(x.join(".git/objects")).unwrap();
                std::fs::create_dir_all(x.join(".git/refs")).unwrap();
                let fifo = outside.join("head-fifo");
                symlink(&fifo, x.join(".git/HEAD")).unwrap();
                fifo
            }
            "dotgit-link-to-dir" => {
                symlink(&outside_gitdir, x.join(".git")).unwrap();
                outside_gitdir.join("HEAD")
            }
            _ => {
                write(
                    &x.join(".git"),
                    &format!("gitdir: {}\n", outside_gitdir.display()),
                );
                outside_gitdir.join("HEAD")
            }
        };
        let made = std::process::Command::new("mkfifo")
            .arg(&fifo)
            .status()
            .unwrap();
        assert!(made.success());

        let got = tokio::time::timeout(
            std::time::Duration::from_secs(20),
            f.vcs().status(&RepoRoot(repo)),
        )
        .await;
        match got {
            Ok(Ok(status)) if changed(&status) == ["x:Untracked"] => {}
            Ok(other) => wrong.push(format!("{layout}：{other:?}")),
            Err(_) => {
                wrong.push(format!("{layout}：卡在仓库外的管道上，说明去开了它"));
                // 让卡在读端的阻塞线程拿到 EOF 退出，测试进程才收得了尾
                let fifo = fifo.clone();
                std::thread::spawn(move || {
                    drop(std::fs::OpenOptions::new().write(true).open(fifo))
                });
            }
        }
    }
    assert!(wrong.is_empty(), "{wrong:#?}");
}

/// 第 3 轮 F-1：已跟踪目录 `t/`（暂存区里有 `t/a`）里出现名叫 `.git` 的东西（空文件、空目录、`git init`），
/// 下面新加的未跟踪文件照常列出，和 `git status` 对拍；diff 里也有它们。
#[tokio::test]
async fn tracked_dir_with_dot_git_still_lists_new_files_like_git() {
    for dot_git in ["empty-file", "empty-dir", "git-init"] {
        let f = Fixture::new();
        let repo = f.repo_with_commit("proj", &[("t/a", "1\n")]);
        let t = repo.join("t");
        match dot_git {
            "empty-file" => write(&t.join(".git"), ""),
            "empty-dir" => std::fs::create_dir_all(t.join(".git")).unwrap(),
            _ => {
                f.git(&t, &["init", "--quiet"]);
            }
        }
        let root = RepoRoot(repo.clone());
        let base = f.vcs().baseline(&root).await.unwrap();
        write(&t.join("a"), "2\n");
        write(&t.join("new"), "n\n");
        write(&t.join("sub/new2"), "n\n");

        let status = f.vcs().status(&root).await.unwrap();
        let porcelain = f.git(&repo, &["status", "--porcelain", "--untracked-files=all"]);
        let mut git: Vec<String> = porcelain
            .lines()
            .map(|l| {
                // f.git 去掉了整段输出首尾的空白，第一行开头的空格可能没了，按第一个空格切
                let (code, path) = l.trim_start().split_once(' ').unwrap();
                let kind = match code {
                    "M" => "Modified",
                    "??" => "Untracked",
                    other => panic!("没料到的 git 状态 {other}"),
                };
                format!("{}:{kind}", path.trim().trim_end_matches('/'))
            })
            .collect();
        git.sort();
        assert_eq!(changed(&status), git, "{dot_git}：和 git status 不一致");
        assert!(
            git.contains(&"t/new:Untracked".to_owned()),
            "{dot_git}：{git:?}"
        );

        let diff = f
            .vcs()
            .diff(&root, DiffRange::WorkspaceSince(base))
            .await
            .unwrap();
        let diffed: Vec<_> = diff.files.iter().map(|d| d.path.clone()).collect();
        for p in ["t/a", "t/new", "t/sub/new2"] {
            assert!(
                diffed.contains(&PathBuf::from(p)),
                "{dot_git}：diff 里没有 {p}：{diffed:?}"
            );
        }
    }
}
