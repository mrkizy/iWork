//! 带清单的包（无证书构建；T03 方案 2.4a）：后台程序旁边有 `iwork-manifest.json`。装齐的依据：清单能解析，
//! 且它为每个二进制登记的每一种身份都对得上磁盘上的那个文件（`sha256:` 核全文哈希；macOS 另核 `cdhash:`）。
//! 看内容不看时间：只 `touch` 不改变结论。

use std::path::Path;

use iwork_ipc::peer::{self, MANIFEST_FILE, Manifest, Want};

use super::{FormVerdict, PackageForm};

pub struct ManifestForm;

/// 清单里的二进制名对应的文件名（Windows 加 `.exe`）。
fn binary_file(name: &str) -> String {
    format!("{name}{}", std::env::consts::EXE_SUFFIX)
}

impl PackageForm for ManifestForm {
    fn name(&self) -> &'static str {
        "manifest"
    }

    fn check(&self, exe_dir: &Path) -> FormVerdict {
        let path = exe_dir.join(MANIFEST_FILE);
        let text = match std::fs::read_to_string(&path) {
            Ok(t) => t,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return FormVerdict::NotThisForm,
            Err(e) => return FormVerdict::Incomplete(format!("读清单失败：{e}")),
        };
        let manifest = match Manifest::parse(&text) {
            Ok(m) => m,
            Err(e) => return FormVerdict::Incomplete(format!("清单解析不了：{e}")),
        };
        for (name, listed) in &manifest.binaries {
            let file = exe_dir.join(binary_file(name));
            let on_disk = match peer::file_image_ids(&file, Want::All) {
                Ok(ids) => ids,
                Err(e) => return FormVerdict::Incomplete(format!("{name} 读不了：{e}")),
            };
            if let Some(missing) = listed.iter().find(|id| !on_disk.contains(id)) {
                return FormVerdict::Incomplete(format!("{name} 对不上清单登记的 {missing}"));
            }
        }
        FormVerdict::Complete
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use std::path::PathBuf;

    /// 按 `scripts/write-install-manifest.sh` 的格式给目录里的这些二进制写清单（身份取 `file_image_ids` 的全部种类）。
    pub(crate) fn write_manifest(dir: &Path, binaries: &[&str]) -> String {
        let entries: Vec<String> = binaries
            .iter()
            .map(|b| {
                let ids = peer::file_image_ids(&dir.join(binary_file(b)), Want::All).unwrap();
                let list: Vec<String> = ids.iter().map(|i| format!("\"{i}\"")).collect();
                format!("\"{b}\":[{}]", list.join(","))
            })
            .collect();
        let text = format!(
            r#"{{"manifest_version":1,"app_version":"t","binaries":{{{}}}}}"#,
            entries.join(",")
        );
        std::fs::write(dir.join(MANIFEST_FILE), &text).unwrap();
        text
    }

    /// 放一个「二进制」：真实的可执行文件（本测试程序）的拷贝；`variant` 不同时内容不同。
    /// macOS 上用不同签名标识做 ad-hoc 重签，cdhash 随之不同（尾部追加字节改不了 cdhash）。
    pub(crate) fn place_binary(dir: &Path, name: &str, variant: &str) -> PathBuf {
        let path = dir.join(binary_file(name));
        let staged = dir.join(format!(".{name}.staged"));
        std::fs::copy(std::env::current_exe().unwrap(), &staged).unwrap();
        if cfg!(target_os = "macos") {
            let ok = std::process::Command::new("codesign")
                .args(["-f", "-s", "-", "-i", &format!("dev.iwork.test.{variant}")])
                .arg(&staged)
                .status()
                .unwrap()
                .success();
            assert!(ok, "codesign 重签失败");
        } else {
            use std::io::Write;
            let mut f = std::fs::OpenOptions::new()
                .append(true)
                .open(&staged)
                .unwrap();
            f.write_all(variant.as_bytes()).unwrap();
        }
        std::fs::rename(&staged, &path).unwrap();
        path
    }

    fn dir() -> tempfile::TempDir {
        tempfile::tempdir().unwrap()
    }

    #[test]
    fn missing_manifest_is_not_this_form() {
        let d = dir();
        place_binary(d.path(), "iwork-daemon", "a");
        assert_eq!(ManifestForm.check(d.path()), FormVerdict::NotThisForm);
    }

    #[test]
    fn complete_when_every_binary_matches() {
        let d = dir();
        place_binary(d.path(), "iwork-daemon", "a");
        place_binary(d.path(), "iwork-client", "c");
        write_manifest(d.path(), &["iwork-daemon", "iwork-client"]);
        assert_eq!(ManifestForm.check(d.path()), FormVerdict::Complete);
    }

    /// S15：只改时间戳（`touch`）不改变结论。
    #[test]
    fn touch_only_is_still_complete() {
        let d = dir();
        let p = place_binary(d.path(), "iwork-daemon", "a");
        write_manifest(d.path(), &["iwork-daemon"]);
        let f = std::fs::File::options().write(true).open(&p).unwrap();
        f.set_modified(std::time::SystemTime::now() + std::time::Duration::from_secs(3600))
            .unwrap();
        assert_eq!(ManifestForm.check(d.path()), FormVerdict::Complete);
    }

    /// 清单是新的、后台文件还是旧的 → 没装齐。
    #[test]
    fn new_manifest_old_daemon_is_incomplete() {
        let d = dir();
        place_binary(d.path(), "iwork-daemon", "b");
        write_manifest(d.path(), &["iwork-daemon"]);
        place_binary(d.path(), "iwork-daemon", "a");
        assert!(matches!(
            ManifestForm.check(d.path()),
            FormVerdict::Incomplete(_)
        ));
    }

    /// 后台文件是新的、清单还是旧的 → 没装齐。
    #[test]
    fn new_daemon_old_manifest_is_incomplete() {
        let d = dir();
        place_binary(d.path(), "iwork-daemon", "a");
        write_manifest(d.path(), &["iwork-daemon"]);
        place_binary(d.path(), "iwork-daemon", "b");
        assert!(matches!(
            ManifestForm.check(d.path()),
            FormVerdict::Incomplete(_)
        ));
    }

    #[test]
    fn half_written_manifest_is_incomplete() {
        let d = dir();
        place_binary(d.path(), "iwork-daemon", "a");
        let text = write_manifest(d.path(), &["iwork-daemon"]);
        std::fs::write(d.path().join(MANIFEST_FILE), &text[..text.len() / 2]).unwrap();
        assert!(matches!(
            ManifestForm.check(d.path()),
            FormVerdict::Incomplete(_)
        ));
    }

    /// 登记了而文件不在（客户端还没拷到）→ 没装齐。
    #[test]
    fn listed_binary_missing_is_incomplete() {
        let d = dir();
        place_binary(d.path(), "iwork-daemon", "a");
        place_binary(d.path(), "iwork-client", "c");
        write_manifest(d.path(), &["iwork-daemon", "iwork-client"]);
        std::fs::remove_file(d.path().join(binary_file("iwork-client"))).unwrap();
        assert!(matches!(
            ManifestForm.check(d.path()),
            FormVerdict::Incomplete(_)
        ));
    }

    /// macOS：`sha256:` 对得上而 `cdhash:` 对不上 → 没装齐（两种都核）。
    #[cfg(target_os = "macos")]
    #[test]
    fn sha_ok_but_cdhash_wrong_is_incomplete() {
        let d = dir();
        let p = place_binary(d.path(), "iwork-daemon", "a");
        let ids = peer::file_image_ids(&p, Want::All).unwrap();
        let sha = ids
            .iter()
            .find(|i| matches!(i, iwork_ipc::peer::ImageId::Sha256(_)))
            .unwrap();
        let text = format!(
            r#"{{"manifest_version":1,"app_version":"t","binaries":{{"iwork-daemon":["{sha}","cdhash:{}"]}}}}"#,
            "00".repeat(20)
        );
        std::fs::write(d.path().join(MANIFEST_FILE), text).unwrap();
        assert!(matches!(
            ManifestForm.check(d.path()),
            FormVerdict::Incomplete(_)
        ));
    }
}
