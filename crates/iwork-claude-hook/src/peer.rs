//! 本机 socket 对端的用户与进程号（T06 方案 2.4 第 2、3 步）。hook 与桥接共用这一处。
//!
//! macOS 用 `getpeereid` + `LOCAL_PEERPID`，Linux 用 `SO_PEERCRED`。只取这两样，不算对端映像身份：
//! 这条通道的信任判据是「同用户 + 进程号等于给定的桥接进程号 + 令牌」，每次工具调用都要做，
//! 用不上也付不起 `iwork-ipc` 那种读文件验签名的开销。
//! Windows（命名管道）随桥接一起在 S6 实现；之前这里不提供，hook 在 Windows 上一律拒绝。

#[cfg(unix)]
pub use unix::{current_user, peer_cred};

/// 对端凭据。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PeerCred {
    pub user: u32,
    pub pid: u32,
}

#[cfg(unix)]
mod unix {
    use std::io;
    use std::os::fd::AsRawFd;

    use super::PeerCred;

    /// 本进程的有效用户。
    pub fn current_user() -> u32 {
        // SAFETY: geteuid 没有参数，也不会失败。
        unsafe { libc::geteuid() }
    }

    /// 已连接本机 socket 的对端用户与进程号。
    pub fn peer_cred(conn: &impl AsRawFd) -> io::Result<PeerCred> {
        imp(conn.as_raw_fd())
    }

    fn pid_from(raw: libc::pid_t) -> io::Result<u32> {
        u32::try_from(raw)
            .ok()
            .filter(|p| *p > 0)
            .ok_or_else(|| io::Error::other(format!("对端进程号不合法：{raw}")))
    }

    #[cfg(target_vendor = "apple")]
    fn imp(fd: libc::c_int) -> io::Result<PeerCred> {
        let mut uid: libc::uid_t = 0;
        let mut gid: libc::gid_t = 0;
        // SAFETY: fd 由调用方借给我们、在本函数内有效；两个出参指向本地变量。
        if unsafe { libc::getpeereid(fd, &mut uid, &mut gid) } != 0 {
            return Err(io::Error::last_os_error());
        }
        let mut pid: libc::pid_t = 0;
        let mut len = size_of::<libc::pid_t>() as libc::socklen_t;
        // SAFETY: 同上；pid 与 len 指向本地变量，内核最多写 len 字节并回写实际长度。
        let rc = unsafe {
            libc::getsockopt(
                fd,
                libc::SOL_LOCAL,
                libc::LOCAL_PEERPID,
                (&raw mut pid).cast(),
                &raw mut len,
            )
        };
        if rc != 0 {
            return Err(io::Error::last_os_error());
        }
        if len as usize != size_of::<libc::pid_t>() {
            return Err(io::Error::other("LOCAL_PEERPID 写回的长度不对"));
        }
        Ok(PeerCred {
            user: uid,
            pid: pid_from(pid)?,
        })
    }

    #[cfg(not(target_vendor = "apple"))]
    fn imp(fd: libc::c_int) -> io::Result<PeerCred> {
        let mut cred = libc::ucred {
            pid: 0,
            uid: 0,
            gid: 0,
        };
        let mut len = size_of::<libc::ucred>() as libc::socklen_t;
        // SAFETY: fd 由调用方借给我们、在本函数内有效；cred 与 len 指向大小匹配的本地变量。
        let rc = unsafe {
            libc::getsockopt(
                fd,
                libc::SOL_SOCKET,
                libc::SO_PEERCRED,
                (&raw mut cred).cast(),
                &raw mut len,
            )
        };
        if rc != 0 {
            return Err(io::Error::last_os_error());
        }
        if len as usize != size_of::<libc::ucred>() {
            return Err(io::Error::other("SO_PEERCRED 写回的长度不对"));
        }
        Ok(PeerCred {
            user: cred.uid,
            pid: pid_from(cred.pid)?,
        })
    }
}

#[cfg(all(test, unix))]
mod tests {
    use std::os::unix::net::UnixStream;

    use super::*;

    /// 同一进程里的一对 socket：两端看到的对端都是自己。
    #[test]
    fn socketpair_peer_is_self() {
        let (a, b) = UnixStream::pair().unwrap();
        let want = PeerCred {
            user: current_user(),
            pid: std::process::id(),
        };
        assert_eq!(peer_cred(&a).unwrap(), want);
        assert_eq!(peer_cred(&b).unwrap(), want);
    }
}
