//! Windows：进程令牌 SID 与运行中映像的身份（方案 2.4 Windows 行、2.4b）。
//!
//! 对端 pid → `OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION)`：句柄在手，pid 就不会被复用 → 令牌 SID。
//! 映像身份按 2.4b ①–④：两次查名 + 按同一个文件句柄验签或算哈希；有证书、无证书两个分支都先过绑定。
//! 取到的映像身份最后都过 [`gate`]：N2 没实测前一律 `Unavailable`，Windows M1 暂不支持（方案 Q1）。

use std::ffi::OsString;
use std::fs::{File, OpenOptions};
use std::io::{self, Read, Seek, SeekFrom};
use std::os::windows::ffi::{OsStrExt, OsStringExt};
use std::os::windows::fs::OpenOptionsExt;
use std::os::windows::io::{AsRawHandle, FromRawHandle, OwnedHandle};
use std::path::{Path, PathBuf};
use std::ptr;

use sha2::{Digest, Sha256};
use windows_sys::Win32::Foundation::{FALSE, HANDLE, HLOCAL, LocalFree};
use windows_sys::Win32::Security::Authorization::ConvertSidToStringSidW;
use windows_sys::Win32::Security::WinTrust::{
    WINTRUST_ACTION_GENERIC_VERIFY_V2, WINTRUST_DATA, WINTRUST_DATA_0, WINTRUST_FILE_INFO,
    WTD_CACHE_ONLY_URL_RETRIEVAL, WTD_CHOICE_FILE, WTD_REVOKE_NONE, WTD_STATEACTION_CLOSE,
    WTD_STATEACTION_VERIFY, WTD_UI_NONE, WTHelperGetProvSignerFromChain,
    WTHelperProvDataFromStateData, WinVerifyTrust,
};
use windows_sys::Win32::Security::{GetTokenInformation, PSID, TOKEN_QUERY, TOKEN_USER, TokenUser};
use windows_sys::Win32::Storage::FileSystem::{
    FILE_NAME_NORMALIZED, FILE_SHARE_DELETE, FILE_SHARE_READ, GETFINALPATHNAMEBYHANDLE_FLAGS,
    GetFinalPathNameByHandleW, VOLUME_NAME_DOS, VOLUME_NAME_NT,
};
use windows_sys::Win32::System::Threading::{
    GetCurrentProcess, OpenProcess, OpenProcessToken, PROCESS_NAME_NATIVE,
    PROCESS_QUERY_LIMITED_INFORMATION, QueryFullProcessImageNameW,
};

use super::windows_image::{TrustCheck, classify_trust_result, gate, image_paths_agree};
use super::{ImageId, PeerError, PeerIdentity, SignerIdentity, UserId};

/// Windows 长路径上限（UTF-16 单元，含结尾 0）。路径查询都用这么大的缓冲区，省掉「不够再来」的循环。
const MAX_LONG_PATH: usize = 32_768;

/// 当前进程令牌里的用户 SID 文本（`S-1-5-21-…`）。管道名与管道 DACL 都用它。
pub(crate) fn current_user_sid() -> Result<String, PeerError> {
    token_user_sid(current_process()).map_err(PeerError::Credentials)
}

pub(crate) fn current_user() -> Result<UserId, PeerError> {
    current_user_sid().map(UserId::Sid)
}

/// 本进程映像的身份与可执行文件路径。和对端走同一套取法（[`probe_image`]），
/// 所以两边的 `exe` 是同一种写法，「同安装目录」的比较不会因为路径格式不同而误判。
pub(crate) fn own_image() -> Result<(SignerIdentity, PathBuf), PeerError> {
    let probe = probe_image(current_process())
        .map_err(|reason| PeerError::SelfIdentity(format!("本进程映像：{reason}")))?;
    Ok((gate(probe.signer), probe.exe))
}

