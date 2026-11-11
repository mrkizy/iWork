//! 安装检查用的身份函数（T04 方案 2.9、S7e）：`running_image_id` 是「我正在运行的这一份」，`file_image_ids` 是
//! 「磁盘上现在那个文件」。没换文件时前者在后者里（两种取法比的是同一种东西）；进程运行中把路径上的文件换成另一版，
//! 再取运行映像只会得到原值或报错，绝不会是新文件——而 `file_image_ids` 这时给的是新文件。
//!
//! 被测进程就是本测试二进制自己：复制进临时目录后，带 [`CHILD_ENV`] 只跑 [`running_image_child_entry`]。
//! 「另一版」在 macOS 上是换签名标识的 ad-hoc 重签（cdhash 必变），在 Linux 上是末尾多一字节（全文哈希必变）。
#![cfg(unix)]

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use iwork_ipc::peer::{ImageId, Want, file_image_ids, running_image_id};

/// 子进程模式的开关：值是控制目录。子进程打印 `start …`，等目录里出现 `go` 再打印 `after …`。
const CHILD_ENV: &str = "IWORK_T04_RUNNING_IMAGE_CHILD";
const CHILD_TEST_NAME: &str = "running_image_child_entry";

#[test]
fn running_image_child_entry() {
    let Some(dir) = std::env::var_os(CHILD_ENV) else {
        return;
    };
    let dir = PathBuf::from(dir);
    let exe = std::env::current_exe().unwrap();
    let running = running_image_id().expect("启动时取运行映像身份");
    println!("@start running={running}");
    println!("@start file={}", ids_text(&exe, &running));
    std::fs::write(dir.join("started"), b"").unwrap();
    let deadline = Instant::now() + Duration::from_secs(20);
    while !dir.join("go").exists() && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(20));
    }
    match running_image_id() {
        Ok(id) => println!("@after running={id}"),
        Err(e) => println!("@after running=error {e}"),
    }
    println!("@after file={}", ids_text(&exe, &running));
}

fn ids_text(path: &Path, like: &ImageId) -> String {
    match file_image_ids(path, Want::SameKindAs(like)) {
        Ok(ids) => ids
            .iter()
            .map(ToString::to_string)
            .collect::<Vec<_>>()
            .join(","),
        Err(e) => format!("error {e}"),
    }
}

/// 临时目录里的被测程序与控制目录。
struct Setup {
    _root: tempfile::TempDir,
    exe: PathBuf,
    control: PathBuf,
}

impl Setup {
    fn new() -> Setup {
        let root = tempfile::tempdir().unwrap();
        let base = root.path().canonicalize().unwrap();
        let exe = base.join("iwork-daemon");
        std::fs::copy(std::env::current_exe().unwrap(), &exe).unwrap();
        let control = base.join("control");
        std::fs::create_dir(&control).unwrap();
        Setup {
            _root: root,
            exe,
            control,
        }
    }

    /// 造一份「另一版」放在旁边，返回路径。
    fn other_build(&self) -> PathBuf {
        let other = self.exe.with_file_name("other-build");
        std::fs::copy(&self.exe, &other).unwrap();
        make_different(&other);
        other
    }
}

#[cfg(target_os = "macos")]
fn make_different(path: &Path) {
    let out = Command::new("codesign")
        .args(["-f", "-s", "-", "-i", "dev.iwork.test.other-build"])
        .arg(path)
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
}

#[cfg(not(target_os = "macos"))]
fn make_different(path: &Path) {
    use std::io::Write;
    std::fs::OpenOptions::new()
        .append(true)
        .open(path)
        .unwrap()
        .write_all(&[0])
        .unwrap();
}

struct Report {
    start_running: String,
    start_file: String,
    after_running: String,
    after_file: String,
}

