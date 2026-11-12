//! `worktree_versions`（T10a 方案 D10、2.4）：路径在工作区里此刻的版本。临时目录里的真实 git 仓库。
//!
//! 口径和 `diff` 判两版相同一致；上级目录被换成链接的路径算不在、不读链接那一头（ADR-0010）。

#[allow(dead_code, reason = "共用的夹具，这里只用一部分")]
mod common;

use std::path::{Path, PathBuf};

use common::{Fixture, write};
use iwork_ports::vcs::{DiffRange, PathKind, PathVersion, RepoRoot, VcsProvider};
use sha2::{Digest, Sha256};

fn digest(bytes: &[u8]) -> [u8; 32] {
    Sha256::digest(bytes).into()
}

fn paths(names: &[&str]) -> Vec<PathBuf> {
    names.iter().map(PathBuf::from).collect()
}

/// 这次运行独有的标记：外面那个文件的内容只有它，读到了就能认出来。
fn marker() -> String {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    format!("T10A-K14-{}-{nanos}\n", std::process::id())
}

/// 7.2a 的 K14：`dir` 被换成指向仓库外的链接，外面同名的是带唯一标记的普通文件。`dir/file` 的版本严格是
/// `Absent`：不读外面那个文件（读了会得到带散列的 `Present`，读失败会是 `Unknown`，都不等于 `Absent`）。
#[cfg(unix)]
#[tokio::test]
async fn parent_symlink_to_external_regular_file_is_absent() {
    let f = Fixture::new();
    let repo = f.repo_with_commit("k14", &[("dir/file", "inside\n")]);
    let mark = marker();
    let outside = f.root().join("outside");
    write(&outside.join("file"), &mark);
    // 正向对照：同样的内容放在仓库里，读得出、散列就是标记的散列
    write(&repo.join("leaf"), &mark);
    std::fs::remove_dir_all(repo.join("dir")).unwrap();
    std::os::unix::fs::symlink(&outside, repo.join("dir")).unwrap();

    let got = f
        .vcs()
        .worktree_versions(&RepoRoot(repo), &paths(&["dir/file", "leaf"]))
        .await
        .unwrap();
    assert_eq!(got[0], PathVersion::Absent);
    assert_eq!(
        got[1],
        PathVersion::Present {
            kind: PathKind::File,
            digest: digest(mark.as_bytes()),
        },
        "对照：仓库里的同一份内容是读得出的"
    );
}

/// 补充：外面放的是命名管道。读穿了会一直卡在打开上，用例带时限。
#[cfg(unix)]
#[tokio::test]
async fn parent_dir_swapped_for_symlink_is_absent_and_not_read() {
    let f = Fixture::new();
    let repo = f.repo_with_commit("fifo", &[("dir/file", "inside\n")]);
    let outside = f.root().join("outside");
    std::fs::create_dir_all(&outside).unwrap();
    let status = std::process::Command::new("mkfifo")
        .arg(outside.join("file"))
        .status()
        .unwrap();
    assert!(status.success());
    std::fs::remove_dir_all(repo.join("dir")).unwrap();
    std::os::unix::fs::symlink(&outside, repo.join("dir")).unwrap();

    let got = tokio::time::timeout(
        std::time::Duration::from_secs(10),
        f.vcs()
            .worktree_versions(&RepoRoot(repo), &paths(&["dir/file"])),
    )
    .await
    .expect("没有卡在管道上")
    .unwrap();
    assert_eq!(got, [PathVersion::Absent]);
}