/// 本进程运行中映像的身份。映像绑定没有被实测证明（N2），[`own_image`] 经 [`gate`] 只会给出 `Unavailable`，
/// 所以这里总是错误：Windows M1 不支持。
pub(crate) fn running_image_id() -> Result<ImageId, PeerError> {
    match own_image()?.0 {
        SignerIdentity::Image(id) => Ok(id),
        other => Err(PeerError::SelfIdentity(format!(
            "本进程运行映像身份取不到：{}",
            other.summary()
        ))),
    }
}

/// 按 pid 取对端身份。进程打不开、令牌读不到 → `Err`；映像身份取不到 → `signer = Unavailable`。
pub(crate) fn identify_process(pid: u32) -> Result<PeerIdentity, PeerError> {
    // SAFETY: OpenProcess 没有指针参数；失败返回空句柄，下面检查。
    let raw = unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, FALSE, pid) };
    if raw.is_null() {
        return Err(PeerError::Credentials(io::Error::last_os_error()));
    }
    // SAFETY: raw 是刚打开、只归我们的进程句柄；OwnedHandle 在离开作用域时关闭它。
    let process = unsafe { OwnedHandle::from_raw_handle(raw) };
    let user =
        UserId::Sid(token_user_sid(process.as_raw_handle()).map_err(PeerError::Credentials)?);
    let (signer, exe) = match probe_image(process.as_raw_handle()) {
        Ok(probe) => (gate(probe.signer), Some(probe.exe)),
        Err(reason) => (SignerIdentity::Unavailable { reason }, None),
    };
    Ok(PeerIdentity {
        pid,
        user,
        exe,
        signer,
    })
}

fn current_process() -> HANDLE {
    // SAFETY: GetCurrentProcess 没有前置条件，返回的伪句柄始终有效、不需要关闭。
    unsafe { GetCurrentProcess() }
}

fn token_user_sid(process: HANDLE) -> io::Result<String> {
    let mut raw: HANDLE = ptr::null_mut();
    // SAFETY: process 是调用方持有的有效进程句柄；raw 是可写的出参。
    if unsafe { OpenProcessToken(process, TOKEN_QUERY, &mut raw) } == 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: 成功时 raw 是只归我们的令牌句柄；OwnedHandle 负责关闭。
    let token = unsafe { OwnedHandle::from_raw_handle(raw) };

    let mut len = 0u32;
    // SAFETY: 空缓冲区 + 长度 0 是查询所需长度的标准用法；预期失败并写回 len。
    unsafe {
        GetTokenInformation(
            token.as_raw_handle(),
            TokenUser,
            ptr::null_mut(),
            0,
            &mut len,
        )
    };
    if len == 0 {
        return Err(io::Error::last_os_error());
    }
    // u64 缓冲区保证 TOKEN_USER（含指针）需要的对齐。
    let mut buf = vec![0u64; (len as usize).div_ceil(size_of::<u64>())];
    // SAFETY: buf 至少 len 字节且可写；len 是系统刚给出的长度。
    let ok = unsafe {
        GetTokenInformation(
            token.as_raw_handle(),
            TokenUser,
            buf.as_mut_ptr().cast(),
            len,
            &mut len,
        )
    };
    if ok == 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: 成功后 buf 开头是一个对齐的 TOKEN_USER，User.Sid 指向 buf 内部，buf 在本函数内一直活着。
    let user = unsafe { &*buf.as_ptr().cast::<TOKEN_USER>() };
    sid_to_string(user.User.Sid)
}

/// SID → `S-1-…` 文本。
pub(crate) fn sid_to_string(sid: PSID) -> io::Result<String> {
    let mut text: *mut u16 = ptr::null_mut();
    // SAFETY: sid 由调用方保证指向有效 SID；text 是可写的出参，成功时由 LocalAlloc 分配。
    if unsafe { ConvertSidToStringSidW(sid, &mut text) } == 0 {
        return Err(io::Error::last_os_error());
    }
    let _free = LocalMem(text.cast());
    // SAFETY: 成功时 text 是以 0 结尾的 UTF-16 串，在 _free 释放之前有效。
    let wide = unsafe { wide_cstr(text) };
    String::from_utf16(wide).map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))
}

