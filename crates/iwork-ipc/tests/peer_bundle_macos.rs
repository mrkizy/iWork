//! 真实 .app 包里的对端身份（T04 N5 A1，`t0-findings.md` 7.1）：无证书构建的包**不做整包签名**，
//! 主程序只有链接器的 ad-hoc 签名。这种未封印的 .app 里，主程序（`CFBundleExecutable`）的静态代码对象代表整个包，
//! 默认参数的静态校验会报「签名说有资源、却找不到」（-67056）。这里证明：
//! ① 主程序位置上 `SelfIdentity::detect` 能算出自己的 cdhash；
//! ② 后台按清单认得出它；③ 对照：换成清单外的映像（换签名标识重签，cdhash 必变）仍被拒，而且拒的理由是
//!    「不在清单里」，不是「取不到身份」——说明校验仍在比代码，只是不再要求包资源。
//!
//! 「主程序」就是本测试二进制自己：复制进临时 .app 的 `Contents/MacOS/iwork-client`，带 [`CHILD_ENV`] 只跑
//! [`bundle_child_entry`]。期望的 cdhash 由 `codesign -dvvv` 独立读出，不经被测代码。
#![cfg(target_os = "macos")]

use std::io::Read;
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Output, Stdio};
use std::time::Duration;

use iwork_ipc::LocalListener;
use iwork_ipc::peer::{
    ImageId, Manifest, PeerIdentity, PeerVerdict, RejectReason, SelfIdentity, SignerIdentity,
    TrustPolicy, UserId, judge_peer,
};
use iwork_ipc::transport::{Connection, Listener};

/// 子进程模式的开关：值是 `detect`（只算自己的身份并打印）或要连的 socket 路径。
const CHILD_ENV: &str = "IWORK_T04_BUNDLE_CHILD";
const CHILD_TEST_NAME: &str = "bundle_child_entry";
const MAIN_EXE: &str = "iwork-client";
const ACCEPT_TIMEOUT: Duration = Duration::from_secs(20);

/// 子进程入口。没有 [`CHILD_ENV`] 时什么都不做（正常跑测试时它是一个空测试）。
#[test]
fn bundle_child_entry() {
    let Some(mode) = std::env::var_os(CHILD_ENV) else {
        return;
    };
    if mode == "detect" {
        match SelfIdentity::detect() {
            Ok(own) => println!("detect=ok signer={:?}", own.signer),
            Err(e) => println!("detect=err {e}"),
        }
        return;
    }
    let mut stream = UnixStream::connect(mode).expect("子进程连 socket 失败");
    // 读到 EOF（服务端判定完关掉连接）再退出，保证判定时进程还活着。
    let _ = stream.read(&mut [0u8; 1]);
}

/// 一个未封印的临时 .app：只有 Info.plist 与主程序，没有 `_CodeSignature`（tauri build 无签名身份时的样子）。
struct App {
    _root: tempfile::TempDir,
    macos_dir: PathBuf,
    socket: PathBuf,
}

impl App {
    fn new() -> App {
        let root = tempfile::tempdir().unwrap();
        // 与内核、Security 框架给出的路径同一种写法（/private/var/…），「同安装目录」才比得上。
        let base = root.path().canonicalize().unwrap();
        let contents = base.join("iWork.app/Contents");
        let macos_dir = contents.join("MacOS");
        std::fs::create_dir_all(&macos_dir).unwrap();
        std::fs::write(
            contents.join("Info.plist"),
            format!(
                r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0"><dict>
  <key>CFBundleIdentifier</key><string>dev.iwork.test.bundle</string>
  <key>CFBundleExecutable</key><string>{MAIN_EXE}</string>
  <key>CFBundlePackageType</key><string>APPL</string>
</dict></plist>
"#
            ),
        )
        .unwrap();
        App {
            socket: base.join("t.sock"),
            _root: root,
            macos_dir,
        }
    }

    /// 把本测试二进制放成包的主程序；`identifier` 给出时换签名标识做 ad-hoc 重签（cdhash 随之改变）。
    fn place_main(&self, identifier: Option<&str>) -> PathBuf {
        let path = self.macos_dir.join(MAIN_EXE);
        // 先在包外准备好再挪进来：在包里对主程序路径签名，codesign 会把它当成签整个包。
        let staged = self
            .macos_dir
            .parent()
            .unwrap()
            .parent()
            .unwrap()
            .with_extension("staged");
        std::fs::copy(std::env::current_exe().unwrap(), &staged).unwrap();
        if let Some(id) = identifier {
            run_ok(
                Command::new("codesign")
                    .args(["-f", "-s", "-", "-i", id])
                    .arg(&staged),
            );
        }
        std::fs::rename(&staged, &path).unwrap();
        assert!(
            !self
                .macos_dir
                .parent()
                .unwrap()
                .join("_CodeSignature")
                .exists(),
            "测试前提：包没有封印"
        );
        path
    }

