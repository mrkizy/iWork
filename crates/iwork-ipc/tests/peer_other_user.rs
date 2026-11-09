//! 验收 1：不同用户的连接被拒，两个方向都测（Linux CI 以另一个用户身份连接；方案 7.1）。
//!
//! 需要：一个已存在的另一用户、能以它的身份跑命令、dev 构建的 `ipc-tool`。CI 的做法：
//! ```text
//! sudo useradd -m iwork-other
//! cargo build -p iwork-ipc --features dev --example ipc-tool
//! IWORK_TEST_OTHER_USER=1 IWORK_IPC_TOOL=$PWD/target/debug/examples/ipc-tool \
//!   cargo test -p iwork-ipc --features dev --test peer_other_user -- --ignored
//! ```
//! `IWORK_RUN_AS` 改「以另一用户运行」的命令前缀（默认 `sudo -n -u iwork-other`；容器里以 root 跑可用 `runuser -u iwork-other --`）。
//! socket 放在 0777 的临时目录里，专门绕过目录权限这道门，测的是第 1 步（同用户判定）本身；
//! 两端都用 `DevTrust`，证明 dev 开关也不跳过第 1 步。
#![cfg(all(target_os = "linux", feature = "dev"))]

mod support;

use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::Duration;

use iwork_ipc::client::{Client, ClientConfig, ConnectError};
use iwork_ipc::peer::{RejectReason, SelfIdentity, TrustPolicy};
use iwork_ipc::server::{Server, ServerConfig};
use iwork_ipc::transport::Endpoint;
use iwork_ipc::{LocalConnector, LocalListener};
use iwork_proto::pb;
use support::{Captured, TestHandler, World, WorldSource};

fn run_as() -> Vec<String> {
    std::env::var("IWORK_RUN_AS")
        .unwrap_or_else(|_| "sudo -n -u iwork-other".into())
        .split_whitespace()
        .map(str::to_owned)
        .collect()
}

/// 以另一用户、umask 000 运行 ipc-tool（让它建的 socket 本用户也能连上，好测判定而不是文件权限）。
fn other_user_tool(tool: &Path, args: &[&str]) -> Command {
    let prefix = run_as();
    let mut cmd = Command::new(&prefix[0]);
    cmd.args(&prefix[1..]).arg("sh").arg("-c").arg("umask 000; exec \"$0\" \"$@\"").arg(tool).args(args);
    cmd
}

fn open_dir() -> tempfile::TempDir {
    let dir = tempfile::Builder::new().prefix("iwork-other-user").tempdir_in("/tmp").unwrap();
    std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(0o777)).unwrap();
    dir
}

fn dev_client_cfg() -> ClientConfig {
    let mut cfg = ClientConfig::new(pb::ClientKind::DevTool, "test");
    cfg.trust = TrustPolicy::DevTrust;
    cfg
}

#[tokio::test]
#[ignore = "需要另一个系统用户：CI Linux 设 IWORK_TEST_OTHER_USER=1 跑"]
async fn other_user_is_rejected_both_directions() {
    assert_eq!(std::env::var("IWORK_TEST_OTHER_USER").as_deref(), Ok("1"), "需要 IWORK_TEST_OTHER_USER=1");
    let tool = PathBuf::from(std::env::var("IWORK_IPC_TOOL").expect("需要 IWORK_IPC_TOOL 指向 dev 构建的 ipc-tool"));
    let (audit, _guard) = Captured::install();
    let own = SelfIdentity::detect().expect("本进程身份");

    // 方向一：本用户的后台，另一用户的客户端连进来 → 后台拒绝（不发任何帧）、审计 DifferentUser。
    let dir = open_dir();
    let sock = dir.path().join("d.sock");
    let world = World::default();
    let mut cfg = ServerConfig::new("other-user-test");
    cfg.trust = TrustPolicy::DevTrust;
    let (server, _publisher) = Server::new(cfg, own.clone(), TestHandler::new(world.clone()), WorldSource::new(world));
    let listener = LocalListener::bind(&sock).unwrap();
    std::fs::set_permissions(&sock, std::fs::Permissions::from_mode(0o777)).unwrap();
    let shutdown = tokio_util::sync::CancellationToken::new();
    let serving = tokio::spawn(server.serve(listener, shutdown.clone()));

    let out = tokio::task::spawn_blocking({
        let (tool, sock) = (tool.clone(), sock.clone());
        move || other_user_tool(&tool, &["--endpoint", sock.to_str().unwrap(), "hello"]).output().unwrap()
    })
    .await
    .unwrap();
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(!out.status.success(), "另一用户不该连上：{stdout}");
    assert!(!stdout.contains("WELCOME"), "另一用户收到了 Welcome：{stdout}");
    tokio::time::sleep(Duration::from_millis(100)).await;
    let rejected = audit.events("peer_rejected");
    assert!(
        rejected.iter().any(|e| e.get("role").map(String::as_str) == Some("server")
            && e.get("reason").is_some_and(|r| r.starts_with("DifferentUser"))),
        "后台审计里应有 DifferentUser：{rejected:?}"
    );
    // 对照：同用户照常连上，后台没被拖垮。
    let (client, _inc) = Client::connect(&LocalConnector, &Endpoint::Unix(sock.clone()), &own, dev_client_cfg())
        .await
        .expect("同用户应能连上");
    assert!(!client.welcome().session_id.is_empty());
    shutdown.cancel();
    serving.await.unwrap();

    // 方向二：另一用户起的假后台，本用户的客户端连过去 → 客户端拒绝（不发 Hello），返回 DifferentUser。
    let dir2 = open_dir();
    let sock2 = dir2.path().join("d.sock");
    let mut fake = other_user_tool(&tool, &["--endpoint", sock2.to_str().unwrap(), "serve"])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    for _ in 0..100 {
        if sock2.exists() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    let result = Client::connect(&LocalConnector, &Endpoint::Unix(sock2), &own, dev_client_cfg()).await;
    let _ = Command::new(&run_as()[0]).args(&run_as()[1..]).args(["pkill", "-f", dir2.path().to_str().unwrap()]).status();
    let _ = fake.wait();
    match result {
        Err(ConnectError::PeerRejected(RejectReason::DifferentUser { .. })) => {}
        other => panic!("客户端应以 DifferentUser 拒绝另一用户的后台，得到 {:?}", other.map(|_| ())),
    }
}
