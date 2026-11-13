//! 测试签名身份脚本 `scripts/test-signing-identity.sh`（审查 3/F-3）：导入失败时脚本必须失败，不能报出一个成功的身份。
#![cfg(target_os = "macos")]

use std::path::Path;
use std::process::Command;

/// 假的 `security`：导入失败（退出 9），之后的 set-key-partition-list 成功（退出 0）。只在测试的 PATH 里，不碰真钥匙串。
#[test]
fn failed_import_fails_the_script_even_if_the_next_step_succeeds() {
    use std::os::unix::fs::PermissionsExt;
    let t = tempfile::tempdir().unwrap();
    let bin = t.path().join("bin");
    std::fs::create_dir_all(&bin).unwrap();
    let fake = bin.join("security");
    std::fs::write(
        &fake,
        "#!/bin/sh\ncase \"$1\" in import) echo 'import failed' >&2; exit 9 ;; *) exit 0 ;; esac\n",
    )
    .unwrap();
    std::fs::set_permissions(&fake, std::fs::Permissions::from_mode(0o755)).unwrap();
    // 共用钥匙串模式：给一个已有的文件当钥匙串，脚本不新建
    let kc = t.path().join("shared.keychain-db");
    std::fs::write(&kc, "").unwrap();
    let work = t.path().join("work");
    std::fs::create_dir_all(&work).unwrap();
    let script =
        Path::new(env!("CARGO_MANIFEST_DIR")).join("../../scripts/test-signing-identity.sh");
    let out = Command::new("/bin/bash")
        .arg(&script)
        .arg(&work)
        .arg("iWork import fails")
        .env(
            "PATH",
            // 假的 security 放最前面，其余照用调用方的 PATH（openssl 要能 -legacy 的那个，不是系统自带的 LibreSSL）
            format!(
                "{}:{}",
                bin.display(),
                std::env::var("PATH").unwrap_or_default()
            ),
        )
        .env("IWORK_TEST_SIGNING_KEYCHAIN", &kc)
        .output()
        .unwrap();
    let stdout = String::from_utf8_lossy(&out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(!out.status.success(), "导入失败，脚本却成功了：{stdout}");
    assert!(
        stderr.contains("import failed"),
        "是在导入这一步失败的：{stderr}"
    );
    assert!(!stdout.contains("sha1="), "导入失败还报出了身份：{stdout}");
}
