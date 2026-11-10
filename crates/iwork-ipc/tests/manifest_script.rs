//! `scripts/write-install-manifest.sh` 写出的清单能被 `Manifest` 解析，且登记的哈希就是二进制的哈希
//! （T03 方案 2.4a、第 11 节第 3 步验收）。脚本需要 bash 与 jq：缺了测试会失败并说明，不会跳过。
#![cfg(unix)]

use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use iwork_ipc::peer::{ImageId, MANIFEST_FILE, Manifest};
use sha2::{Digest, Sha256};

fn script() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../scripts/write-install-manifest.sh")
}

fn run_script(args: &[&Path]) -> Output {
    Command::new("bash")
        .arg(script())
        .args(args)
        .output()
        .expect("起不了 bash：这个测试需要 bash 与 jq")
}

fn sha256_image(bytes: &[u8]) -> ImageId {
    ImageId::Sha256(Sha256::digest(bytes).into())
}

#[test]
fn script_output_parses_and_lists_each_binary_hash() {
    let dir = tempfile::tempdir().unwrap();
    let daemon = b"fake daemon image".as_slice();
    let client = b"fake client image, different bytes".as_slice();
    std::fs::write(dir.path().join("iwork-daemon"), daemon).unwrap();
    std::fs::write(dir.path().join("iwork-client"), client).unwrap();

    let out = run_script(&[dir.path()]);
    assert!(
        out.status.success(),
        "脚本失败（缺 jq 时也会走到这里）：{}",
        String::from_utf8_lossy(&out.stderr)
    );

    let manifest = Manifest::load(&dir.path().join(MANIFEST_FILE)).unwrap();
    // 版本默认取 workspace 版本，本 crate 用的就是它。
    assert_eq!(manifest.app_version, env!("CARGO_PKG_VERSION"));
    // 假二进制没有签名，macOS 上也只有 sha256 一条。
    assert_eq!(
        manifest.binaries["iwork-daemon"],
        vec![sha256_image(daemon)]
    );
    assert_eq!(
        manifest.binaries["iwork-client"],
        vec![sha256_image(client)]
    );
    assert_eq!(manifest.binaries.len(), 2);
    assert!(!manifest.contains(&sha256_image(b"not installed")));
}

#[test]
fn script_takes_explicit_app_version() {
    let dir = tempfile::tempdir().unwrap();
    for name in ["iwork-daemon", "iwork-client"] {
        std::fs::write(dir.path().join(name), name).unwrap();
    }
    let out = run_script(&[dir.path(), Path::new("9.8.7")]);
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let manifest = Manifest::load(&dir.path().join(MANIFEST_FILE)).unwrap();
    assert_eq!(manifest.app_version, "9.8.7");
}

#[test]
fn script_fails_and_names_missing_binary() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("iwork-daemon"), b"daemon").unwrap();

    let out = run_script(&[dir.path()]);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert_eq!(out.status.code(), Some(1), "{stderr}");
    assert!(stderr.contains("iwork-client"), "{stderr}");
    assert!(!stderr.contains("iwork-daemon"), "只点名缺的那个：{stderr}");
    assert!(!dir.path().join(MANIFEST_FILE).exists());
}
