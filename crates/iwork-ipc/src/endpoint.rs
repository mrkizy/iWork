//! 当前用户的本机端点：路径、目录创建与权限校验（设计 3.1、5.1；方案 2.3）。
//!
//! 目录权限与 DACL 是第一道门，挡的是其他用户；同用户的进程挡不住，那由 [`crate::peer::judge_peer`] 负责。
//! 不满足条件时返回带路径与原因的错误，**不自动修复**：悄悄 `chmod` 会把别人放进来的东西当成自己的。

use std::path::{Path, PathBuf};

use crate::transport::Endpoint;

/// socket 文件名。短名字给 macOS 的路径长度留余量。
pub const SOCKET_FILE: &str = "d.sock";

/// `sun_path` 能放的最大字节数（不含结尾 0）。
#[cfg(target_os = "macos")]
const MAX_SOCKET_PATH: usize = 103;
#[cfg(all(unix, not(target_os = "macos")))]
const MAX_SOCKET_PATH: usize = 107;

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum EndpointError {
    #[error(
        "socket 路径 {path} 有 {len} 字节，超过上限 {max}；请缩短用户目录或设置 XDG_RUNTIME_DIR"
    )]
    PathTooLong {
        path: PathBuf,
        len: usize,
        max: usize,
    },
    #[error(
        "目录 {path} 权限不安全：权限位 {found_mode:o}、属主 uid {owner}，要求属主是当前用户且权限 0700；请自行修正或删除后重启"
    )]
    InsecureDirectory {
        path: PathBuf,
        found_mode: u32,
        owner: u32,
    },
    #[error("目录 {path} 是符号链接，拒绝使用")]
    IsSymlink { path: PathBuf },
    #[error("找不到用户目录（HOME 未设置）")]
    NoHome,
    #[error("准备目录 {path} 失败：{message}")]
    Io { path: PathBuf, message: String },
}

/// 当前用户的端点。daemon（T04）与客户端（T09）都调 [`LocalEndpoint::for_current_user`]。
#[derive(Debug, Clone)]
pub struct LocalEndpoint {
    endpoint: Endpoint,
    run_dir: Option<PathBuf>,
}

impl LocalEndpoint {
    /// 当前用户的端点。只算路径、建目录、验权限，不绑定。
    pub fn for_current_user() -> Result<LocalEndpoint, EndpointError> {
        #[cfg(unix)]
        {
            let home = std::env::home_dir();
            let run_dir = unix_run_dir(|k| std::env::var_os(k), home)?;
            Self::in_run_dir(&run_dir)
        }
        #[cfg(windows)]
        {
            let user = crate::peer::current_user_sid().map_err(|e| EndpointError::Io {
                path: PathBuf::new(),
                message: e.to_string(),
            })?;
            Ok(LocalEndpoint {
                endpoint: Endpoint::Pipe(pipe_name(&user)),
                run_dir: None,
            })
        }
    }

    /// 在指定的 run 目录里建端点：按 0700 创建目录，已存在则校验不是符号链接、属主本人、`mode & 0o077 == 0`；
    /// 校验 socket 路径长度。
    #[cfg(unix)]
    pub fn in_run_dir(run_dir: &Path) -> Result<LocalEndpoint, EndpointError> {
        let socket = run_dir.join(SOCKET_FILE);
        let len = socket.as_os_str().len();
        if len > MAX_SOCKET_PATH {
            return Err(EndpointError::PathTooLong {
                path: socket,
                len,
                max: MAX_SOCKET_PATH,
            });
        }
        ensure_private_dir(run_dir)?;
        Ok(LocalEndpoint {
            endpoint: Endpoint::Unix(socket),
            run_dir: Some(run_dir.to_path_buf()),
        })
    }

    pub fn endpoint(&self) -> &Endpoint {
        &self.endpoint
    }

    /// Unix 上 socket 所在目录（`daemon.lock` 也放这里，T04 用）；Windows 为 `None`。
    pub fn run_dir(&self) -> Option<&Path> {
        self.run_dir.as_deref()
    }
}

/// Windows 命名管道名：每个用户一个，名字里带 SID。
#[cfg(windows)]
pub fn pipe_name(user_sid: &str) -> String {
    format!(r"\\.\pipe\iwork-{user_sid}")
}

/// run 目录（方案 2.3 的路径表）。环境变量与家目录由调用方传入，测试不碰真实环境。
#[cfg(unix)]
pub fn unix_run_dir(
    env: impl Fn(&str) -> Option<std::ffi::OsString>,
    home: Option<PathBuf>,
) -> Result<PathBuf, EndpointError> {
    let non_empty = |k: &str| env(k).filter(|v| !v.is_empty()).map(PathBuf::from);
    if cfg!(target_os = "macos") {
        let home = home.ok_or(EndpointError::NoHome)?;
        return Ok(home.join("Library/Application Support/iWork/run"));
    }
    if let Some(runtime) = non_empty("XDG_RUNTIME_DIR") {
        return Ok(runtime.join("iwork"));
    }
    let data = match non_empty("XDG_DATA_HOME") {
        Some(d) => d,
        None => home.ok_or(EndpointError::NoHome)?.join(".local/share"),
    };
    Ok(data.join("iwork/run"))
}