/// 系统用 `LocalAlloc` 分配、要调用方 `LocalFree` 的内存。
pub(crate) struct LocalMem(pub(crate) HLOCAL);

impl Drop for LocalMem {
    fn drop(&mut self) {
        if !self.0.is_null() {
            // SAFETY: self.0 来自系统的 LocalAlloc 分配，只在这里释放一次。
            unsafe { LocalFree(self.0) };
        }
    }
}

/// # Safety
/// `p` 指向以 0 结尾的 UTF-16 串，且在返回的切片用完之前不被释放。
unsafe fn wide_cstr<'a>(p: *const u16) -> &'a [u16] {
    let mut len = 0;
    // SAFETY: 调用方保证串以 0 结尾，读到 0 之前都在串内。
    while unsafe { *p.add(len) } != 0 {
        len += 1;
    }
    // SAFETY: 同上，[p, p+len) 都在串内。
    unsafe { std::slice::from_raw_parts(p, len) }
}

struct ImageProbe {
    signer: SignerIdentity,
    exe: PathBuf,
}

/// 2.4b ①–④。`Err` 是 `Unavailable` 的原因。
fn probe_image(process: HANDLE) -> Result<ImageProbe, String> {
    // ① 进程映像的 NT 路径。
    let before = process_image_name(process).map_err(|e| format!("查进程映像名失败：{e}"))?;
    // ② 打开它。不给 FILE_SHARE_WRITE：持有句柄期间谁也不能以写方式打开这个文件。
    let file = OpenOptions::new()
        .read(true)
        .share_mode(FILE_SHARE_READ | FILE_SHARE_DELETE)
        .open(format!(r"\\?\GLOBALROOT{before}"))
        .map_err(|e| format!("打开映像 {before} 失败：{e}"))?;
    // ③ 再查一次进程映像名，并按句柄反查；三者一致才算句柄打开的就是运行中的映像。
    let after = process_image_name(process).map_err(|e| format!("再查进程映像名失败：{e}"))?;
    let by_handle = final_path(&file, VOLUME_NAME_NT)
        .map_err(|e| format!("按句柄取路径失败：{e}"))?
        .into_string()
        .map_err(|p| format!("按句柄取到的路径不是合法 Unicode：{p:?}"))?;
    if !image_paths_agree(&before, &after, &by_handle) {
        return Err(format!(
            "映像路径前后不一致（{before} / {after} / {by_handle}），可能在校验期间被换"
        ));
    }
    let exe = PathBuf::from(
        final_path(&file, VOLUME_NAME_DOS).map_err(|e| format!("取映像 DOS 路径失败：{e}"))?,
    );
    // ④ 验签、取证书、算哈希都只按这个句柄做。
    let signer = signer_of(&file, &exe)?;
    Ok(ImageProbe { signer, exe })
}

fn process_image_name(process: HANDLE) -> io::Result<String> {
    let mut buf = vec![0u16; MAX_LONG_PATH];
    let mut len = buf.len() as u32;
    // SAFETY: buf 有 len 个可写的 UTF-16 单元；成功时 len 改写为不含结尾 0 的长度。
    let ok = unsafe {
        QueryFullProcessImageNameW(process, PROCESS_NAME_NATIVE, buf.as_mut_ptr(), &mut len)
    };
    if ok == 0 {
        return Err(io::Error::last_os_error());
    }
    String::from_utf16(&buf[..len as usize])
        .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))
}

