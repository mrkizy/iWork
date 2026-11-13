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

/// 认领受管根（审查 2/F-1）：不存在就建目录与记录；空的真目录建记录；带记录的认；链接、没有记录的非空目录（哪怕里面
/// 有旧的 .iwork-owned 标记）不认，里面什么都不写。
#[test]
fn journal_root_rules() {
    let (_t, d) = tmp();
    ok(&sh(
        &d,
        r#"journal_root "$1/new"; [ -f "$1/new/.iwork-journal" ]"#,
    ));
    ok(&sh(&d, r#"journal_root "$1/new""#));
    std::fs::create_dir_all(d.join("empty")).unwrap();
    ok(&sh(
        &d,
        r#"journal_root "$1/empty"; [ -f "$1/empty/.iwork-journal" ]"#,
    ));
    std::fs::create_dir_all(d.join("foreign")).unwrap();
    std::fs::write(d.join("foreign/.iwork-owned"), "").unwrap();
    assert!(!sh(&d, r#"journal_root "$1/foreign""#).status.success());
    assert!(
        !d.join("foreign/.iwork-journal").exists(),
        "不认的目录里什么都没写"
    );
    std::os::unix::fs::symlink(d.join("new"), d.join("link")).unwrap();
    assert!(!sh(&d, r#"journal_root "$1/link""#).status.success());
}

/// 记录认身份（设备号:inode），不认名字：同名的换了一个（删了重建）就不认；最后一条是 del 就不认；改名之前记下改名
/// 之后的名字（身份不变），改名之后照认。
#[test]
fn journal_knows_by_identity_and_last_record() {
    let (_t, d) = tmp();
    let verdict = |script: &str| {
        let out = sh(
            &d,
            &format!(r#"journal_root "$1/root"; {script} && echo known || echo unknown"#),
        );
        ok(&out).trim().to_owned()
    };
    assert_eq!(
        verdict(r#"mkdir "$1/root/a"; journal_add "$1/root/a"; journal_knows "$1/root/a""#),
        "known"
    );
    assert_eq!(
        verdict(r#"rmdir "$1/root/a"; mkdir "$1/root/a"; journal_knows "$1/root/a""#),
        "unknown",
        "同名的换了一个"
    );
    assert_eq!(
        verdict(
            r#"mkdir "$1/root/b"; journal_add "$1/root/b"; journal_del "$1/root/b"; journal_knows "$1/root/b""#
        ),
        "unknown",
        "最后一条是 del"
    );
    assert_eq!(
        verdict(
            r#"mkdir "$1/root/c"; journal_add "$1/root/c2" "$(_ident "$1/root/c")"; mv "$1/root/c" "$1/root/c2"; journal_knows "$1/root/c2""#
        ),
        "known",
        "改名不改身份"
    );
    assert_eq!(
        verdict(r#"mkdir "$1/root/never"; journal_knows "$1/root/never""#),
        "unknown",
        "没记过"
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

/// 审查 3/F-1：已有的记录文件要证明是这个根自己的：不是硬链接（nlink 为 1）、第一行是带这个根身份的表头、每行格式
/// 对。外面的文件硬链接成记录、同名的普通文本、从别的根拷来的记录，都不认；外面文件的字节一个不变。
#[test]
fn journal_root_refuses_forged_or_linked_journals() {
    let (_t, d) = tmp();
    // 硬链接：外面的文件链进一个空根
    std::fs::create_dir_all(d.join("hard")).unwrap();
    std::fs::write(d.join("outside.txt"), "precious\n").unwrap();
    std::fs::hard_link(d.join("outside.txt"), d.join("hard/.iwork-journal")).unwrap();
    let out = sh(&d, r#"journal_root "$1/hard"; journal_add "$1/hard" "x""#);
    assert!(!out.status.success(), "硬链接的记录被认了");
    assert_eq!(
        std::fs::read_to_string(d.join("outside.txt")).unwrap(),
        "precious\n",
        "外面文件的字节没变"
    );
    // 硬链接，并且外面那个文件的内容恰好是这个根合格的记录（表头、格式都对）：只有「硬链接数为 1」这一条挡得住
    std::fs::create_dir_all(d.join("hard2")).unwrap();
    let header = ok(&sh(
        &d,
        r#"printf 'iwork-journal 1\t%s\n' "$(_ident "$1/hard2")""#,
    ));
    std::fs::write(d.join("outside2.txt"), &header).unwrap();
    std::fs::hard_link(d.join("outside2.txt"), d.join("hard2/.iwork-journal")).unwrap();
    let out = sh(
        &d,
        r#"journal_root "$1/hard2"; mkdir "$1/hard2/x"; journal_add "$1/hard2/x""#,
    );
    assert!(!out.status.success(), "内容合格的硬链接记录被认了");
    assert_eq!(
        std::fs::read_to_string(d.join("outside2.txt")).unwrap(),
        header,
        "外面文件的字节没变"
    );
    // 同名的普通文本
    std::fs::create_dir_all(d.join("plain")).unwrap();
    std::fs::write(d.join("plain/.iwork-journal"), "just notes\n").unwrap();
    assert!(!sh(&d, r#"journal_root "$1/plain""#).status.success());
    assert_eq!(
        std::fs::read_to_string(d.join("plain/.iwork-journal")).unwrap(),
        "just notes\n"
    );
    // 从别的根拷来的记录（表头里的根身份对不上）
    ok(&sh(&d, r#"journal_root "$1/orig""#));
    std::fs::create_dir_all(d.join("copy")).unwrap();
    std::fs::copy(d.join("orig/.iwork-journal"), d.join("copy/.iwork-journal")).unwrap();
    assert!(!sh(&d, r#"journal_root "$1/copy""#).status.success());
    // 对照：自己建的记录，下一次照认
    ok(&sh(&d, r#"journal_root "$1/orig""#));
}

/// 审查 3/F-4：读不了目录内容（权限 0300）时不能当成空目录认领：拒绝，里面不建记录。
#[test]
fn unreadable_dir_is_not_treated_as_empty() {
    use std::os::unix::fs::PermissionsExt;
    let (_t, d) = tmp();
    std::fs::create_dir_all(d.join("locked")).unwrap();
    std::fs::write(d.join("locked/keep"), "keep").unwrap();
    std::fs::set_permissions(d.join("locked"), std::fs::Permissions::from_mode(0o300)).unwrap();
    let out = sh(&d, r#"journal_root "$1/locked""#);
    let empty = sh(&d, r#"is_empty_dir "$1/locked""#);
    std::fs::set_permissions(d.join("locked"), std::fs::Permissions::from_mode(0o700)).unwrap();
    assert!(!out.status.success(), "读不了内容的目录被认领了");
    assert!(!d.join("locked/.iwork-journal").exists());
    assert!(!empty.status.success(), "读不了内容的目录被当成空的");
    // 对照：真空目录是空的；有东西的不是
    std::fs::create_dir_all(d.join("empty")).unwrap();
    ok(&sh(&d, r#"is_empty_dir "$1/empty""#));
    assert!(!sh(&d, r#"is_empty_dir "$1/locked""#).status.success());
}
