//! Windows：命名管道（方案 2.2 Windows 行）。
//!
//! - 第一个实例 `first_pipe_instance(true)`：名字已被占就报错，T04 当「已有实例」处理；
//! - 每接受一个连接，先建好下一个实例，再交出已连接的那个，保证任何时刻都有实例在等；
//! - `reject_remote_clients(true)`；DACL 只有当前用户一条 ACE（[`pipe_sddl`]）；
//! - 对端 pid：服务端用 `GetNamedPipeClientProcessId`，客户端用 `GetNamedPipeServerProcessId`，
//!   之后两端走同一条 `peer::identify_process`。

use std::ffi::OsStr;
use std::io;
use std::os::windows::ffi::OsStrExt;
use std::os::windows::io::AsRawHandle;
use std::pin::Pin;
use std::ptr;
use std::task::{Context, Poll};

use tokio::io::{AsyncRead, AsyncWrite, ReadBuf, ReadHalf, WriteHalf};
use tokio::net::windows::named_pipe::{
    ClientOptions, NamedPipeClient, NamedPipeServer, ServerOptions,
};
use windows_sys::Win32::Foundation::{ERROR_PIPE_BUSY, FALSE};
use windows_sys::Win32::Security::Authorization::{
    ConvertStringSecurityDescriptorToSecurityDescriptorW, SDDL_REVISION_1,
};
use windows_sys::Win32::Security::SECURITY_ATTRIBUTES;
use windows_sys::Win32::System::Pipes::{
    GetNamedPipeClientProcessId, GetNamedPipeServerProcessId, WaitNamedPipeW,
};

use super::pipe_sddl::pipe_sddl;
use super::{Connection, Connector, Endpoint, Listener};
use crate::peer::windows::LocalMem;
use crate::peer::{self, PeerError, PeerIdentity};

/// 管道忙（所有实例都被占）时，`WaitNamedPipeW` 每次最多等多久。
const PIPE_BUSY_WAIT_MS: u32 = 2_000;
/// 管道忙时最多重试几轮；还忙就把 `ERROR_PIPE_BUSY` 交给调用方，由 T09 的退避重连处理。
const PIPE_BUSY_RETRIES: usize = 5;

pub struct LocalListener {
    name: String,
    user_sid: String,
    /// 已建好、正在等客户端的实例。
    next: NamedPipeServer,
    endpoint: Endpoint,
}

impl LocalListener {
    /// 建第一个管道实例。名字已被占（另一个后台在跑，或别人抢先建了同名管道）返回错误，不接管。
    pub fn bind(name: &str) -> io::Result<LocalListener> {
        let user_sid = peer::current_user_sid().map_err(io::Error::other)?;
        let next = create_instance(name, &user_sid, true)?;
        Ok(LocalListener {
            name: name.to_owned(),
            user_sid,
            next,
            endpoint: Endpoint::Pipe(name.to_owned()),
        })
    }
}

impl Listener for LocalListener {
    type Conn = LocalConn;

    async fn accept(&mut self) -> io::Result<LocalConn> {
        self.next.connect().await?;
        let fresh = create_instance(&self.name, &self.user_sid, false)?;
        let connected = std::mem::replace(&mut self.next, fresh);
        Ok(LocalConn {
            pipe: Pipe::Server(connected),
        })
    }

    fn endpoint(&self) -> &Endpoint {
        &self.endpoint
    }
}

fn create_instance(name: &str, user_sid: &str, first: bool) -> io::Result<NamedPipeServer> {
    let descriptor = security_descriptor(&pipe_sddl(user_sid))?;
    let mut attrs = SECURITY_ATTRIBUTES {
        nLength: size_of::<SECURITY_ATTRIBUTES>() as u32,
        lpSecurityDescriptor: descriptor.0,
        bInheritHandle: FALSE,
    };
    // SAFETY: attrs 指向有效的 SECURITY_ATTRIBUTES，其中的安全描述符由 descriptor 持有，活过这次调用。
    unsafe {
        ServerOptions::new()
            .first_pipe_instance(first)
            .reject_remote_clients(true)
            .create_with_security_attributes_raw(name, (&raw mut attrs).cast())
    }
}

/// SDDL → 自相关安全描述符（`LocalAlloc` 分配，由返回的守卫释放）。
fn security_descriptor(sddl: &str) -> io::Result<LocalMem> {
    let wide = to_wide(sddl);
    let mut descriptor = ptr::null_mut();
    // SAFETY: wide 以 0 结尾；descriptor 是可写的出参；长度出参可以为空。
    let ok = unsafe {
        ConvertStringSecurityDescriptorToSecurityDescriptorW(
            wide.as_ptr(),
            SDDL_REVISION_1,
            &mut descriptor,
            ptr::null_mut(),
        )
    };
    if ok == 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(LocalMem(descriptor))
}