fn final_path(file: &File, volume: GETFINALPATHNAMEBYHANDLE_FLAGS) -> io::Result<OsString> {
    let mut buf = vec![0u16; MAX_LONG_PATH];
    // SAFETY: file 的句柄在借用期间有效；buf 有 buf.len() 个可写单元。
    let len = unsafe {
        GetFinalPathNameByHandleW(
            file.as_raw_handle(),
            buf.as_mut_ptr(),
            buf.len() as u32,
            FILE_NAME_NORMALIZED | volume,
        )
    } as usize;
    if len == 0 {
        return Err(io::Error::last_os_error());
    }
    if len >= buf.len() {
        return Err(io::Error::other(format!(
            "路径长 {len}，超过 {MAX_LONG_PATH}"
        )));
    }
    Ok(OsString::from_wide(&buf[..len]))
}

fn signer_of(file: &File, path: &Path) -> Result<SignerIdentity, String> {
    let session = TrustSession::verify(file, path);
    match classify_trust_result(session.code) {
        TrustCheck::Signed => session
            .leaf_certificate_sha256()
            .map(|leaf_sha256| SignerIdentity::Certificate { leaf_sha256 }),
        TrustCheck::Unsigned => {
            drop(session);
            hash_whole_file(file)
                .map(|h| SignerIdentity::Image(ImageId::Sha256(h)))
                .map_err(|e| format!("读映像算哈希失败：{e}"))
        }
        TrustCheck::Invalid => Err(format!(
            "WinVerifyTrust 返回 0x{:08X}，签名完整性存疑",
            session.code as u32
        )),
    }
}

/// 一次 `WinVerifyTrust(STATEACTION_VERIFY)` 及它留下的状态；drop 时 `STATEACTION_CLOSE`。
/// 证书从这次验证的状态里取，不再按路径打开第二次。
struct TrustSession {
    data: WINTRUST_DATA,
    code: i32,
    // data 里的指针指向下面两块堆内存；它们活到 CLOSE 之后。
    _file_info: Box<WINTRUST_FILE_INFO>,
    _path: Vec<u16>,
}

impl TrustSession {
    fn verify(file: &File, path: &Path) -> TrustSession {
        let path: Vec<u16> = path.as_os_str().encode_wide().chain([0]).collect();
        let mut file_info = Box::new(WINTRUST_FILE_INFO {
            cbStruct: size_of::<WINTRUST_FILE_INFO>() as u32,
            pcwszFilePath: path.as_ptr(),
            hFile: file.as_raw_handle(),
            pgKnownSubject: ptr::null_mut(),
        });
        let data = WINTRUST_DATA {
            cbStruct: size_of::<WINTRUST_DATA>() as u32,
            dwUIChoice: WTD_UI_NONE,
            fdwRevocationChecks: WTD_REVOKE_NONE,
            dwUnionChoice: WTD_CHOICE_FILE,
            Anonymous: WINTRUST_DATA_0 {
                pFile: &mut *file_info,
            },
            dwStateAction: WTD_STATEACTION_VERIFY,
            // 自签证书的链建不到受信根时，不让 CryptoAPI 联网取中间证书拖慢建连。
            dwProvFlags: WTD_CACHE_ONLY_URL_RETRIEVAL,
            ..Default::default()
        };
        let mut session = TrustSession {
            data,
            code: 0,
            _file_info: file_info,
            _path: path,
        };
        session.code = session.call();
        session
    }

    fn call(&mut self) -> i32 {
        let mut action = WINTRUST_ACTION_GENERIC_VERIFY_V2;
        // SAFETY: data 里的文件信息、路径都指向 self 持有的堆内存；hFile 在调用方借用的 File 里有效；
        // WTD_UI_NONE 不弹窗，hwnd 可以为空。
        unsafe { WinVerifyTrust(ptr::null_mut(), &mut action, (&raw mut self.data).cast()) }
    }

