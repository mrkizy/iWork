//! 发布包测试的夹具（T04p）：假的包、两种形态、测试用的签名身份。`check_install`、`assemble_release` 共用。
//! 程序用本测试程序的拷贝（只读、不执行）；形态 A 的清单由仓库里的 `scripts/write-install-manifest.sh` 写，
//! 不在测试里另算一份。
#![allow(dead_code)]

use std::path::{Path, PathBuf};
use std::process::Command;

pub const DAEMON: &str = iwork_daemon::install::DAEMON_NAME;
pub const CLIENT: &str = iwork_daemon::install::CLIENT_NAME;

pub fn repo_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .canonicalize()
        .unwrap()
}

pub fn run(cmd: &mut Command) -> String {
    let out = cmd.output().unwrap();
    assert!(
        out.status.success(),
        "{cmd:?}：{}{}",
        String::from_utf8_lossy(&out.stderr),
        String::from_utf8_lossy(&out.stdout)
    );
    String::from_utf8(out.stdout).unwrap()
}

/// 放一个「程序」：本测试程序的拷贝。`variant` 不同时身份不同（macOS 用另一个签名标识做 ad-hoc 重签，cdhash 随之
/// 变；Linux 在末尾追加程序名与 `variant`，全文哈希随之变）。和链接器给的 ad-hoc 签名一样，这是形态 A 里程序的样子。
///
/// 在包外签好再拷进去：对包的主程序（Info.plist 里的 `CFBundleExecutable`）跑 `codesign`，会连带给整个包做资源封印，
/// 而 Tauri 打出来的无证书包没有封印（N5 第 0 项）。
pub fn place_program(dir: &Path, name: &str, variant: &str) -> PathBuf {
    std::fs::create_dir_all(dir).unwrap();
    let staging = tempfile::tempdir().unwrap();
    let staged = staging.path().join(name);
    std::fs::copy(std::env::current_exe().unwrap(), &staged).unwrap();
    if cfg!(target_os = "macos") {
        run(Command::new("codesign")
            .args([
                "-f",
                "-s",
                "-",
                "-i",
                &format!("dev.iwork.test.{name}.{variant}"),
            ])
            .arg(&staged));
    } else {
        use std::io::Write;
        let mut f = std::fs::OpenOptions::new()
            .append(true)
            .open(&staged)
            .unwrap();
        // 和 macOS 的签名标识一样带上程序名：同一版的后台与客户端内容不同，清单里的身份不会碰巧通用
        // （CI run 43295353857：两者内容相同时，只登记后台的清单也认得客户端）
        f.write_all(format!("dev.iwork.test.{name}.{variant}").as_bytes())
            .unwrap();
    }
    let path = dir.join(name);
    std::fs::copy(&staged, &path).unwrap();
    path
}

/// 仓库的清单脚本给 `dir` 写清单。
pub fn write_manifest(dir: &Path) {
    run(
        Command::new(repo_root().join("scripts/write-install-manifest.sh"))
            .arg(dir)
            .arg("0.0.0-test"),
    );
}

/// macOS：在 `root` 下造一个 `iWork.app` 的骨架（Info.plist、一个资源文件），返回 `Contents/MacOS`。
pub fn app_skeleton(root: &Path) -> PathBuf {
    let app = root.join("iWork.app");
    let macos = app.join("Contents/MacOS");
    std::fs::create_dir_all(&macos).unwrap();
    std::fs::create_dir_all(app.join("Contents/Resources")).unwrap();
    std::fs::write(
        app.join("Contents/Info.plist"),
        r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0"><dict>
  <key>CFBundleIdentifier</key><string>dev.iwork.test.package</string>
  <key>CFBundleExecutable</key><string>iwork-client</string>
  <key>CFBundlePackageType</key><string>APPL</string>
</dict></plist>
"#,
    )
    .unwrap();
    std::fs::write(app.join("Contents/Resources/res.txt"), "资源").unwrap();
    macos
}

/// 形态 A：两个程序加清单，没有封印。macOS 在 `iWork.app/Contents/MacOS`，Linux 在 `iwork/`。返回放程序的目录。
pub fn form_a(root: &Path, version: &str) -> PathBuf {
    let dir = if cfg!(target_os = "macos") {
        app_skeleton(root)
    } else {
        root.join("iwork")
    };
    place_program(&dir, DAEMON, version);
    place_program(&dir, CLIENT, version);
    write_manifest(&dir);
    dir
}

/// 测试用的签名身份：临时钥匙串里的自签证书（`scripts/test-signing-identity.sh`）。丢弃时删钥匙串。
#[cfg(target_os = "macos")]
pub struct Signer {
    _dir: tempfile::TempDir,
    pub keychain: String,
    pub identity: String,
    pub leaf: [u8; 32],
    /// 叶证书的 SHA-1：codesign 按它指定身份，签的恰好是这一张
    pub sha1: String,
}

#[cfg(target_os = "macos")]
impl Signer {
    pub fn new(name: &str) -> Signer {
        let dir = tempfile::tempdir().unwrap();
        let out = run(
            Command::new(repo_root().join("scripts/test-signing-identity.sh"))
                .arg(dir.path())
                .arg(name),
        );
        let field = |key: &str| {
            out.lines()
                .find_map(|l| l.strip_prefix(&format!("{key}=")))
                .unwrap_or_else(|| panic!("脚本没报 {key}：{out}"))
                .to_owned()
        };
        Signer {
            keychain: field("keychain"),
            identity: field("identity"),
            sha1: field("sha1"),
            leaf: hex::decode(field("leaf_sha256"))
                .unwrap()
                .try_into()
                .unwrap(),
            _dir: dir,
        }
    }

    /// `codesign -f -s <身份> [-i <标识>] <路径>`
    pub fn sign(&self, path: &Path, id: Option<&str>) {
        let mut cmd = Command::new("codesign");
        cmd.args(["-f", "-s", &self.sha1, "--keychain", &self.keychain]);
        if let Some(id) = id {
            cmd.args(["-i", id]);
        }
        run(cmd.arg(path));
    }
}

#[cfg(target_os = "macos")]
impl Drop for Signer {
    fn drop(&mut self) {
        let _ = Command::new("security")
            .args(["delete-keychain", &self.keychain])
            .output();
    }
}

/// 形态 B：后台用 `daemon_signer` 签，整包（连同主程序客户端）用 `bundle_signer` 封；没有清单。返回 `Contents/MacOS`。
#[cfg(target_os = "macos")]
pub fn form_b(
    root: &Path,
    version: &str,
    daemon_signer: &Signer,
    bundle_signer: &Signer,
) -> PathBuf {
    let dir = app_skeleton(root);
    place_program(&dir, DAEMON, version);
    place_program(&dir, CLIENT, version);
    daemon_signer.sign(&dir.join(DAEMON), Some("dev.iwork.daemon"));
    bundle_signer.sign(&root.join("iWork.app"), None);
    dir
}
