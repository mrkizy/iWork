//! 出包前的核对（T04p 方案 4.4；R2、R10）：`install::package_check::check_package` 的三件事。
#![cfg(unix)]

mod package_support;

use iwork_daemon::install::package_check::{Finding, check_package};
use package_support::*;

fn finding<'a>(findings: &'a [Finding], item: &str) -> &'a Finding {
    findings
        .iter()
        .find(|f| f.item == item)
        .unwrap_or_else(|| panic!("没有「{item}」这一件：{findings:?}"))
}

/// 完整的形态 A：恰好一种形态、双向互认。
#[test]
fn complete_unsigned_package_passes() {
    let t = tempfile::tempdir().unwrap();
    let dir = form_a(t.path(), "a");
    let f = check_package(&dir, None);
    assert!(f.iter().all(|f| f.passed), "{f:#?}");
    assert_eq!(f.len(), 2, "没给预期证书就不核第 3 件");
}

/// R10 `check_install_trust_table` 的形态 A 一行：清单里少登记客户端 → 形态齐（清单登记了后台），但后台不认客户端。
#[test]
fn manifest_without_client_fails_trust() {
    let t = tempfile::tempdir().unwrap();
    let dir = form_a(t.path(), "a");
    // 重写清单：只登记后台
    let client = dir.join(CLIENT);
    let moved = t.path().join("client.away");
    std::fs::rename(&client, &moved).unwrap();
    write_manifest_only(&dir, &[DAEMON]);
    std::fs::rename(&moved, &client).unwrap();
    let f = check_package(&dir, None);
    assert!(finding(&f, "形态").passed, "{f:#?}");
    let trust = finding(&f, "互认");
    assert!(!trust.passed, "{trust:?}");
    assert!(trust.detail.contains("ImageNotTrusted"), "{trust:?}");
}

/// 清单脚本只认它固定的两个程序，少一个就失败；这里直接写一份只含给定程序的清单（身份照脚本的规矩取全部种类）。
fn write_manifest_only(dir: &std::path::Path, names: &[&str]) {
    use iwork_ipc::peer::{Want, file_image_ids};
    let entries: Vec<String> = names
        .iter()
        .map(|n| {
            let ids = file_image_ids(&dir.join(n), Want::All).unwrap();
            let list: Vec<String> = ids.iter().map(|i| format!("\"{i}\"")).collect();
            format!("\"{n}\":[{}]", list.join(","))
        })
        .collect();
    std::fs::write(
        dir.join(iwork_ipc::peer::MANIFEST_FILE),
        format!(
            r#"{{"manifest_version":1,"app_version":"t","binaries":{{{}}}}}"#,
            entries.join(",")
        ),
    )
    .unwrap();
}

#[cfg(target_os = "macos")]
mod signed {
    use super::*;
    use std::process::Command;

    /// R2 `check_install_forms_table`：完整的 A、完整的 B 过；A 里多一个封印目录、B 里多一份清单，不过。
    #[test]
    fn check_install_forms_table() {
        let t = tempfile::tempdir().unwrap();
        let s = Signer::new("iWork check_install forms");

        let a = form_a(&t.path().join("a"), "a");
        assert!(finding(&check_package(&a, None), "形态").passed, "完整的 A");

        let b = form_b(&t.path().join("b"), "b", &s, &s);
        let fb = check_package(&b, None);
        assert!(finding(&fb, "形态").passed, "完整的 B：{fb:#?}");

        // A 里多一个封印目录（像一个没封好的签名包）：整包签名那一行判 Incomplete，不放过
        let a2 = form_a(&t.path().join("a2"), "a");
        let app = a2.parent().unwrap().parent().unwrap();
        std::fs::create_dir_all(app.join("Contents/_CodeSignature")).unwrap();
        std::fs::write(app.join("Contents/_CodeSignature/CodeResources"), "x").unwrap();
        let fa2 = check_package(&a2, None);
        assert!(!finding(&fa2, "形态").passed, "A 里多封印：{fa2:#?}");

        // B 里多一份清单（封印之后放进去）：清单那一行齐、整包签名那一行不齐 → 两可，不放过
        let b2 = form_b(&t.path().join("b2"), "b", &s, &s);
        write_manifest_only(&b2, &[DAEMON, CLIENT]);
        let fb2 = check_package(&b2, None);
        assert!(!finding(&fb2, "形态").passed, "B 里多清单：{fb2:#?}");
    }

    /// R10 `check_install_trust_table` 的形态 B 两行：同一张证书 → 双向接受；后台与客户端两张证书 → SignerMismatch。
    #[test]
    fn check_install_trust_table_signed() {
        let t = tempfile::tempdir().unwrap();
        let s1 = Signer::new("iWork check_install one");
        let s2 = Signer::new("iWork check_install two");

        let same = form_b(&t.path().join("same"), "b", &s1, &s1);
        let f = check_package(&same, None);
        assert!(f.iter().all(|f| f.passed), "{f:#?}");

        let mixed = form_b(&t.path().join("mixed"), "b", &s2, &s1);
        let f = check_package(&mixed, None);
        let trust = finding(&f, "互认");
        assert!(!trust.passed, "{f:#?}");
        assert!(trust.detail.contains("SignerMismatch"), "{trust:?}");
    }

    /// R10 `expected_leaf_mismatch_fails`：B 是用另一张证书签的，给的是预期那张 → 第 3 件不过；给对了就过。
    #[test]
    fn expected_leaf_mismatch_fails() {
        let t = tempfile::tempdir().unwrap();
        let used = Signer::new("iWork check_install used");
        let expected = Signer::new("iWork check_install expected");
        let b = form_b(&t.path().join("b"), "b", &used, &used);
        let wrong = check_package(&b, Some(&expected.leaf));
        assert!(!finding(&wrong, "证书").passed, "{wrong:#?}");
        assert!(finding(&wrong, "互认").passed, "两个程序本身互认");
        let right = check_package(&b, Some(&used.leaf));
        assert!(right.iter().all(|f| f.passed), "{right:#?}");
    }

    /// R10 `adhoc_identity_fails_before_publish` 的后一半：手工做一个 ad-hoc 整包签名的包（绕过装配脚本第 0 步），
    /// 第 1 件过（整包签名那一行齐），第 2 件两个程序都只有映像身份又没有清单 → NoManifest，不过。
    #[test]
    fn adhoc_sealed_package_fails_trust() {
        let t = tempfile::tempdir().unwrap();
        let dir = app_skeleton(t.path());
        place_program(&dir, DAEMON, "x");
        place_program(&dir, CLIENT, "x");
        run(Command::new("codesign")
            .args(["-f", "-s", "-"])
            .arg(t.path().join("iWork.app")));
        let f = check_package(&dir, None);
        assert!(finding(&f, "形态").passed, "{f:#?}");
        let trust = finding(&f, "互认");
        assert!(
            !trust.passed && trust.detail.contains("NoManifest"),
            "{trust:?}"
        );
    }
}