    /// 后台服务的自我身份：清单只登记 `listed`，安装目录是包的 `Contents/MacOS`。
    fn daemon_identity(&self, listed: &ImageId) -> SelfIdentity {
        let json = format!(
            r#"{{"manifest_version":1,"app_version":"0.0.0-test","binaries":{{"{MAIN_EXE}":["{listed}"]}}}}"#
        );
        SelfIdentity {
            user: current_uid(),
            // 后台自己的身份不参与判定无证书的对端，只决定走清单这一条路。
            signer: SignerIdentity::Image(listed.clone()),
            exe_dir: self.macos_dir.clone(),
            manifest: Some(Manifest::parse(&json).unwrap()),
        }
    }
}

fn run_ok(cmd: &mut Command) -> Output {
    let out = cmd.output().expect("起命令失败");
    assert!(
        out.status.success(),
        "{cmd:?} 失败：{}",
        String::from_utf8_lossy(&out.stderr)
    );
    out
}

/// `codesign -dvvv` 读出的 cdhash：期望值的独立来源。
fn cdhash_by_codesign(path: &Path) -> ImageId {
    let out = run_ok(Command::new("codesign").arg("-dvvv").arg(path));
    let text = String::from_utf8_lossy(&out.stderr);
    let hex = text
        .lines()
        .find_map(|l| l.strip_prefix("CDHash="))
        .unwrap_or_else(|| panic!("codesign 没给出 CDHash：{text}"));
    ImageId::CdHash(hex::decode(hex.trim()).unwrap())
}

fn current_uid() -> UserId {
    // SAFETY: geteuid 没有参数、不会失败、不读写任何内存。
    UserId::Uid(unsafe { libc::geteuid() })
}

fn child(exe: &Path, mode: &std::ffi::OsStr) -> Command {
    let mut cmd = Command::new(exe);
    cmd.args([
        "--exact",
        CHILD_TEST_NAME,
        "--test-threads=1",
        "--nocapture",
    ])
    .env(CHILD_ENV, mode);
    cmd
}

/// 跑着的对端进程；测试失败提前返回时也不留孤儿。
struct Peer(Child);

impl Drop for Peer {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

async fn identify(app: &App, exe: &Path) -> (Peer, PeerIdentity) {
    let mut listener = LocalListener::bind(&app.socket).unwrap();
    let peer = Peer(
        child(exe, app.socket.as_os_str())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap(),
    );
    let conn = tokio::time::timeout(ACCEPT_TIMEOUT, listener.accept())
        .await
        .expect("对端没有连上")
        .unwrap();
    let identity = conn.peer().await.unwrap();
    (peer, identity)
}

#[test]
fn unsealed_app_main_executable_detects_its_own_identity() {
    let app = App::new();
    let exe = app.place_main(None);
    let expected = cdhash_by_codesign(&exe);

    let out = child(&exe, "detect".as_ref()).output().unwrap();
    let stdout = String::from_utf8_lossy(&out.stdout);
    // libtest 把「test bundle_child_entry ... 」打在同一行前面，按标记截取。
    let line = stdout
        .lines()
        .find_map(|l| l.find("detect=").map(|i| &l[i..]))
        .unwrap_or_else(|| panic!("子进程没有输出结果：{stdout}"));
    assert_eq!(
        line,
        format!("detect=ok signer={:?}", SignerIdentity::Image(expected))
    );
}

#[tokio::test]
async fn listed_main_executable_of_unsealed_app_is_accepted() {
    let app = App::new();
    let exe = app.place_main(None);
    let expected = cdhash_by_codesign(&exe);
    let (peer, identity) = identify(&app, &exe).await;

    assert_eq!(identity.pid, peer.0.id());
    assert_eq!(identity.exe.as_deref(), Some(exe.as_path()));
    assert_eq!(identity.signer, SignerIdentity::Image(expected.clone()));
    assert_eq!(
        judge_peer(
            &app.daemon_identity(&expected),
            &identity,
            TrustPolicy::RequireSigner
        ),
        PeerVerdict::Accepted {
            signature_checked: true
        }
    );
}

#[tokio::test]
async fn main_executable_outside_manifest_is_rejected_as_not_trusted() {
    let app = App::new();
    let listed = cdhash_by_codesign(&std::env::current_exe().unwrap());
    let exe = app.place_main(Some("dev.iwork.test.other"));
    let other = cdhash_by_codesign(&exe);
    assert_ne!(other, listed, "重签之后 cdhash 应当变了");
    let (_peer, identity) = identify(&app, &exe).await;

    // 身份照样取得到（是它自己的 cdhash，不是 Unavailable），拒绝是因为它不在清单里。
    assert_eq!(identity.signer, SignerIdentity::Image(other.clone()));
    assert_eq!(
        judge_peer(
            &app.daemon_identity(&listed),
            &identity,
            TrustPolicy::RequireSigner
        ),
        PeerVerdict::Rejected(RejectReason::ImageNotTrusted { image: other })
    );
}
