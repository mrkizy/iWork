//! 会话目录：建、写、删、清扫（T06 方案 3.3）。用临时目录当数据目录。

use std::fs;
use std::os::unix::fs::{MetadataExt, PermissionsExt};

use super::*;

fn data() -> tempfile::TempDir {
    tempfile::tempdir().unwrap()
}

#[test]
fn creates_private_dir_with_owner_and_short_name() {
    let d = data();
    let dir = SessionDir::create(d.path()).unwrap();
    let name = dir.path().file_name().unwrap().to_str().unwrap().to_owned();
    assert_eq!(name.len(), 12);
    assert!(name.bytes().all(|b| b.is_ascii_hexdigit()));
    assert_eq!(dir.path().parent().unwrap(), root(d.path()));
    let meta = fs::metadata(dir.path()).unwrap();
    assert_eq!(meta.mode() & 0o777, 0o700);
    assert_eq!(
        fs::read_to_string(dir.files().owner).unwrap(),
        std::process::id().to_string()
    );
    // 两次建的不是同一个
    let other = SessionDir::create(d.path()).unwrap();
    assert_ne!(other.path(), dir.path());
}

#[test]
fn config_files_are_0600_and_written_once() {
    let d = data();
    let dir = SessionDir::create(d.path()).unwrap();
    dir.write_config(b"{}", b"{\"mcpServers\":{}}").unwrap();
    for f in [dir.files().settings, dir.files().mcp] {
        assert_eq!(fs::metadata(&f).unwrap().mode() & 0o777, 0o600, "{f:?}");
    }
    assert!(
        dir.write_config(b"{}", b"{}").is_err(),
        "不覆盖已有的设置文件"
    );
}

#[test]
fn remove_deletes_only_own_dir() {
    let d = data();
    let dir = SessionDir::create(d.path()).unwrap();
    dir.write_config(b"{}", b"{}").unwrap();
    let path = dir.path().to_owned();
    dir.remove().unwrap();
    assert!(!path.exists());
    assert!(root(d.path()).exists(), "上一层留着");

    // 目录里多了不认识的文件：不删
    let dir = SessionDir::create(d.path()).unwrap();
    fs::write(dir.path().join("stranger"), b"x").unwrap();
    let path = dir.path().to_owned();
    assert!(dir.remove().is_err());
    assert!(path.join("stranger").exists());
}

#[test]
fn refuses_root_that_is_not_private() {
    let d = data();
    let r = root(d.path());
    fs::create_dir_all(&r).unwrap();
    fs::set_permissions(&r, fs::Permissions::from_mode(0o755)).unwrap();
    let err = SessionDir::create(d.path()).unwrap_err();
    assert!(err.0.contains("组或别人"), "{err}");

    // 符号链接冒充
    let d = data();
    let target = d.path().join("elsewhere");
    fs::create_dir_all(&target).unwrap();
    fs::set_permissions(&target, fs::Permissions::from_mode(0o700)).unwrap();
    fs::create_dir_all(d.path().join("run")).unwrap();
    std::os::unix::fs::symlink(&target, root(d.path())).unwrap();
    let err = SessionDir::create(d.path()).unwrap_err();
    assert!(err.0.contains("符号链接"), "{err}");
}

#[test]
fn socket_path_length_is_checked() {
    let long = PathBuf::from(format!("/{}", "x".repeat(SOCKET_PATH_MAX)));
    let err = check_socket_len(&long).unwrap_err();
    assert!(err.0.contains("太长"), "{err}");
    let edge = PathBuf::from(format!("/{}", "x".repeat(SOCKET_PATH_MAX - 1)));
    assert!(check_socket_len(&edge).is_ok(), "对照：恰好到上限");
    // 数据目录很深时建目录前就报错，不留下半截目录
    let d = data();
    let deep = d.path().join("y".repeat(SOCKET_PATH_MAX));
    fs::create_dir_all(&deep).unwrap();
    assert!(SessionDir::create(&deep).is_err());
    assert_eq!(fs::read_dir(root(&deep)).unwrap().count(), 0);
}

#[test]
fn sweep_removes_only_dead_owners_dirs() {
    let d = data();
    // 属主还活着（本进程）
    let live = SessionDir::create(d.path()).unwrap();
    // 属主已经不在：起一个马上退出的子进程，拿它的进程号
    let dead = SessionDir::create(d.path()).unwrap();
    let mut child = std::process::Command::new("true").spawn().unwrap();
    let dead_pid = child.id();
    child.wait().unwrap();
    fs::write(dead.files().owner, dead_pid.to_string()).unwrap();
    dead.write_config(b"{}", b"{}").unwrap();
    // 名字不对的目录、里面有别的文件的目录：不碰
    let r = root(d.path());
    let foreign = r.join("not-ours");
    fs::create_dir(&foreign).unwrap();
    let crowded = SessionDir::create(d.path()).unwrap();
    fs::write(crowded.files().owner, dead_pid.to_string()).unwrap();
    fs::write(crowded.path().join("stranger"), b"x").unwrap();

    assert_eq!(sweep(d.path()).unwrap(), 1);
    assert!(live.path().exists());
    assert!(!dead.path().exists());
    assert!(foreign.exists());
    assert!(crowded.path().exists());
    // 没有 run/h 时什么都不做
    assert_eq!(sweep(data().path()).unwrap(), 0);
}
