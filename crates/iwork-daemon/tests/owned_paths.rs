//! 路径归属的共用函数 `scripts/lib/owned-paths.sh`（T04p 审查 1/F-1、F-2、F-7）。装配与演练都靠它，
//! 这里用 macOS 自带的 bash 3.2（别的平台用 PATH 里的 bash）直接 source 它，逐条验规则。
#![cfg(unix)]

use std::path::{Path, PathBuf};
use std::process::{Command, Output};

const BASH: &str = if cfg!(target_os = "macos") {
    "/bin/bash"
} else {
    "bash"
};

fn lib() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../scripts/lib/owned-paths.sh")
        .canonicalize()
        .unwrap()
}

/// 在 source 了 owned-paths.sh 的 bash 里跑一段脚本；$1 是测试的临时目录。
fn sh(dir: &Path, script: &str) -> Output {
    Command::new(BASH)
        .arg("-c")
        .arg(format!(
            "set -euo pipefail; source '{}'; {script}",
            lib().display()
        ))
        .arg("sh")
        .arg(dir)
        .env("LANG", "en_US.UTF-8")
        .env("LC_ALL", "en_US.UTF-8")
        .output()
        .unwrap()
}

fn ok(out: &Output) -> String {
    assert!(
        out.status.success(),
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).into_owned()
}

fn tmp() -> (tempfile::TempDir, PathBuf) {
    let t = tempfile::tempdir().unwrap();
    let p = t.path().canonicalize().unwrap();
    (t, p)
}

/// F-7：按规范化后的目录边界判，不看子串；经过父链接的路径与真实路径一样判。
#[test]
fn under_dir_uses_directory_boundaries() {
    let (_t, d) = tmp();
    std::fs::create_dir_all(d.join("root/sub")).unwrap();
    std::fs::create_dir_all(d.join("rootish")).unwrap();
    std::fs::create_dir_all(d.join(format!("unexpected{}", d.join("root").display()))).unwrap();
    std::os::unix::fs::symlink(d.join("root"), d.join("alias")).unwrap();
    let verdict = |path: &str| {
        let out = sh(
            &d,
            &format!(r#"under_dir "$1/{path}" "$1/root" && echo in || echo out"#),
        );
        ok(&out).trim().to_owned()
    };
    assert_eq!(verdict("root/sub/iwork.db"), "in");
    assert_eq!(
        verdict("alias/sub/iwork.db"),
        "in",
        "经过链接，真实路径在 root 下"
    );
    assert_eq!(verdict("rootish/iwork.db"), "out", "前缀相同、目录不同");
    assert_eq!(
        verdict(&format!("unexpected{}/iwork.db", d.join("root").display())),
        "out",
        "D 只是子串"
    );
    assert_eq!(verdict("root"), "out", "根本身不算在它之下");
}

/// 只删登记过的；没登记的路径删不了，原样留着。
#[test]
fn owned_rm_only_removes_owned() {
    let (_t, d) = tmp();
    std::fs::create_dir_all(d.join("theirs")).unwrap();
    std::fs::write(d.join("theirs/keep.txt"), "keep").unwrap();
    let out = sh(&d, r#"owned_rm "$1/theirs""#);
    assert!(!out.status.success());
    assert!(d.join("theirs/keep.txt").exists());
    // 对照：自己建的能删
    ok(&sh(
        &d,
        r#"owned_mkdir "$1/mine"; owned_rm "$1/mine"; [ ! -e "$1/mine" ]"#,
    ));
}

/// 目标已经有东西（目录、文件、悬空链接）→ 拒绝；所在目录是链接 → 拒绝。
#[test]
fn dest_free_refuses_existing_and_linked_parents() {
    let (_t, d) = tmp();
    std::fs::create_dir_all(d.join("real")).unwrap();
    std::fs::write(d.join("real/file"), "x").unwrap();
    std::os::unix::fs::symlink(d.join("nowhere"), d.join("real/dangling")).unwrap();
    std::os::unix::fs::symlink(d.join("real"), d.join("linked")).unwrap();
    for target in ["real/file", "real/dangling", "linked/new"] {
        let out = sh(&d, &format!(r#"dest_free "$1/{target}""#));
        assert!(!out.status.success(), "{target} 应被拒");
    }
    ok(&sh(&d, r#"dest_free "$1/real/new""#));
}

/// 认领目录：不存在就建并打标记；带标记的认；空的真目录认并打标记；链接、有别人东西的目录不认。
#[test]
fn claim_dir_rules() {
    let (_t, d) = tmp();
    ok(&sh(
        &d,
        r#"claim_dir "$1/new"; [ -f "$1/new/.iwork-owned" ]"#,
    ));
    ok(&sh(&d, r#"claim_dir "$1/new""#));
    std::fs::create_dir_all(d.join("empty")).unwrap();
    ok(&sh(
        &d,
        r#"claim_dir "$1/empty"; [ -f "$1/empty/.iwork-owned" ]"#,
    ));
    std::fs::create_dir_all(d.join("foreign")).unwrap();
    std::fs::write(d.join("foreign/x"), "x").unwrap();
    assert!(!sh(&d, r#"claim_dir "$1/foreign""#).status.success());
    std::os::unix::fs::symlink(d.join("new"), d.join("link")).unwrap();
    assert!(!sh(&d, r#"claim_dir "$1/link""#).status.success());
    assert!(
        !d.join("foreign/.iwork-owned").exists(),
        "不认的目录里什么都没写"
    );
}

/// 改名：两端不在同一个文件系统上 → 拒绝（mv 会退化成复制再删）。用替身的设备号模拟两个文件系统。
#[test]
fn owned_mv_refuses_cross_filesystem() {
    let (_t, d) = tmp();
    std::fs::create_dir_all(d.join("a")).unwrap();
    std::fs::create_dir_all(d.join("b")).unwrap();
    let out = sh(
        &d,
        r#"owned_mkdir "$1/a/x"
           _dev_of() { case "$1" in */a/*|*/a) echo 1 ;; *) echo 2 ;; esac; }
           owned_mv "$1/a/x" "$1/b/x""#,
    );
    assert!(!out.status.success());
    assert!(String::from_utf8_lossy(&out.stderr).contains("同一个文件系统"));
    assert!(d.join("a/x").exists() && !d.join("b/x").exists());
    // 对照：同一个文件系统上照常改名，目标登记为自己的
    ok(&sh(
        &d,
        r#"owned_mkdir "$1/a/y"; owned_mv "$1/a/y" "$1/b/y"; is_owned "$1/b/y""#,
    ));
}

/// 目录里有链接 → 拒绝往里写。
#[test]
fn no_links_in_refuses_links() {
    let (_t, d) = tmp();
    std::fs::create_dir_all(d.join("pkg/Contents")).unwrap();
    ok(&sh(&d, r#"no_links_in "$1/pkg""#));
    std::os::unix::fs::symlink(d.join("elsewhere"), d.join("pkg/Contents/MacOS")).unwrap();
    assert!(!sh(&d, r#"no_links_in "$1/pkg""#).status.success());
}
