//! Linux 的进程身份（方案 2.4、2.4b；实测 N3）。
//!
//! 映像身份不按路径取：打开 `/proc/<pid>/exe` 得到的是指向运行中映像 inode 的 fd，哈希从这个 fd 读。
//! N3 实测（`tools/t0/iwork-t03-linux-procexe.sh`）：映像文件被 `mv` 走、`mv` 覆盖、`rm` 之后，读到的
//! 仍是启动时那份；原地写入被内核以 `ETXTBSY` 拒绝。路径也从同一个 fd 反查（`/proc/self/fd/<n>`），
//! 所以哈希与路径一定说的是同一个 inode。

use std::fs::File;
use std::io::{self, Read};
use std::os::fd::{AsRawFd, BorrowedFd};
use std::path::{Path, PathBuf};

use sha2::{Digest, Sha256};

use super::{ImageId, PeerError, PeerIdentity, SignerIdentity, UserId};

/// 内核在映像文件被删除或被改名覆盖后，给 `readlink` 结果加的后缀。
const DELETED_SUFFIX: &str = " (deleted)";

pub(crate) fn current_user() -> Result<UserId, PeerError> {
    // SAFETY: geteuid 没有参数、不会失败、不读写任何内存。
    Ok(UserId::Uid(unsafe { libc::geteuid() }))
}

pub(crate) fn own_image() -> Result<(SignerIdentity, PathBuf), PeerError> {
    let (image, exe) =
        running_image(Path::new("/proc/self/exe")).map_err(PeerError::SelfIdentity)?;
    Ok((SignerIdentity::Image(image), exe))
}

/// socket 对端的身份：`SO_PEERCRED` 取 uid 与 pid，再按 pid 取运行中映像。
///
/// 取不到凭据返回 `PeerError::Credentials`；凭据有了但映像取不到（对端已退出、`/proc` 不可读），
/// 返回 `signer = Unavailable`、`exe = None`，由 [`super::judge_peer`] 拒绝。
pub(crate) fn identify_socket_peer(fd: BorrowedFd<'_>) -> Result<PeerIdentity, PeerError> {
    let cred = peer_cred(fd).map_err(PeerError::Credentials)?;
    let pid = u32::try_from(cred.pid).map_err(|_| {
        PeerError::Credentials(io::Error::other(format!(
            "SO_PEERCRED 给出的 pid 不合法：{}",
            cred.pid
        )))
    })?;
    let (signer, exe) = match running_image(Path::new(&format!("/proc/{pid}/exe"))) {
        Ok((image, exe)) => (SignerIdentity::Image(image), Some(exe)),
        Err(reason) => (SignerIdentity::Unavailable { reason }, None),
    };
    Ok(PeerIdentity {
        pid,
        user: UserId::Uid(cred.uid),
        exe,
        signer,
    })
}

fn peer_cred(fd: BorrowedFd<'_>) -> io::Result<libc::ucred> {
    let mut cred = libc::ucred {
        pid: 0,
        uid: 0,
        gid: 0,
    };
    let mut len = std::mem::size_of::<libc::ucred>() as libc::socklen_t;
    // SAFETY: fd 在 BorrowedFd 的生命周期内有效；cred 与 len 指向本栈帧里大小匹配的可写内存，
    // 内核最多写入 len 字节并回写实际长度。
    let rc = unsafe {
        libc::getsockopt(
            fd.as_raw_fd(),
            libc::SOL_SOCKET,
            libc::SO_PEERCRED,
            (&raw mut cred).cast(),
            &raw mut len,
        )
    };
    if rc != 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(cred)
}

/// 打开 `/proc/<pid>/exe`（或 `/proc/self/exe`），返回运行中映像全文的 SHA-256 与它的路径。
/// 失败时返回点名了路径与原因的说明。
fn running_image(proc_exe: &Path) -> Result<(ImageId, PathBuf), String> {
    let mut file =
        File::open(proc_exe).map_err(|e| format!("打开 {} 失败：{e}", proc_exe.display()))?;
    let hash =
        sha256_of(&mut file).map_err(|e| format!("读取 {} 失败：{e}", proc_exe.display()))?;
    // 从已打开的 fd 反查路径，而不是再 readlink 一次 proc_exe：对端在两步之间 exec 了别的程序，
    // 哈希与路径就会各说各的。
    let fd_link = format!("/proc/self/fd/{}", file.as_raw_fd());
    let target = std::fs::read_link(&fd_link)
        .map_err(|e| format!("取 {} 的路径失败：{e}", proc_exe.display()))?;
    Ok((ImageId::Sha256(hash), strip_deleted(target)))
}

fn sha256_of(file: &mut File) -> io::Result<[u8; 32]> {
    let mut hasher = Sha256::new();
    let mut buf = vec![0u8; 64 * 1024];
    loop {
        let n = file.read(&mut buf)?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
    }
    Ok(hasher.finalize().into())
}

/// 映像文件被删除或被覆盖后，内核给路径加 ` (deleted)`；去掉后缀，路径仍指向原来所在的目录。
fn strip_deleted(target: PathBuf) -> PathBuf {
    match target.to_str().and_then(|s| s.strip_suffix(DELETED_SUFFIX)) {
        Some(stripped) => PathBuf::from(stripped),
        None => target,
    }
}

#[cfg(test)]
mod tests {
    use std::os::fd::AsFd;
    use std::os::unix::net::UnixStream;

    use super::*;

    #[test]
    fn identify_socket_peer_on_own_socket_matches_own_image() {
        let (a, _b) = UnixStream::pair().unwrap();
        let peer = identify_socket_peer(a.as_fd()).unwrap();
        let (own_signer, own_exe) = own_image().unwrap();

        assert_eq!(peer.pid, std::process::id());
        assert_eq!(peer.user, current_user().unwrap());
        assert_eq!(peer.signer, own_signer);
        assert_eq!(peer.exe.as_deref(), Some(own_exe.as_path()));
    }

    #[test]
    fn own_image_hash_equals_hash_of_current_exe_file() {
        let (signer, exe) = own_image().unwrap();
        let expected: [u8; 32] = Sha256::digest(std::fs::read(&exe).unwrap()).into();
        assert_eq!(signer, SignerIdentity::Image(ImageId::Sha256(expected)));
        assert_eq!(
            exe,
            std::env::current_exe().unwrap().canonicalize().unwrap()
        );
    }

    #[test]
    fn missing_process_is_reported_not_hashed() {
        // pid 上限之外的进程一定不存在。
        let err = running_image(Path::new("/proc/2147483647/exe")).unwrap_err();
        assert!(err.contains("/proc/2147483647/exe"), "{err}");
    }

    #[test]
    fn strip_deleted_removes_only_the_kernel_suffix() {
        assert_eq!(
            strip_deleted(PathBuf::from("/opt/iwork/iwork-client (deleted)")),
            PathBuf::from("/opt/iwork/iwork-client")
        );
        assert_eq!(
            strip_deleted(PathBuf::from("/opt/iwork/iwork-client")),
            PathBuf::from("/opt/iwork/iwork-client")
        );
    }
}