#[cfg(unix)]
fn ensure_private_dir(dir: &Path) -> Result<(), EndpointError> {
    use std::os::unix::fs::{DirBuilderExt, MetadataExt};

    let io_err = |e: std::io::Error| EndpointError::Io {
        path: dir.to_path_buf(),
        message: e.to_string(),
    };
    if let Some(parent) = dir.parent() {
        std::fs::create_dir_all(parent).map_err(io_err)?;
    }
    match std::fs::DirBuilder::new().mode(0o700).create(dir) {
        Ok(()) => {}
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {}
        Err(e) => return Err(io_err(e)),
    }
    let meta = std::fs::symlink_metadata(dir).map_err(io_err)?;
    if meta.file_type().is_symlink() {
        return Err(EndpointError::IsSymlink {
            path: dir.to_path_buf(),
        });
    }
    // SAFETY: geteuid 没有前置条件，不会失败。
    let me = unsafe { libc::geteuid() };
    let mode = meta.mode() & 0o7777;
    if !meta.is_dir() || meta.uid() != me || mode & 0o077 != 0 {
        return Err(EndpointError::InsecureDirectory {
            path: dir.to_path_buf(),
            found_mode: mode,
            owner: meta.uid(),
        });
    }
    Ok(())
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    fn mode_of(p: &Path) -> u32 {
        std::fs::metadata(p).unwrap().permissions().mode() & 0o7777
    }

    #[test]
    fn new_run_dir_is_created_0700() {
        let tmp = tempfile::tempdir().unwrap();
        let run = tmp.path().join("iwork/run");
        let ep = LocalEndpoint::in_run_dir(&run).unwrap();
        assert_eq!(mode_of(&run), 0o700);
        assert_eq!(ep.endpoint(), &Endpoint::Unix(run.join(SOCKET_FILE)));
        assert_eq!(ep.run_dir(), Some(run.as_path()));
        // 再来一次：已存在且合规，照样通过。
        LocalEndpoint::in_run_dir(&run).unwrap();
    }

    #[test]
    fn existing_0755_dir_is_rejected_not_repaired() {
        let tmp = tempfile::tempdir().unwrap();
        let run = tmp.path().join("run");
        std::fs::create_dir(&run).unwrap();
        std::fs::set_permissions(&run, std::fs::Permissions::from_mode(0o755)).unwrap();
        let err = LocalEndpoint::in_run_dir(&run).unwrap_err();
        assert!(
            matches!(
                err,
                EndpointError::InsecureDirectory {
                    found_mode: 0o755,
                    ..
                }
            ),
            "{err:?}"
        );
        assert_eq!(mode_of(&run), 0o755, "不自动修权限");
    }

    #[test]
    fn symlinked_dir_is_rejected() {
        let tmp = tempfile::tempdir().unwrap();
        let real = tmp.path().join("real");
        std::fs::create_dir(&real).unwrap();
        std::fs::set_permissions(&real, std::fs::Permissions::from_mode(0o700)).unwrap();
        let link = tmp.path().join("link");
        std::os::unix::fs::symlink(&real, &link).unwrap();
        assert_eq!(
            LocalEndpoint::in_run_dir(&link).unwrap_err(),
            EndpointError::IsSymlink { path: link }
        );
    }

    #[test]
    fn too_long_socket_path_is_rejected() {
        let tmp = tempfile::tempdir().unwrap();
        let long = tmp.path().join("x".repeat(MAX_SOCKET_PATH));
        assert!(matches!(
            LocalEndpoint::in_run_dir(&long).unwrap_err(),
            EndpointError::PathTooLong {
                max: MAX_SOCKET_PATH,
                ..
            }
        ));
        assert!(!long.exists(), "路径超长时不建目录");
    }

    #[test]
    fn run_dir_follows_platform_table() {
        let home = Some(PathBuf::from("/home/u"));
        let env = |vars: &'static [(&'static str, &'static str)]| {
            move |k: &str| vars.iter().find(|(n, _)| *n == k).map(|(_, v)| (*v).into())
        };
        if cfg!(target_os = "macos") {
            assert_eq!(
                unix_run_dir(env(&[("XDG_RUNTIME_DIR", "/run/user/1")]), home).unwrap(),
                PathBuf::from("/home/u/Library/Application Support/iWork/run")
            );
        } else {
            assert_eq!(
                unix_run_dir(env(&[("XDG_RUNTIME_DIR", "/run/user/1")]), home.clone()).unwrap(),
                PathBuf::from("/run/user/1/iwork")
            );
            assert_eq!(
                unix_run_dir(env(&[("XDG_DATA_HOME", "/d")]), home.clone()).unwrap(),
                PathBuf::from("/d/iwork/run")
            );
            assert_eq!(
                unix_run_dir(env(&[("XDG_RUNTIME_DIR", "")]), home).unwrap(),
                PathBuf::from("/home/u/.local/share/iwork/run")
            );
        }
        assert_eq!(
            unix_run_dir(env(&[]), None).unwrap_err(),
            EndpointError::NoHome
        );
    }
}