/// 起子进程，等它取完启动时的身份，执行 `replace`，再让它重取，收集四行结果。
fn run_child(setup: &Setup, replace: impl FnOnce()) -> Report {
    let child = Command::new(&setup.exe)
        .args([
            "--exact",
            CHILD_TEST_NAME,
            "--test-threads=1",
            "--nocapture",
        ])
        .env(CHILD_ENV, &setup.control)
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let deadline = Instant::now() + Duration::from_secs(20);
    while !setup.control.join("started").exists() {
        assert!(Instant::now() < deadline, "子进程没有起来");
        std::thread::sleep(Duration::from_millis(20));
    }
    replace();
    std::fs::write(setup.control.join("go"), b"").unwrap();
    let out = child.wait_with_output().unwrap();
    let text = String::from_utf8_lossy(&out.stdout).into_owned();
    let field = |tag: &str| -> String {
        text.lines()
            .find_map(|l| l.find(tag).map(|i| l[i + tag.len()..].to_string()))
            .unwrap_or_else(|| panic!("子进程输出里没有 {tag}：{text}"))
    };
    Report {
        start_running: field("@start running="),
        start_file: field("@start file="),
        after_running: field("@after running="),
        after_file: field("@after file="),
    }
}

/// 对照：没换文件时，运行身份就在 `file_image_ids(自己的路径)` 里，前后两次一样。
#[test]
fn running_image_id_matches_file_before_replacement() {
    let setup = Setup::new();
    let r = run_child(&setup, || {});
    assert_eq!(r.start_file, r.start_running);
    assert_eq!(r.after_running, r.start_running);
    assert_eq!(r.after_file, r.start_running);
}

fn assert_survives(r: &Report, other: &Path) {
    let new_id = file_image_ids(other, Want::All).unwrap();
    let new_text: Vec<String> = new_id.iter().map(ToString::to_string).collect();
    assert!(
        !new_text.contains(&r.start_running),
        "测试前提：另一版的身份要和原来不同"
    );
    assert_eq!(r.start_file, r.start_running);
    // 磁盘上现在是另一版：file_image_ids 看得到它
    assert!(
        new_text.contains(&r.after_file),
        "换完之后路径上的身份 {} 应当是另一版 {new_text:?}",
        r.after_file
    );
    // 运行映像只会是原值或报错，绝不是新文件
    assert!(
        r.after_running == r.start_running || r.after_running.starts_with("error"),
        "换文件后运行映像变成了 {}",
        r.after_running
    );
    assert!(!new_text.contains(&r.after_running));
}

#[test]
fn running_image_id_survives_file_replacement_by_rename() {
    let setup = Setup::new();
    let other = setup.other_build();
    let r = run_child(&setup, || {
        std::fs::rename(&setup.exe, setup.exe.with_file_name("iwork-daemon.old")).unwrap();
        std::fs::copy(&other, &setup.exe).unwrap();
    });
    assert_survives(&r, &other);
}

#[test]
fn running_image_id_survives_file_replacement_by_remove() {
    let setup = Setup::new();
    let other = setup.other_build();
    let r = run_child(&setup, || {
        std::fs::remove_file(&setup.exe).unwrap();
        std::fs::copy(&other, &setup.exe).unwrap();
    });
    assert_survives(&r, &other);
}

#[test]
fn file_image_ids_reports_what_was_asked() {
    let setup = Setup::new();
    let running = running_image_id().unwrap();
    let all = file_image_ids(&setup.exe, Want::All).unwrap();
    let same = file_image_ids(&setup.exe, Want::SameKindAs(&running)).unwrap();
    assert_eq!(same.len(), 1);
    assert!(all.contains(&same[0]));
    assert!(matches!(
        (&running, &same[0]),
        (ImageId::Sha256(_), ImageId::Sha256(_)) | (ImageId::CdHash(_), ImageId::CdHash(_))
    ));
    // 全文哈希独立算一遍
    use sha2::{Digest, Sha256};
    let sha = ImageId::Sha256(Sha256::digest(std::fs::read(&setup.exe).unwrap()).into());
    assert!(all.contains(&sha));
    let missing = setup.exe.with_file_name("missing");
    assert!(file_image_ids(&missing, Want::All).is_err());
}

#[cfg(target_os = "macos")]
mod bundle {
    use std::path::Path;
    use std::process::Command;

    use iwork_ipc::peer::verify_bundle;