/// 同一组夹具上：一个路径两次的版本相等 ⇔ `diff` 不列它（种类、执行位、链接目标、内容、删除、新增、改了又改回）。
#[cfg(unix)]
#[tokio::test]
async fn worktree_versions_match_diff_equality() {
    use std::os::unix::fs::PermissionsExt;
    let f = Fixture::new();
    let repo = f.repo_with_commit(
        "eq",
        &[
            ("same.txt", "a\n"),
            ("mod.txt", "a\n"),
            ("exec.sh", "echo\n"),
            ("gone.txt", "g\n"),
            ("revert.txt", "r\n"),
            ("f2l.txt", "t\n"),
        ],
    );
    std::os::unix::fs::symlink("t1", repo.join("link")).unwrap();
    f.commit_all(&repo, "link");
    let root = RepoRoot(repo.clone());
    let base = f.vcs().baseline(&root).await.unwrap();
    let all = paths(&[
        "same.txt",
        "mod.txt",
        "exec.sh",
        "gone.txt",
        "revert.txt",
        "f2l.txt",
        "link",
        "new.txt",
    ]);
    let before = f.vcs().worktree_versions(&root, &all).await.unwrap();

    write(&repo.join("mod.txt"), "b\n");
    std::fs::set_permissions(repo.join("exec.sh"), std::fs::Permissions::from_mode(0o755)).unwrap();
    std::fs::remove_file(repo.join("gone.txt")).unwrap();
    write(&repo.join("revert.txt"), "r2\n");
    write(&repo.join("revert.txt"), "r\n");
    std::fs::remove_file(repo.join("f2l.txt")).unwrap();
    std::os::unix::fs::symlink("same.txt", repo.join("f2l.txt")).unwrap();
    std::fs::remove_file(repo.join("link")).unwrap();
    std::os::unix::fs::symlink("t2", repo.join("link")).unwrap();
    write(&repo.join("new.txt"), "n\n");
    let after = f.vcs().worktree_versions(&root, &all).await.unwrap();

    let diff = f
        .vcs()
        .diff(&root, DiffRange::WorkspaceSince(base))
        .await
        .unwrap();
    let listed = |p: &Path| diff.files.iter().any(|d| d.path == p);
    let mut unchanged = Vec::new();
    for (i, p) in all.iter().enumerate() {
        let same = before[i] == after[i];
        assert_eq!(
            same,
            !listed(p),
            "{}: {:?} → {:?}",
            p.display(),
            before[i],
            after[i]
        );
        if same {
            unchanged.push(p.display().to_string());
        }
    }
    // 对照：两个方向都有
    assert_eq!(unchanged, ["same.txt", "revert.txt"]);
    assert_eq!(before[7], PathVersion::Absent);
    assert_eq!(after[3], PathVersion::Absent);
    assert!(matches!(
        after[2],
        PathVersion::Present {
            kind: PathKind::ExecutableFile,
            ..
        }
    ));
    assert_eq!(
        after[6],
        PathVersion::Present {
            kind: PathKind::Symlink,
            digest: digest(b"t2"),
        }
    );
}

/// 要外部过滤程序的文件：不运行它，散列文件原样的字节；改了字节版本就变。
#[tokio::test]
async fn externally_filtered_file_hashes_raw_bytes() {
    let f = Fixture::new();
    let repo = f.repo_with_commit("rot", &[(".gitattributes", "*.r filter=rot\n")]);
    f.git(&repo, &["config", "filter.rot.clean", "tr a-z n-za-m"]);
    write(&repo.join("x.r"), "hello\n");
    f.commit_all(&repo, "rot");
    let root = RepoRoot(repo.clone());
    let one = paths(&["x.r"]);
    let before = f.vcs().worktree_versions(&root, &one).await.unwrap();
    assert_eq!(
        before,
        [PathVersion::Present {
            kind: PathKind::File,
            digest: digest(b"hello\n"),
        }]
    );
    write(&repo.join("x.r"), "uryyb\n");
    let after = f.vcs().worktree_versions(&root, &one).await.unwrap();
    assert_ne!(before, after);
}

/// 超过 16 MiB 的普通文件不读，记「不知道」；上限以内的照常读。
#[tokio::test]
async fn oversized_file_is_unknown() {
    let f = Fixture::new();
    let repo = f.repo_with_commit("big", &[("small.txt", "s\n")]);
    std::fs::write(repo.join("big.bin"), vec![0u8; 16 * 1024 * 1024 + 1]).unwrap();
    let got = f
        .vcs()
        .worktree_versions(&RepoRoot(repo), &paths(&["big.bin", "small.txt", "nope"]))
        .await
        .unwrap();
    assert_eq!(got[0], PathVersion::Unknown);
    assert_eq!(
        got[1],
        PathVersion::Present {
            kind: PathKind::File,
            digest: digest(b"s\n"),
        }
    );
    assert_eq!(got[2], PathVersion::Absent);
}
