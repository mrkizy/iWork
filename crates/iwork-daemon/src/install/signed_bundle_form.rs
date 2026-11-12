//! 整包签名的包（macOS；有证书构建，包里不放清单）：后台程序在一个 `.app` 的 `Contents/MacOS` 里，且 `.app`
//! 有签名封印（`Contents/_CodeSignature/CodeResources`）。装齐的依据：后台程序在 `Contents/MacOS` 里，并且整包
//! 静态验签通过（严格、含嵌套代码）——封印登记了包里每一个文件，少一个、半个、多一个没登记的，都过不了；后台
//! 在而封印里没有它也过不了。只有客户端的 `.app` 验签能过，但后台不在，不算装齐（组 3 审查 1/F-5）。

use std::path::{Path, PathBuf};

use iwork_ipc::peer;

use super::{DAEMON_NAME, FormVerdict, PackageForm};

pub struct SignedBundleForm;

/// `exe_dir` 是 `X.app/Contents/MacOS` 时返回 `X.app`。
fn app_of(exe_dir: &Path) -> Option<PathBuf> {
    if exe_dir.file_name()? != "MacOS" {
        return None;
    }
    let contents = exe_dir.parent()?;
    if contents.file_name()? != "Contents" {
        return None;
    }
    let app = contents.parent()?;
    (app.extension()? == "app").then(|| app.to_path_buf())
}

impl PackageForm for SignedBundleForm {
    fn name(&self) -> &'static str {
        "signed-bundle"
    }

    fn check(&self, exe_dir: &Path) -> FormVerdict {
        let Some(app) = app_of(exe_dir) else {
            return FormVerdict::NotThisForm;
        };
        if !app.join("Contents/_CodeSignature/CodeResources").exists() {
            return FormVerdict::NotThisForm;
        }
        let daemon = exe_dir.join(DAEMON_NAME);
        if !std::fs::symlink_metadata(&daemon).is_ok_and(|m| m.is_file()) {
            return FormVerdict::Incomplete(format!("包里没有 {DAEMON_NAME}"));
        }
        match peer::verify_bundle(&app) {
            Ok(()) => FormVerdict::Complete,
            Err(e) => FormVerdict::Incomplete(format!("整包验签不过：{e}")),
        }
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use std::process::Command;

    fn run(cmd: &mut Command) {
        let out = cmd.output().unwrap();
        assert!(
            out.status.success(),
            "{cmd:?}：{}",
            String::from_utf8_lossy(&out.stderr)
        );
    }

    /// 在 `root` 下造一个 ad-hoc 整包签名的 `iWork.app`：Info.plist、`Contents/MacOS` 里的后台与客户端
    /// （本测试程序的拷贝，各按 `variant` 换签名标识）、一个资源文件。返回 `.app` 路径。
    pub(crate) fn sealed_app(root: &Path, variant: &str) -> PathBuf {
        sealed_app_with(root, variant, &["iwork-daemon", "iwork-client"])
    }

    /// 同 [`sealed_app`]，`Contents/MacOS` 里只放 `binaries` 这几个。
    pub(crate) fn sealed_app_with(root: &Path, variant: &str, binaries: &[&str]) -> PathBuf {
        let app = root.join("iWork.app");
        let macos = app.join("Contents/MacOS");
        std::fs::create_dir_all(&macos).unwrap();
        std::fs::create_dir_all(app.join("Contents/Resources")).unwrap();
        std::fs::write(
            app.join("Contents/Info.plist"),
            r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0"><dict>
  <key>CFBundleIdentifier</key><string>dev.iwork.test.sealed</string>
  <key>CFBundleExecutable</key><string>iwork-client</string>
  <key>CFBundlePackageType</key><string>APPL</string>
</dict></plist>
"#,
        )
        .unwrap();
        std::fs::write(
            app.join("Contents/Resources/res.txt"),
            format!("资源 {variant}"),
        )
        .unwrap();
        for name in binaries {
            let id = name.trim_start_matches("iwork-");
            let staged = root.join(format!(".{name}"));
            std::fs::copy(std::env::current_exe().unwrap(), &staged).unwrap();
            run(Command::new("codesign")
                .args([
                    "-f",
                    "-s",
                    "-",
                    "-i",
                    &format!("dev.iwork.test.{id}.{variant}"),
                ])
                .arg(&staged));
            std::fs::rename(&staged, macos.join(name)).unwrap();
        }
        run(Command::new("codesign").args(["-f", "-s", "-"]).arg(&app));
        app
    }

    fn copy_app(src: &Path, dst_root: &Path) -> PathBuf {
        std::fs::create_dir_all(dst_root).unwrap();
        run(Command::new("cp").arg("-R").arg(src).arg(dst_root));
        dst_root.join("iWork.app")
    }

    fn verdict(app: &Path) -> FormVerdict {
        SignedBundleForm.check(&app.join("Contents/MacOS"))
    }

    /// S7b：完整的封印包 → Complete；删一个资源、截断一个二进制、多放一个没登记的文件 → 各自 Incomplete。
    #[test]
    fn partial_copy_is_incomplete() {
        let t = tempfile::tempdir().unwrap();
        let app = sealed_app(&t.path().join("orig"), "a");
        assert_eq!(verdict(&app), FormVerdict::Complete, "对照：完整的包");

        let a = copy_app(&app, &t.path().join("del"));
        std::fs::remove_file(a.join("Contents/Resources/res.txt")).unwrap();
        assert!(matches!(verdict(&a), FormVerdict::Incomplete(_)), "删资源");

        let b = copy_app(&app, &t.path().join("trunc"));
        let bin = b.join("Contents/MacOS/iwork-daemon");
        let len = std::fs::metadata(&bin).unwrap().len();
        std::fs::File::options()
            .write(true)
            .open(&bin)
            .unwrap()
            .set_len(len / 2)
            .unwrap();
        assert!(
            matches!(verdict(&b), FormVerdict::Incomplete(_)),
            "截断后台"
        );

        let c = copy_app(&app, &t.path().join("extra"));
        std::fs::write(c.join("Contents/Resources/extra.txt"), "多出来的").unwrap();
        assert!(
            matches!(verdict(&c), FormVerdict::Incomplete(_)),
            "多一个文件"
        );
    }

    /// 1/F-5 反例：只有客户端的 `.app`，整包验签能过，后台不在 → 没装齐。对照：再按同样做法带上后台 → 装齐。
    #[test]
    fn client_only_bundle_is_incomplete() {
        let t = tempfile::tempdir().unwrap();
        let app = sealed_app_with(&t.path().join("client-only"), "a", &["iwork-client"]);
        assert!(peer::verify_bundle(&app).is_ok(), "前提：整包验签能过");
        assert!(matches!(verdict(&app), FormVerdict::Incomplete(_)));
        let full = sealed_app(&t.path().join("full"), "a");
        assert_eq!(verdict(&full), FormVerdict::Complete);
    }

    /// 不是 .app、或没有封印 → NotThisForm（交给别的行判）。
    #[test]
    fn not_a_sealed_bundle_is_not_this_form() {
        let t = tempfile::tempdir().unwrap();
        assert_eq!(SignedBundleForm.check(t.path()), FormVerdict::NotThisForm);
        let app = sealed_app(&t.path().join("x"), "a");
        std::fs::remove_dir_all(app.join("Contents/_CodeSignature")).unwrap();
        assert_eq!(verdict(&app), FormVerdict::NotThisForm);
    }
}
