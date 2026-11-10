//! 7.1 验收 2 的 Linux 集成测试（T03 方案 2.4a、2.4b）：无证书构建按安装清单认对端，认的是对端
//! **运行中的映像**，不是它路径上现在放着的文件；清单在启动时读进内存，之后改磁盘上的清单不起作用。
//!
//! 「客户端二进制」就是本测试二进制自己：复制进临时安装目录后，带 [`CONNECT_ENV`] 只跑
//! [`peer_child_entry`]，它连上 socket 后一直等到服务端关闭连接。被改过的副本 = 原件末尾多一字节
//! （哈希不同、照样能运行）。服务端这一侧是真实的 [`LocalListener`] 与 [`Connection::peer`]。
#![cfg(target_os = "linux")]

use std::io::Read;
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::Mutex;
use std::time::Duration;

use iwork_ipc::LocalListener;
use iwork_ipc::peer::{
    ImageId, MANIFEST_FILE, Manifest, PeerIdentity, PeerVerdict, RejectReason, SelfIdentity,
    SignerIdentity, TrustPolicy, UserId, judge_peer,
};
use iwork_ipc::transport::{Connection, Listener};
use sha2::{Digest, Sha256};

/// 子进程模式的开关：值是要连的 socket 路径。
const CONNECT_ENV: &str = "IWORK_T03_PEER_CONNECT";
const CHILD_TEST_NAME: &str = "peer_child_entry";
const ACCEPT_TIMEOUT: Duration = Duration::from_secs(20);

/// 写可执行文件与起子进程互斥：另一个线程 fork 时会把本线程正在写的 fd 带进子进程，
/// 随后 exec 这个文件就报 ETXTBSY。
static SPAWN_LOCK: Mutex<()> = Mutex::new(());

/// 子进程入口。没有 [`CONNECT_ENV`] 时什么都不做（正常跑测试时它是一个空测试）。
#[test]
fn peer_child_entry() {
    let Some(socket) = std::env::var_os(CONNECT_ENV) else {
        return;
    };
    let mut stream = UnixStream::connect(socket).expect("子进程连 socket 失败");
    // 读到 EOF（服务端判定完关掉连接）再退出，保证判定时进程还活着。
    let _ = stream.read(&mut [0u8; 1]);
}

/// 一个临时安装目录：`iwork-client` 原件、多一字节的副本、socket、按需写出的清单。
struct Install {
    _root: tempfile::TempDir,
    dir: PathBuf,
    socket: PathBuf,
    original: Vec<u8>,
    tampered: Vec<u8>,
    // 测试二进制有几十 MB，哈希只算一次。
    original_id: ImageId,
    tampered_id: ImageId,
}

impl Install {
    fn new() -> Install {
        let root = tempfile::tempdir().unwrap();
        let dir = root.path().join("install");
        std::fs::create_dir(&dir).unwrap();
        // 与内核 readlink 给出的路径同一种写法，「同安装目录」才比得上。
        let dir = dir.canonicalize().unwrap();
        let original = std::fs::read(std::env::current_exe().unwrap()).unwrap();
        let mut tampered = original.clone();
        tampered.push(0);
        Install {
            socket: root.path().join("t.sock"),
            _root: root,
            dir,
            original_id: sha256_image(&original),
            tampered_id: sha256_image(&tampered),
            original,
            tampered,
        }
    }

    /// 在安装目录里放一个可执行文件。
    fn place(&self, name: &str, bytes: &[u8]) -> PathBuf {
        use std::os::unix::fs::PermissionsExt;
        let path = self.dir.join(name);
        let _guard = SPAWN_LOCK.lock().unwrap();
        std::fs::write(&path, bytes).unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
        path
    }

    /// 原子地把 `name` 换成 `bytes`（先写旁边的临时文件再 rename 覆盖，和打包升级的换法一样）。
    fn replace(&self, name: &str, bytes: &[u8]) {
        let staged = self.place(&format!("{name}.staged"), bytes);
        std::fs::rename(staged, self.dir.join(name)).unwrap();
    }

    fn manifest_json(&self, ids: &[ImageId]) -> String {
        let entries: Vec<String> = ids.iter().map(|id| format!("\"{id}\"")).collect();
        format!(
            r#"{{"manifest_version":1,"app_version":"0.0.0-test","binaries":{{"iwork-client":[{}]}}}}"#,
            entries.join(",")
        )
    }

    /// 后台服务的自我身份：自己就是原件，清单只登记原件。
    fn self_identity(&self, manifest: Manifest) -> SelfIdentity {
        SelfIdentity {
            user: current_uid(),
            signer: SignerIdentity::Image(self.original_id.clone()),
            exe_dir: self.dir.clone(),
            manifest: Some(manifest),
        }
    }

    fn only_original_listed(&self) -> SelfIdentity {
        self.self_identity(
            Manifest::parse(&self.manifest_json(std::slice::from_ref(&self.original_id))).unwrap(),
        )
    }
}

fn sha256_image(bytes: &[u8]) -> ImageId {
    ImageId::Sha256(Sha256::digest(bytes).into())
}

fn current_uid() -> UserId {
    // SAFETY: geteuid 没有参数、不会失败、不读写任何内存。
    UserId::Uid(unsafe { libc::geteuid() })
}