    fn sealed_app(dir: &Path) -> std::path::PathBuf {
        let app = dir.join("Probe.app");
        let contents = app.join("Contents");
        std::fs::create_dir_all(contents.join("MacOS")).unwrap();
        std::fs::create_dir_all(contents.join("Resources")).unwrap();
        std::fs::write(
            contents.join("Info.plist"),
            r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0"><dict>
  <key>CFBundleIdentifier</key><string>dev.iwork.test.sealed</string>
  <key>CFBundleExecutable</key><string>main</string>
</dict></plist>
"#,
        )
        .unwrap();
        std::fs::copy(
            std::env::current_exe().unwrap(),
            contents.join("MacOS/main"),
        )
        .unwrap();
        std::fs::write(contents.join("Resources/data.txt"), b"resource").unwrap();
        // 嵌套程序：在包外单独签好再放进来（在包里对它的路径签名，codesign 会当成签整个包），然后签整包
        let helper = dir.join("helper.staged");
        std::fs::copy(std::env::current_exe().unwrap(), &helper).unwrap();
        codesign(&["-f", "-s", "-", "-i", "dev.iwork.test.helper"], &helper);
        std::fs::rename(&helper, contents.join("MacOS/helper")).unwrap();
        codesign(&["-f", "-s", "-"], &app);
        app
    }

    fn codesign(args: &[&str], path: &Path) {
        let out = Command::new("codesign")
            .args(args)
            .arg(path)
            .output()
            .unwrap();
        assert!(
            out.status.success(),
            "{}",
            String::from_utf8_lossy(&out.stderr)
        );
    }

    /// 签名覆盖范围中间某一页里的一个字节取反：签名本身（代码目录、cdhash）不变，只有逐页核对才看得出来。
    fn flip_byte_in_signed_page(path: &Path) {
        use std::io::{Read, Seek, SeekFrom, Write};
        let out = Command::new("codesign")
            .arg("-dvvv")
            .arg(path)
            .output()
            .unwrap();
        let text = String::from_utf8_lossy(&out.stderr);
        let pages: u64 = text
            .split_whitespace()
            .find_map(|w| w.strip_prefix("hashes="))
            .and_then(|h| h.split('+').next())
            .and_then(|n| n.parse().ok())
            .unwrap_or_else(|| panic!("codesign 没给出 hashes=：{text}"));
        let offset = pages / 2 * 4096 + 2048;
        let mut f = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(path)
            .unwrap();
        let mut b = [0u8; 1];
        f.seek(SeekFrom::Start(offset)).unwrap();
        f.read_exact(&mut b).unwrap();
        f.seek(SeekFrom::Start(offset)).unwrap();
        f.write_all(&[b[0] ^ 0xff]).unwrap();
    }

    /// 「含嵌套代码」真的生效（第 1 轮审查 F-5）：嵌套程序的一个代码页被改、它自己的签名（cdhash）不变时，
    /// 外层封印记的是嵌套程序的 cdhash，看不出来；只有逐个验嵌套代码才不过。去掉嵌套那一项，这条会变红。
    #[test]
    fn verify_bundle_checks_nested_code_pages() {
        let root = tempfile::tempdir().unwrap();
        let app = sealed_app(root.path());
        verify_bundle(&app).expect("完整的包应当通过");
        flip_byte_in_signed_page(&app.join("Contents/MacOS/helper"));
        let err = verify_bundle(&app).expect_err("嵌套程序被改应当不过");
        assert!(err.to_string().contains("OSStatus"), "{err}");
    }

    /// 整包验签：完整的包通过；拿走一个资源、多放一个文件都不过；放回去之后再查又通过（不记上次的结果）。
    #[test]
    fn verify_bundle_sees_every_file() {
        let root = tempfile::tempdir().unwrap();
        let app = sealed_app(root.path());
        verify_bundle(&app).expect("完整的包应当通过");

        let res = app.join("Contents/Resources/data.txt");
        let aside = root.path().join("data.txt");
        std::fs::rename(&res, &aside).unwrap();
        assert!(verify_bundle(&app).is_err(), "少一个资源应当不过");
        std::fs::rename(&aside, &res).unwrap();
        verify_bundle(&app).expect("放回去之后应当通过");

        let extra = app.join("Contents/Resources/extra.txt");
        std::fs::write(&extra, b"x").unwrap();
        assert!(verify_bundle(&app).is_err(), "多一个没登记的文件应当不过");
        std::fs::remove_file(&extra).unwrap();

        // 没有封印的包（只有链接器签名）不算通过
        std::fs::remove_dir_all(app.join("Contents/_CodeSignature")).unwrap();
        assert!(verify_bundle(&app).is_err());
    }
}