    /// 签名者叶证书 DER 的 SHA-256（与 macOS 的 `leaf_sha256` 同一个定义）。
    fn leaf_certificate_sha256(&self) -> Result<[u8; 32], String> {
        // SAFETY: hWVTStateData 由上面那次 STATEACTION_VERIFY 填入，CLOSE（drop）之前有效。
        let provider = unsafe { WTHelperProvDataFromStateData(self.data.hWVTStateData) };
        if provider.is_null() {
            return Err("WinVerifyTrust 没有留下验证状态".into());
        }
        // SAFETY: provider 非空且属于同一次验证；取第 0 个签名者，不取副署。
        let signer = unsafe { WTHelperGetProvSignerFromChain(provider, 0, FALSE, 0) };
        if signer.is_null() {
            return Err("验证状态里没有签名者".into());
        }
        // SAFETY: signer 非空，指向验证状态内的 CRYPT_PROVIDER_SGNR，CLOSE 之前有效。
        let signer = unsafe { &*signer };
        if signer.csCertChain == 0 || signer.pasCertChain.is_null() {
            return Err("签名者没有证书链".into());
        }
        // SAFETY: 链至少一个元素；第 0 个是签名者自己的（叶）证书。
        let leaf = unsafe { &*signer.pasCertChain };
        if leaf.pCert.is_null() {
            return Err("签名者证书为空".into());
        }
        // SAFETY: pCert 非空，指向验证状态持有的 CERT_CONTEXT。
        let cert = unsafe { &*leaf.pCert };
        // SAFETY: pbCertEncoded 指向 cbCertEncoded 字节的 DER，CLOSE 之前有效。
        let der =
            unsafe { std::slice::from_raw_parts(cert.pbCertEncoded, cert.cbCertEncoded as usize) };
        Ok(Sha256::digest(der).into())
    }
}

impl Drop for TrustSession {
    fn drop(&mut self) {
        self.data.dwStateAction = WTD_STATEACTION_CLOSE;
        // 关闭状态不会失败到需要处理的程度；返回码只说明关闭本身。
        let _ = self.call();
    }
}

/// 按同一个句柄从头读全文算 SHA-256。WinVerifyTrust 可能动过文件指针，先回到开头。
fn hash_whole_file(mut file: &File) -> io::Result<[u8; 32]> {
    file.seek(SeekFrom::Start(0))?;
    let mut hasher = Sha256::new();
    let mut buf = vec![0u8; 64 * 1024];
    loop {
        let n = file.read(&mut buf)?;
        if n == 0 {
            return Ok(hasher.finalize().into());
        }
        hasher.update(&buf[..n]);
    }
}

#[cfg(test)]
mod tests {
    use super::super::windows_image::{IMAGE_BINDING_VERIFIED, UNVERIFIED_REASON};
    use super::*;

    #[test]
    fn current_user_is_a_user_sid() {
        let sid = current_user_sid().unwrap();
        assert!(sid.starts_with("S-1-5-"), "{sid}");
    }

    /// 测试二进制没有签名：取法应走「未签名 → 按句柄读全文 SHA-256」，结果等于独立读文件算的哈希。
    #[test]
    fn unsigned_own_image_is_hashed_through_the_bound_handle() {
        let probe = probe_image(current_process()).unwrap();
        let exe = std::env::current_exe().unwrap();
        let expected: [u8; 32] = Sha256::digest(std::fs::read(&exe).unwrap()).into();
        assert_eq!(
            probe.signer,
            SignerIdentity::Image(ImageId::Sha256(expected))
        );
        assert_eq!(probe.exe, std::fs::canonicalize(&exe).unwrap());
    }

    #[test]
    fn own_process_is_current_user_and_signer_passes_the_gate() {
        let me = identify_process(std::process::id()).unwrap();
        assert_eq!(me.user, current_user().unwrap());
        assert!(me.exe.is_some(), "绑定步骤应当成功");
        if !IMAGE_BINDING_VERIFIED {
            assert_eq!(
                me.signer,
                SignerIdentity::Unavailable {
                    reason: UNVERIFIED_REASON.into()
                }
            );
        }
    }

    #[test]
    fn unopenable_pid_is_an_error_not_an_identity() {
        // pid 0 是 System Idle Process，OpenProcess 拒绝。
        assert!(identify_process(0).is_err());
    }
}
