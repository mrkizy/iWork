//! `peer::file_signer`（T04p 方案 4.4、R10 `file_signer_table`）：磁盘上一个程序文件是谁签的。
//! 期望值不经 `file_signer` 自己算：证书那一例用建证书的脚本报出的叶证书 SHA-256，ad-hoc 那一例用
//! `file_image_ids` 读到的 cdhash，Linux 用文件全文的 SHA-256。
#![cfg(unix)]

use std::path::{Path, PathBuf};
use std::process::Command;

use iwork_ipc::peer::{ImageId, SignerIdentity, Want, file_image_ids, file_signer};

/// 一份能签名的程序：本测试程序的拷贝（不执行它，只读）。
fn program_copy(dir: &Path, name: &str) -> PathBuf {
    let path = dir.join(name);
    std::fs::copy(std::env::current_exe().unwrap(), &path).unwrap();
    path
}

#[test]
fn missing_file_is_unavailable() {
    let d = tempfile::tempdir().unwrap();
    assert!(matches!(
        file_signer(&d.path().join("nope")),
        SignerIdentity::Unavailable { .. }
    ));
}

#[cfg(target_os = "linux")]
#[test]
fn linux_file_signer_is_whole_file_sha256() {
    use sha2::{Digest, Sha256};
    let d = tempfile::tempdir().unwrap();
    let p = program_copy(d.path(), "prog");
    let want: [u8; 32] = Sha256::digest(std::fs::read(&p).unwrap()).into();
    assert_eq!(
        file_signer(&p),
        SignerIdentity::Image(ImageId::Sha256(want))
    );
}

#[cfg(target_os = "macos")]
mod macos {
    use super::*;

    fn run(cmd: &mut Command) -> String {
        let out = cmd.output().unwrap();
        assert!(
            out.status.success(),
            "{cmd:?}：{}",
            String::from_utf8_lossy(&out.stderr)
        );
        String::from_utf8(out.stdout).unwrap()
    }

    /// 测试用的签名身份：临时钥匙串里的自签证书。丢弃时删钥匙串。
    struct Signer {
        _dir: tempfile::TempDir,
        keychain: String,
        identity: String,
        leaf: [u8; 32],
    }

    impl Signer {
        fn new(name: &str) -> Signer {
            let dir = tempfile::tempdir().unwrap();
            let script = Path::new(env!("CARGO_MANIFEST_DIR"))
                .join("../../scripts/test-signing-identity.sh");
            let out = run(Command::new(script).arg(dir.path()).arg(name));
            let field = |key: &str| {
                out.lines()
                    .find_map(|l| l.strip_prefix(&format!("{key}=")))
                    .unwrap_or_else(|| panic!("脚本没报 {key}：{out}"))
                    .to_owned()
            };
            let leaf: [u8; 32] = hex::decode(field("leaf_sha256"))
                .unwrap()
                .try_into()
                .unwrap();
            Signer {
                keychain: field("keychain"),
                identity: field("identity"),
                leaf,
                _dir: dir,
            }
        }

        fn sign(&self, path: &Path) {
            run(Command::new("codesign")
                .args(["-f", "-s", &self.identity, "--keychain", &self.keychain])
                .arg(path));
        }
    }

    impl Drop for Signer {
        fn drop(&mut self) {
            let _ = Command::new("security")
                .args(["delete-keychain", &self.keychain])
                .output();
        }
    }

    /// 证书签的、ad-hoc 签的、没签的、签了之后被改坏的，各一例。
    #[test]
    fn file_signer_table() {
        let d = tempfile::tempdir().unwrap();
        let signer = Signer::new("iWork file_signer test");

        let cert = program_copy(d.path(), "cert");
        signer.sign(&cert);
        assert_eq!(
            file_signer(&cert),
            SignerIdentity::Certificate {
                leaf_sha256: signer.leaf
            },
            "证书签的：叶证书的 SHA-256"
        );

        let adhoc = program_copy(d.path(), "adhoc");
        run(Command::new("codesign").args(["-f", "-s", "-"]).arg(&adhoc));
        let cdhash = file_image_ids(&adhoc, Want::All)
            .unwrap()
            .into_iter()
            .find(|i| matches!(i, ImageId::CdHash(_)))
            .expect("ad-hoc 签的文件有 cdhash");
        assert_eq!(
            file_signer(&adhoc),
            SignerIdentity::Image(cdhash),
            "ad-hoc：cdhash"
        );

        let unsigned = program_copy(d.path(), "unsigned");
        run(Command::new("codesign")
            .arg("--remove-signature")
            .arg(&unsigned));
        assert!(
            matches!(file_signer(&unsigned), SignerIdentity::Unavailable { .. }),
            "没签的：取不到"
        );

        // 签完之后在文件中部改一个字节：代码页哈希对不上，静态校验不过
        let broken = program_copy(d.path(), "broken");
        signer.sign(&broken);
        let mut bytes = std::fs::read(&broken).unwrap();
        let mid = bytes.len() / 2;
        bytes[mid] ^= 0xff;
        std::fs::write(&broken, bytes).unwrap();
        assert!(
            matches!(file_signer(&broken), SignerIdentity::Unavailable { .. }),
            "改坏的：验不过"
        );
    }
}