/// 跑着的对端进程；测试失败提前返回时也不留孤儿。
struct Peer(Child);

impl Drop for Peer {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

fn spawn_peer(exe: &Path, socket: &Path) -> Peer {
    let _guard = SPAWN_LOCK.lock().unwrap();
    let child = Command::new(exe)
        .args(["--exact", CHILD_TEST_NAME, "--test-threads=1"])
        .env(CONNECT_ENV, socket)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap_or_else(|e| panic!("起对端 {} 失败：{e}", exe.display()));
    Peer(child)
}

/// 起对端、等它连上；连上之后先调 `while_connected`（换文件），再取对端身份。
async fn identify(
    install: &Install,
    exe: &Path,
    while_connected: impl FnOnce(),
) -> (Peer, PeerIdentity) {
    let mut listener = LocalListener::bind(&install.socket).unwrap();
    let peer = spawn_peer(exe, &install.socket);
    let conn = tokio::time::timeout(ACCEPT_TIMEOUT, listener.accept())
        .await
        .expect("对端没有连上")
        .unwrap();
    while_connected();
    let identity = conn.peer().await.unwrap();
    (peer, identity)
}

fn assert_accepted(verdict: PeerVerdict) {
    assert_eq!(
        verdict,
        PeerVerdict::Accepted {
            signature_checked: true
        }
    );
}

fn assert_not_trusted(verdict: PeerVerdict, image: ImageId) {
    assert_eq!(
        verdict,
        PeerVerdict::Rejected(RejectReason::ImageNotTrusted { image })
    );
}

#[tokio::test]
async fn original_image_listed_in_manifest_is_accepted() {
    let install = Install::new();
    let exe = install.place("iwork-client", &install.original);
    let (peer, identity) = identify(&install, &exe, || {}).await;

    assert_eq!(identity.pid, peer.0.id());
    assert_eq!(identity.exe.as_deref(), Some(exe.as_path()));
    assert_eq!(
        identity.signer,
        SignerIdentity::Image(install.original_id.clone())
    );
    assert_accepted(judge_peer(
        &install.only_original_listed(),
        &identity,
        TrustPolicy::RequireSigner,
    ));
}

#[tokio::test]
async fn copy_with_one_byte_changed_is_not_trusted() {
    let install = Install::new();
    let exe = install.place("iwork-client", &install.tampered);
    let (_peer, identity) = identify(&install, &exe, || {}).await;

    assert_not_trusted(
        judge_peer(
            &install.only_original_listed(),
            &identity,
            TrustPolicy::RequireSigner,
        ),
        install.tampered_id.clone(),
    );
}

#[tokio::test]
async fn original_stays_accepted_after_its_path_is_replaced_while_connected() {
    let install = Install::new();
    let exe = install.place("iwork-client", &install.original);
    let (_peer, identity) = identify(&install, &exe, || {
        install.replace("iwork-client", &install.tampered);
    })
    .await;

    // 对照：路径上现在确实是改过的那份，判定却按运行中的原件。
    let on_disk = std::fs::read(&exe).unwrap();
    assert_eq!(on_disk, install.tampered);
    // 内核给被覆盖的映像加 " (deleted)"；去掉后仍是安装目录里的这个名字。
    assert_eq!(identity.exe.as_deref(), Some(exe.as_path()));
    assert_accepted(judge_peer(
        &install.only_original_listed(),
        &identity,
        TrustPolicy::RequireSigner,
    ));
}

#[tokio::test]
async fn tampered_stays_rejected_after_its_path_is_restored_while_connected() {
    let install = Install::new();
    let exe = install.place("iwork-client", &install.tampered);
    let (_peer, identity) = identify(&install, &exe, || {
        install.replace("iwork-client", &install.original);
    })
    .await;

    let on_disk = std::fs::read(&exe).unwrap();
    assert_eq!(on_disk, install.original);
    assert_not_trusted(
        judge_peer(
            &install.only_original_listed(),
            &identity,
            TrustPolicy::RequireSigner,
        ),
        install.tampered_id.clone(),
    );
}

#[tokio::test]
async fn manifest_change_after_start_is_ignored() {
    let install = Install::new();
    let manifest_path = install.dir.join(MANIFEST_FILE);
    std::fs::write(
        &manifest_path,
        install.manifest_json(std::slice::from_ref(&install.original_id)),
    )
    .unwrap();
    // 「启动」：清单此刻读进内存。
    let own = install.self_identity(Manifest::load(&manifest_path).unwrap());

    let exe = install.place("iwork-client", &install.tampered);
    let (_peer, identity) = identify(&install, &exe, || {}).await;

    // 启动之后把对端哈希加进磁盘上的清单。
    std::fs::write(
        &manifest_path,
        install.manifest_json(&[install.original_id.clone(), install.tampered_id.clone()]),
    )
    .unwrap();

    assert_not_trusted(
        judge_peer(&own, &identity, TrustPolicy::RequireSigner),
        install.tampered_id.clone(),
    );
    // 对照：改过的清单若在启动时读到就会放行，证明上面的拒绝来自「不重读」，不是清单写错了。
    let restarted = install.self_identity(Manifest::load(&manifest_path).unwrap());
    assert_accepted(judge_peer(
        &restarted,
        &identity,
        TrustPolicy::RequireSigner,
    ));
}