fn to_wide(s: impl AsRef<OsStr>) -> Vec<u16> {
    s.as_ref().encode_wide().chain([0]).collect()
}

#[derive(Debug, Default, Clone, Copy)]
pub struct LocalConnector;

impl Connector for LocalConnector {
    type Conn = LocalConn;

    async fn connect(&self, endpoint: &Endpoint) -> io::Result<LocalConn> {
        let Endpoint::Pipe(name) = endpoint else {
            return Err(io::Error::new(
                io::ErrorKind::Unsupported,
                format!("本平台只支持命名管道端点：{endpoint:?}"),
            ));
        };
        let mut retries = 0;
        loop {
            // 默认 SECURITY_IDENTIFICATION：服务端只能识别、不能冒充客户端。
            match ClientOptions::new().open(name) {
                Ok(client) => {
                    return Ok(LocalConn {
                        pipe: Pipe::Client(client),
                    });
                }
                Err(e)
                    if e.raw_os_error() == Some(ERROR_PIPE_BUSY as i32)
                        && retries < PIPE_BUSY_RETRIES =>
                {
                    retries += 1;
                    wait_for_free_instance(name).await?;
                }
                Err(e) => return Err(e),
            }
        }
    }
}

/// `WaitNamedPipeW` 会阻塞线程，放到阻塞线程池。超时返回错误。
async fn wait_for_free_instance(name: &str) -> io::Result<()> {
    let wide = to_wide(name);
    tokio::task::spawn_blocking(move || {
        // SAFETY: wide 以 0 结尾，在调用期间有效。
        if unsafe { WaitNamedPipeW(wide.as_ptr(), PIPE_BUSY_WAIT_MS) } == 0 {
            Err(io::Error::last_os_error())
        } else {
            Ok(())
        }
    })
    .await
    .map_err(io::Error::other)?
}

pub struct LocalConn {
    pipe: Pipe,
}

impl Connection for LocalConn {
    type Read = ReadHalf<Pipe>;
    type Write = WriteHalf<Pipe>;

    async fn peer(&self) -> Result<PeerIdentity, PeerError> {
        let pid = self.pipe.peer_pid().map_err(PeerError::Credentials)?;
        // 验签、算映像哈希要读文件，放到阻塞线程池。
        tokio::task::spawn_blocking(move || peer::identify_process(pid))
            .await
            .map_err(|e| PeerError::Credentials(io::Error::other(e)))?
    }

    fn into_split(self) -> (Self::Read, Self::Write) {
        tokio::io::split(self.pipe)
    }
}

/// 管道的一端。服务端、客户端在 tokio 里是两个类型，这里合成一个，读写原样转发。
pub enum Pipe {
    Server(NamedPipeServer),
    Client(NamedPipeClient),
}

impl Pipe {
    /// 对端进程的 pid：服务端问客户端是谁，客户端问服务端是谁。
    fn peer_pid(&self) -> io::Result<u32> {
        let mut pid = 0u32;
        // SAFETY: 句柄由 self 持有、在调用期间有效；pid 是可写的出参。
        let ok = unsafe {
            match self {
                Pipe::Server(s) => GetNamedPipeClientProcessId(s.as_raw_handle(), &mut pid),
                Pipe::Client(c) => GetNamedPipeServerProcessId(c.as_raw_handle(), &mut pid),
            }
        };
        if ok == 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(pid)
    }
}

impl AsyncRead for Pipe {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        match self.get_mut() {
            Pipe::Server(s) => Pin::new(s).poll_read(cx, buf),
            Pipe::Client(c) => Pin::new(c).poll_read(cx, buf),
        }
    }
}

impl AsyncWrite for Pipe {
    fn poll_write(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        match self.get_mut() {
            Pipe::Server(s) => Pin::new(s).poll_write(cx, buf),
            Pipe::Client(c) => Pin::new(c).poll_write(cx, buf),
        }
    }

    fn poll_flush(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        match self.get_mut() {
            Pipe::Server(s) => Pin::new(s).poll_flush(cx),
            Pipe::Client(c) => Pin::new(c).poll_flush(cx),
        }
    }

    fn poll_shutdown(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        match self.get_mut() {
            Pipe::Server(s) => Pin::new(s).poll_shutdown(cx),
            Pipe::Client(c) => Pin::new(c).poll_shutdown(cx),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::peer::windows::sid_to_string;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use windows_sys::Win32::Foundation::{ERROR_SUCCESS, GENERIC_ALL};
    use windows_sys::Win32::Security::Authorization::{GetSecurityInfo, SE_KERNEL_OBJECT};
    use windows_sys::Win32::Security::{
        ACCESS_ALLOWED_ACE, ACL, ACL_SIZE_INFORMATION, AclSizeInformation,
        DACL_SECURITY_INFORMATION, GetAce, GetAclInformation,
    };
    use windows_sys::Win32::Storage::FileSystem::FILE_ALL_ACCESS;
    use windows_sys::Win32::System::SystemServices::ACCESS_ALLOWED_ACE_TYPE;

    fn unique_name(tag: &str) -> String {
        format!(r"\\.\pipe\iwork-test-{tag}-{}", ulid::Ulid::generate())
    }

    #[tokio::test]
    async fn pipe_dacl_only_current_user() {
        let listener = LocalListener::bind(&unique_name("dacl")).unwrap();
        let handle = listener.next.as_raw_handle();

        let mut dacl: *mut ACL = ptr::null_mut();
        let mut descriptor = ptr::null_mut();
        // SAFETY: handle 是 listener 持有的管道句柄；出参都可写；descriptor 由下面的守卫释放。
        let rc = unsafe {
            GetSecurityInfo(
                handle,
                SE_KERNEL_OBJECT,
                DACL_SECURITY_INFORMATION,
                ptr::null_mut(),
                ptr::null_mut(),
                &mut dacl,
                ptr::null_mut(),
                &mut descriptor,
            )
        };
        assert_eq!(rc, ERROR_SUCCESS);
        let _free = LocalMem(descriptor);
        assert!(!dacl.is_null(), "DACL 为空等于谁都能连");

        let mut size = ACL_SIZE_INFORMATION::default();
        // SAFETY: dacl 指向 descriptor 内的有效 ACL；size 大小与类别匹配。
        let ok = unsafe {
            GetAclInformation(
                dacl,
                (&raw mut size).cast(),
                size_of::<ACL_SIZE_INFORMATION>() as u32,
                AclSizeInformation,
            )
        };
        assert_ne!(ok, 0);
        assert_eq!(size.AceCount, 1, "只能有当前用户一条 ACE");

        let mut ace: *mut core::ffi::c_void = ptr::null_mut();
        // SAFETY: 下标 0 < AceCount。
        assert_ne!(unsafe { GetAce(dacl, 0, &mut ace) }, 0);
        // SAFETY: GetAce 成功，ace 指向 ACL 内的一条 ACE；类型下面先核对。
        let ace = unsafe { &*ace.cast::<ACCESS_ALLOWED_ACE>() };
        assert_eq!(u32::from(ace.Header.AceType), ACCESS_ALLOWED_ACE_TYPE);
        // 建对象时系统按管道的通用映射把 GENERIC_ALL 换成 FILE_ALL_ACCESS，两种写法都是「全部权限」。
        assert!(
            ace.Mask == GENERIC_ALL || ace.Mask == FILE_ALL_ACCESS,
            "Mask = {:#x}",
            ace.Mask
        );
        let sid = sid_to_string((&raw const ace.SidStart).cast_mut().cast()).unwrap();
        assert_eq!(sid, peer::current_user_sid().unwrap());
    }

    #[tokio::test]
    async fn second_listener_on_same_name_is_refused() {
        let name = unique_name("dup");
        let _first = LocalListener::bind(&name).unwrap();
        assert!(
            LocalListener::bind(&name).is_err(),
            "名字被占必须报错，不接管"
        );
        // 对照：换个名字可以绑。
        LocalListener::bind(&unique_name("dup-other")).unwrap();
    }

    #[tokio::test]
    async fn both_ends_see_this_process_and_bytes_flow() {
        let name = unique_name("rt");
        let mut listener = LocalListener::bind(&name).unwrap();
        let endpoint = listener.endpoint().clone();
        let (server, client) = tokio::join!(listener.accept(), LocalConnector.connect(&endpoint));
        let (server, client) = (server.unwrap(), client.unwrap());

        let me = std::process::id();
        let seen_by_server = server.peer().await.unwrap();
        let seen_by_client = client.peer().await.unwrap();
        assert_eq!(seen_by_server.pid, me);
        assert_eq!(seen_by_client.pid, me);
        assert_eq!(seen_by_server.user, peer::windows::current_user().unwrap());

        let (_sr, mut sw) = server.into_split();
        let (mut cr, _cw) = client.into_split();
        sw.write_all(b"ping").await.unwrap();
        let mut got = [0u8; 4];
        cr.read_exact(&mut got).await.unwrap();
        assert_eq!(&got, b"ping");

        // accept 之后下一个实例已经在等：第二个客户端也能连上。
        let (again, client2) = tokio::join!(listener.accept(), LocalConnector.connect(&endpoint));
        again.unwrap();
        client2.unwrap();
    }

    #[tokio::test]
    async fn non_pipe_endpoint_is_unsupported() {
        let err = LocalConnector
            .connect(&Endpoint::Memory("x".into()))
            .await
            .err()
            .unwrap();
        assert_eq!(err.kind(), io::ErrorKind::Unsupported);
    }
}
