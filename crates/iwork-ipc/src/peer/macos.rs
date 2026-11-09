//! macOS 对端身份（方案 2.4、2.4b；实测见 `t0-findings.md`「T03 本机通信实测 · macOS」）。
//!
//! 怎么保证核验的是**运行中的映像**：
//! 1. 对端用 socket 上的审计令牌（`LOCAL_PEERTOKEN`）换成内核跟踪的动态 `SecCode`，不经 pid，不受 pid 复用影响；
//! 2. `SecCodeCopySigningInformation` 读出证书 / cdhash。头文件说明这些信息有一部分**来自磁盘**，可能已被换掉；
//! 3. 所以把读到的身份写成代码要求（`certificate leaf = H"…"` 或 `cdhash H"…"`），先对映像文件做全量静态校验
//!    （每一页哈希都对，见 [`check_image_file`]），再让 `SecCodeCheckValidity` 对同一个动态对象验一遍。后者按头文件
//!    说明「不受文件系统来源被改动的影响」：只有运行中的代码确实是这个身份才通过；磁盘上的代码目录与运行中的不一致
//!    时返回 `errSecCSStaticCodeChanged`（-67034）。
//!
//! 第 2、3 步之间磁盘被换，结果只会是 `Unavailable`，不会是一个错的身份（实测 N1 反例 a/b/c）。自己（[`own_image`]）
//! 走同一条路，只是动态对象来自 `SecCodeCopySelf`。

use std::ffi::{CStr, c_void};
use std::io;
use std::os::fd::{AsRawFd, BorrowedFd};
use std::os::unix::ffi::OsStrExt;
use std::path::PathBuf;
use std::ptr;

use core_foundation_sys::array::{
    CFArrayGetCount, CFArrayGetTypeID, CFArrayGetValueAtIndex, CFArrayRef,
};
use core_foundation_sys::base::{
    CFGetTypeID, CFIndex, CFRelease, CFTypeID, CFTypeRef, OSStatus, kCFAllocatorDefault,
};
use core_foundation_sys::data::{
    CFDataCreate, CFDataGetBytePtr, CFDataGetLength, CFDataGetTypeID, CFDataRef,
};
use core_foundation_sys::dictionary::{
    CFDictionaryCreate, CFDictionaryGetValue, CFDictionaryRef, kCFTypeDictionaryKeyCallBacks,
    kCFTypeDictionaryValueCallBacks,
};
use core_foundation_sys::string::{CFStringCreateWithBytes, CFStringRef, kCFStringEncodingUTF8};
use core_foundation_sys::url::{CFURLGetFileSystemRepresentation, CFURLGetTypeID, CFURLRef};
use sha2::{Digest, Sha256};

use super::{ImageId, PeerError, PeerIdentity, SignerIdentity, UserId};

type SecCSFlags = u32;
/// `SecCodeRef` / `SecRequirementRef` / `SecCertificateRef` 都是 CF 对象，这里不区分具体类型。
type SecRef = CFTypeRef;

const K_SEC_CS_DEFAULT_FLAGS: SecCSFlags = 0;
const K_SEC_CS_SIGNING_INFORMATION: SecCSFlags = 1 << 1;
/// CommonCrypto 的 `CC_SHA1_DIGEST_LENGTH`。
const SHA1_LEN: usize = 20;

/// `audit_token_t`（`<mach/message.h>`）：8 个 `unsigned int`。
type AuditToken = [u32; 8];

#[link(name = "Security", kind = "framework")]
unsafe extern "C" {
    static kSecGuestAttributeAudit: CFStringRef;
    static kSecCodeInfoCertificates: CFStringRef;
    static kSecCodeInfoUnique: CFStringRef;
    static kSecCodeInfoMainExecutable: CFStringRef;

    fn SecCodeCopySelf(flags: SecCSFlags, code: *mut SecRef) -> OSStatus;
    fn SecCodeCopyGuestWithAttributes(
        host: SecRef,
        attributes: CFDictionaryRef,
        flags: SecCSFlags,
        guest: *mut SecRef,
    ) -> OSStatus;
    fn SecCodeCheckValidity(code: SecRef, flags: SecCSFlags, requirement: SecRef) -> OSStatus;
    fn SecCodeCopyStaticCode(code: SecRef, flags: SecCSFlags, static_code: *mut SecRef)
    -> OSStatus;
    fn SecStaticCodeCheckValidity(
        static_code: SecRef,
        flags: SecCSFlags,
        requirement: SecRef,
    ) -> OSStatus;
    fn SecCodeCopySigningInformation(
        code: SecRef,
        flags: SecCSFlags,
        information: *mut CFDictionaryRef,
    ) -> OSStatus;
    fn SecRequirementCreateWithString(
        text: CFStringRef,
        flags: SecCSFlags,
        requirement: *mut SecRef,
    ) -> OSStatus;
    fn SecCertificateCopyData(certificate: SecRef) -> CFDataRef;
}

// libSystem 自带 CommonCrypto，不用另外链接。
unsafe extern "C" {
    fn CC_SHA1(data: *const c_void, len: u32, md: *mut u8) -> *mut u8;
}

pub(crate) fn current_user() -> Result<UserId, PeerError> {
    // SAFETY: geteuid 没有前置条件，总是成功。
    Ok(UserId::Uid(unsafe { libc::geteuid() }))
}

/// 本进程运行中映像的身份与可执行文件路径。路径取不到算错误：`SelfIdentity` 要用它定安装目录。
pub(crate) fn own_image() -> Result<(SignerIdentity, PathBuf), PeerError> {
    let code = copy_self().map_err(PeerError::SelfIdentity)?;
    let image = identify_code(&code);
    let exe = image.exe.ok_or_else(|| {
        PeerError::SelfIdentity(format!(
            "取不到本进程可执行文件路径（签名身份 {}）",
            image.signer.summary()
        ))
    })?;
    Ok((image.signer, exe))
}

/// 已连接 UDS 的对端身份。uid、pid 取不到是系统错误；映像身份取不到装成 `Unavailable`，由 `judge_peer` 拒。
pub(crate) fn identify_socket_peer(fd: BorrowedFd<'_>) -> Result<PeerIdentity, PeerError> {
    let uid = peer_uid(fd).map_err(PeerError::Credentials)?;
    let pid = local_sockopt::<libc::pid_t>(fd, libc::LOCAL_PEERPID, "LOCAL_PEERPID")
        .map_err(PeerError::Credentials)?;
    let pid = u32::try_from(pid).map_err(|_| {
        PeerError::Credentials(io::Error::other(format!(
            "LOCAL_PEERPID 给出负数 pid：{pid}"
        )))
    })?;
    let image = local_sockopt::<AuditToken>(fd, libc::LOCAL_PEERTOKEN, "LOCAL_PEERTOKEN")
        .map_err(|e| e.to_string())
        .and_then(|token| copy_guest(&token))
        .map_or_else(RunningImage::unavailable, |code| identify_code(&code));
    Ok(PeerIdentity {
        pid,
        user: UserId::Uid(uid),
        exe: image.exe,
        signer: image.signer,
    })
}

/// 一个动态 `SecCode` 能说出的身份。`exe` 只给审计与「同安装目录」检查用。
struct RunningImage {
    signer: SignerIdentity,
    exe: Option<PathBuf>,
}

impl RunningImage {
    fn unavailable(reason: String) -> RunningImage {
        RunningImage {
            signer: SignerIdentity::Unavailable { reason },
            exe: None,
        }
    }
}

/// 签名信息里声称的身份，还没经运行中的代码确认。
enum Claim {
    Certificate { leaf_der: Vec<u8> },
    CdHash(Vec<u8>),
}

impl Claim {
    /// 把声称的身份钉成代码要求。叶证书只能用 SHA-1 写（requirement 语言不收 SHA-256，实测 `invalid hash length`）；
    /// 对外报的仍是同一份 DER 的 SHA-256。要伪造，攻击者得造出与我们叶证书 SHA-1 相同的证书（第二原像），不可行。
    fn requirement_text(&self) -> String {
        match self {
            Claim::Certificate { leaf_der } => {
                format!("certificate leaf = H\"{}\"", hex::encode(sha1(leaf_der)))
            }
            Claim::CdHash(cdhash) => format!("cdhash H\"{}\"", hex::encode(cdhash)),
        }
    }

    fn into_signer(self) -> SignerIdentity {
        match self {
            Claim::Certificate { leaf_der } => SignerIdentity::Certificate {
                leaf_sha256: Sha256::digest(&leaf_der).into(),
            },
            Claim::CdHash(cdhash) => SignerIdentity::Image(ImageId::CdHash(cdhash)),
        }
    }
}

/// 读签名信息 → 钉成要求 → 对运行中的代码验。任一步失败都是 `Unavailable`，原因写清哪一步。
fn identify_code(code: &Cf) -> RunningImage {
    let info = match copy_signing_information(code) {
        Ok(info) => info,
        Err(reason) => return RunningImage::unavailable(reason),
    };
    let exe = main_executable(&info);
    let signer = match claimed_identity(&info).and_then(|claim| confirm_running(code, claim)) {
        Ok(signer) => signer,
        Err(reason) => SignerIdentity::Unavailable { reason },
    };
    RunningImage { signer, exe }
}

fn claimed_identity(info: &Cf) -> Result<Claim, String> {
    // SAFETY: 读 Security.framework 导出的常量 CFString，链接期已解析，进程内只读。
    let (certs_key, unique_key) = unsafe { (kSecCodeInfoCertificates, kSecCodeInfoUnique) };
    if let Some(certs) = dict_value(info, certs_key, cf_array_type(), "kSecCodeInfoCertificates")? {
        // SAFETY: dict_value 已确认它是 CFArray；数组归 info 所有，info 在本函数内一直活着。
        let count = unsafe { CFArrayGetCount(certs as CFArrayRef) };
        if count < 1 {
            return Err("kSecCodeInfoCertificates 是空数组".into());
        }
        // SAFETY: 下标 0 < count；签名信息里的证书数组元素是 SecCertificate（头文件约定），引用归数组所有。
        let leaf = unsafe { CFArrayGetValueAtIndex(certs as CFArrayRef, 0) };
        // SAFETY: leaf 是有效的 SecCertificateRef；返回值遵循 Copy 规则（+1），交给 Cf 释放。
        let der = unsafe { Cf::adopt(SecCertificateCopyData(leaf).cast()) }
            .ok_or("SecCertificateCopyData 返回空")?;
        return Ok(Claim::Certificate {
            leaf_der: data_bytes(&der),
        });
    }
    match dict_value(info, unique_key, cf_data_type(), "kSecCodeInfoUnique")? {
        Some(unique) => {
            // 借用的 CFData：info 活着它就活着；data_bytes 立刻复制出来。
            let borrowed = Borrowed(unique);
            Ok(Claim::CdHash(data_bytes(&borrowed)))
        }
        None => Err("未签名：签名信息里既没有证书也没有 cdhash".into()),
    }
}

fn confirm_running(code: &Cf, claim: Claim) -> Result<SignerIdentity, String> {
    let text = cf_string(&claim.requirement_text())?;
    let mut requirement: SecRef = ptr::null();
    // SAFETY: text 是有效 CFString；出参成功时是 +1 引用，交给 Cf 释放。
    let status = unsafe {
        SecRequirementCreateWithString(
            text.as_ptr().cast(),
            K_SEC_CS_DEFAULT_FLAGS,
            &mut requirement,
        )
    };
    check(status, "SecRequirementCreateWithString")?;
    // SAFETY: 上面成功，requirement 非空且 +1。
    let requirement =
        unsafe { Cf::adopt(requirement) }.ok_or("SecRequirementCreateWithString 返回空")?;
    check_image_file(code, &requirement)?;
    // SAFETY: code、requirement 都是有效的 CF 对象，调用期间由守卫持有。
    let status = unsafe {
        SecCodeCheckValidity(code.as_ptr(), K_SEC_CS_DEFAULT_FLAGS, requirement.as_ptr())
    };
    check(status, "SecCodeCheckValidity")?;
    Ok(claim.into_signer())
}

/// 对映像文件做全量静态校验（每一页的哈希）。
///
/// 动态校验只看签名本身和内核的有效位；内核是在**页被调入时**才逐页核对的。实测（N1 反例 b）：对端运行中
/// 原地改掉一个还没调入的页，动态校验照样通过，等那一页被执行才出事。这里把这种文件当作已被篡改，直接拒。
/// 文件按内核记录的映像定位（不是按路径重新找），之后的动态校验再确认它与运行中的代码目录一致。
fn check_image_file(code: &Cf, requirement: &Cf) -> Result<(), String> {
    let mut image_file: SecRef = ptr::null();
    // SAFETY: code 是有效 SecCode；出参成功时是 +1 引用，交给 Cf。
    let status =
        unsafe { SecCodeCopyStaticCode(code.as_ptr(), K_SEC_CS_DEFAULT_FLAGS, &mut image_file) };
    check(status, "SecCodeCopyStaticCode")?;
    // SAFETY: 上面成功，image_file 非空且 +1。
    let image_file = unsafe { Cf::adopt(image_file) }.ok_or("SecCodeCopyStaticCode 返回空")?;
    // SAFETY: image_file、requirement 都是有效的 CF 对象，调用期间由守卫持有。
    let status = unsafe {
        SecStaticCodeCheckValidity(
            image_file.as_ptr(),
            K_SEC_CS_DEFAULT_FLAGS,
            requirement.as_ptr(),
        )
    };
    check(status, "SecStaticCodeCheckValidity")
}

fn copy_self() -> Result<Cf, String> {
    let mut code: SecRef = ptr::null();
    // SAFETY: 出参成功时是 +1 引用，交给 Cf 释放。
    let status = unsafe { SecCodeCopySelf(K_SEC_CS_DEFAULT_FLAGS, &mut code) };
    check(status, "SecCodeCopySelf")?;
    // SAFETY: 上面成功，code 非空且 +1。
    unsafe { Cf::adopt(code) }.ok_or_else(|| "SecCodeCopySelf 返回空".into())
}

/// 审计令牌 → 内核跟踪的动态 `SecCode`（host 传 NULL = 以内核为根）。
fn copy_guest(token: &AuditToken) -> Result<Cf, String> {
    // SAFETY: token 是 32 字节的有效内存，CFDataCreate 复制它；返回 +1 引用交给 Cf。
    let data = unsafe {
        Cf::adopt(
            CFDataCreate(
                kCFAllocatorDefault,
                token.as_ptr().cast(),
                size_of::<AuditToken>() as CFIndex,
            )
            .cast(),
        )
    }
    .ok_or("CFDataCreate（审计令牌）返回空")?;
    // SAFETY: 读 Security.framework 导出的常量 CFString。
    let keys = [unsafe { kSecGuestAttributeAudit }.cast::<c_void>()];
    let values = [data.as_ptr()];
    // SAFETY: keys、values 各 1 个有效 CF 对象，回调用 CF 标准的 retain/release；返回 +1 引用交给 Cf。
    let attributes = unsafe {
        Cf::adopt(
            CFDictionaryCreate(
                kCFAllocatorDefault,
                keys.as_ptr(),
                values.as_ptr(),
                1,
                &kCFTypeDictionaryKeyCallBacks,
                &kCFTypeDictionaryValueCallBacks,
            )
            .cast(),
        )
    }
    .ok_or("CFDictionaryCreate（guest 属性）返回空")?;
    let mut guest: SecRef = ptr::null();
    // SAFETY: attributes 是有效 CFDictionary；出参成功时是 +1 引用，交给 Cf。
    let status = unsafe {
        SecCodeCopyGuestWithAttributes(
            ptr::null(),
            attributes.as_ptr().cast(),
            K_SEC_CS_DEFAULT_FLAGS,
            &mut guest,
        )
    };
    check(status, "SecCodeCopyGuestWithAttributes")?;
    // SAFETY: 上面成功，guest 非空且 +1。
    unsafe { Cf::adopt(guest) }.ok_or_else(|| "SecCodeCopyGuestWithAttributes 返回空".into())
}

fn copy_signing_information(code: &Cf) -> Result<Cf, String> {
    let mut info: CFDictionaryRef = ptr::null();
    // SAFETY: code 是有效 SecCode；出参成功时是 +1 引用，交给 Cf。
    let status = unsafe {
        SecCodeCopySigningInformation(code.as_ptr(), K_SEC_CS_SIGNING_INFORMATION, &mut info)
    };
    check(status, "SecCodeCopySigningInformation")?;
    // SAFETY: 上面成功，info 非空且 +1。
    unsafe { Cf::adopt(info.cast()) }.ok_or_else(|| "SecCodeCopySigningInformation 返回空".into())
}

/// 主可执行文件的规范化路径；规范化失败（文件已被挪走）用原路径，取不到就是 None。
fn main_executable(info: &Cf) -> Option<PathBuf> {
    // SAFETY: 读 Security.framework 导出的常量 CFString。
    let key = unsafe { kSecCodeInfoMainExecutable };
    let url = dict_value(info, key, cf_url_type(), "kSecCodeInfoMainExecutable").ok()??;
    let mut buf = [0u8; libc::PATH_MAX as usize];
    // SAFETY: url 已确认是 CFURL，归 info 所有；buf 长度如实传入，成功时以 NUL 结尾。
    let ok = unsafe {
        CFURLGetFileSystemRepresentation(url as CFURLRef, 1, buf.as_mut_ptr(), buf.len() as CFIndex)
    };
    if ok == 0 {
        return None;
    }
    let path = PathBuf::from(std::ffi::OsStr::from_bytes(
        CStr::from_bytes_until_nul(&buf).ok()?.to_bytes(),
    ));
    Some(path.canonicalize().unwrap_or(path))
}

fn peer_uid(fd: BorrowedFd<'_>) -> io::Result<u32> {
    let mut uid: libc::uid_t = 0;
    let mut gid: libc::gid_t = 0;
    // SAFETY: fd 在借用期内有效；两个出参指向本地变量。
    if unsafe { libc::getpeereid(fd.as_raw_fd(), &mut uid, &mut gid) } != 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(uid)
}

/// `getsockopt(SOL_LOCAL, name)`，要求内核写回的长度正好是 `T` 的大小。
fn local_sockopt<T: Default>(fd: BorrowedFd<'_>, name: libc::c_int, what: &str) -> io::Result<T> {
    let mut value = T::default();
    let mut len = size_of::<T>() as libc::socklen_t;
    // SAFETY: fd 在借用期内有效；value 是 len 字节的可写内存，T 是纯整数（数组）类型，任何位模式都合法。
    let rc = unsafe {
        libc::getsockopt(
            fd.as_raw_fd(),
            libc::SOL_LOCAL,
            name,
            (&mut value as *mut T).cast(),
            &mut len,
        )
    };
    if rc != 0 {
        let e = io::Error::last_os_error();
        return Err(io::Error::new(e.kind(), format!("getsockopt({what})：{e}")));
    }
    if len as usize != size_of::<T>() {
        return Err(io::Error::other(format!(
            "getsockopt({what}) 写回 {len} 字节，预期 {}",
            size_of::<T>()
        )));
    }
    Ok(value)
}

fn check(status: OSStatus, step: &str) -> Result<(), String> {
    if status == 0 {
        Ok(())
    } else {
        Err(format!("{step} 失败（OSStatus {status}）"))
    }
}

/// 从字典里取一个值并核对类型。缺键是 `Ok(None)`；类型不对是错误（不按错的类型去读）。
/// 返回的引用归字典所有，不另 retain。
fn dict_value(
    dict: &Cf,
    key: CFStringRef,
    expected: CFTypeID,
    name: &str,
) -> Result<Option<CFTypeRef>, String> {
    // SAFETY: dict 是有效 CFDictionary，key 是有效 CFString；返回值不转移所有权。
    let value = unsafe { CFDictionaryGetValue(dict.as_ptr().cast(), key.cast()) };
    if value.is_null() {
        return Ok(None);
    }
    // SAFETY: value 非空，是字典里的有效 CF 对象。
    if unsafe { CFGetTypeID(value) } != expected {
        return Err(format!("签名信息里 {name} 的类型不对"));
    }
    Ok(Some(value))
}

fn cf_array_type() -> CFTypeID {
    // SAFETY: 无前置条件。
    unsafe { CFArrayGetTypeID() }
}

fn cf_data_type() -> CFTypeID {
    // SAFETY: 无前置条件。
    unsafe { CFDataGetTypeID() }
}

fn cf_url_type() -> CFTypeID {
    // SAFETY: 无前置条件。
    unsafe { CFURLGetTypeID() }
}

fn cf_string(text: &str) -> Result<Cf, String> {
    // SAFETY: 指针与长度来自同一个 &str（UTF-8）；返回 +1 引用交给 Cf。
    unsafe {
        Cf::adopt(
            CFStringCreateWithBytes(
                kCFAllocatorDefault,
                text.as_ptr(),
                text.len() as CFIndex,
                kCFStringEncodingUTF8,
                0,
            )
            .cast(),
        )
    }
    .ok_or_else(|| format!("CFStringCreateWithBytes 返回空：{text}"))
}

/// 复制一个 CFData 的字节。
fn data_bytes(data: &impl AsCf) -> Vec<u8> {
    let data = data.as_ptr() as CFDataRef;
    // SAFETY: data 是有效 CFData（调用方已核对类型或来自返回 CFData 的 API）；长度与指针来自同一对象。
    unsafe {
        let len = CFDataGetLength(data) as usize;
        let ptr = CFDataGetBytePtr(data);
        if len == 0 || ptr.is_null() {
            return Vec::new();
        }
        std::slice::from_raw_parts(ptr, len).to_vec()
    }
}

fn sha1(bytes: &[u8]) -> [u8; SHA1_LEN] {
    let mut out = [0u8; SHA1_LEN];
    let len = u32::try_from(bytes.len()).expect("证书 DER 不会超过 4 GiB");
    // SAFETY: 输入指针与长度来自同一切片；输出缓冲 20 字节，正是 SHA-1 摘要长度。
    unsafe { CC_SHA1(bytes.as_ptr().cast(), len, out.as_mut_ptr()) };
    out
}

trait AsCf {
    fn as_ptr(&self) -> CFTypeRef;
}

/// 持有一个按 Create / Copy 规则得来的 CF 对象（+1 引用），离开作用域时 `CFRelease`。
struct Cf(CFTypeRef);

impl Cf {
    /// # Safety
    /// `ptr` 必须是空，或调用方拥有的一份 +1 引用（不能再由别人释放）。
    unsafe fn adopt(ptr: CFTypeRef) -> Option<Cf> {
        (!ptr.is_null()).then_some(Cf(ptr))
    }
}

impl AsCf for Cf {
    fn as_ptr(&self) -> CFTypeRef {
        self.0
    }
}

impl Drop for Cf {
    fn drop(&mut self) {
        // SAFETY: adopt 保证 self.0 非空且是我们拥有的一份引用，只在这里释放一次。
        unsafe { CFRelease(self.0) }
    }
}

/// 借来的 CF 对象（不拥有，不释放），只在所有者活着的范围内用。
struct Borrowed(CFTypeRef);

impl AsCf for Borrowed {
    fn as_ptr(&self) -> CFTypeRef {
        self.0
    }
}

#[cfg(test)]
mod tests {
    use std::os::fd::AsFd;
    use std::os::unix::net::UnixStream;

    use super::*;

    #[test]
    fn own_image_is_identified() {
        let (signer, exe) = own_image().expect("own_image");
        assert!(
            !matches!(signer, SignerIdentity::Unavailable { .. }),
            "本进程身份不该是 Unavailable：{signer:?}"
        );
        assert_eq!(
            Some(exe.clone()),
            std::env::current_exe()
                .ok()
                .and_then(|p| p.canonicalize().ok()),
            "可执行文件路径应是本测试二进制"
        );
    }

    /// 链接器给 cargo test 的二进制做 ad-hoc 签名：没有证书，身份是 20 字节的 cdhash。
    #[test]
    fn own_image_of_linker_signed_test_binary_is_cdhash() {
        let (signer, _) = own_image().expect("own_image");
        match signer {
            SignerIdentity::Image(ImageId::CdHash(h)) => assert_eq!(h.len(), 20),
            other => panic!("预期 CdHash，实际 {other:?}"),
        }
    }

    #[test]
    fn socket_peer_of_self_matches_own_image() {
        let (a, _b) = UnixStream::pair().expect("socketpair");
        let peer = identify_socket_peer(a.as_fd()).expect("identify_socket_peer");
        let (own_signer, own_exe) = own_image().expect("own_image");
        assert_eq!(peer.pid, std::process::id());
        // SAFETY: geteuid 没有前置条件。
        assert_eq!(peer.user, UserId::Uid(unsafe { libc::geteuid() }));
        assert_eq!(peer.signer, own_signer);
        assert_eq!(peer.exe, Some(own_exe));
    }

    #[test]
    fn claim_pins_are_well_formed_requirements() {
        let cdhash = Claim::CdHash(vec![0xab; 20]);
        assert_eq!(
            cdhash.requirement_text(),
            format!("cdhash H\"{}\"", "ab".repeat(20))
        );
        // "abc" 的 SHA-1 是公开测试向量，独立于实现。
        let leaf = Claim::Certificate {
            leaf_der: b"abc".to_vec(),
        };
        assert_eq!(
            leaf.requirement_text(),
            "certificate leaf = H\"a9993e364706816aba3e25717850c26c9cd0d89d\""
        );
        for claim in [cdhash, leaf] {
            let text = cf_string(&claim.requirement_text()).unwrap();
            let mut req: SecRef = ptr::null();
            // SAFETY: text 有效；出参 +1 交给 Cf。
            let status =
                unsafe { SecRequirementCreateWithString(text.as_ptr().cast(), 0, &mut req) };
            assert_eq!(status, 0, "要求文本应能编译：{}", claim.requirement_text());
            drop(unsafe { Cf::adopt(req) });
        }
    }

    /// 钉了别的身份，本进程必须过不了：确认 `confirm_running` 真的在比东西。
    /// -67050 是 `errSecCSReqFailed`（CSCommon.h）：签名完好，只是不满足要求。
    #[test]
    fn running_check_rejects_a_different_identity() {
        let code = copy_self().unwrap();
        let (own, _) = own_image().unwrap();
        let SignerIdentity::Image(ImageId::CdHash(own_cdhash)) = own else {
            panic!("测试二进制应是 ad-hoc 签名：{own:?}");
        };
        assert!(confirm_running(&code, Claim::CdHash(own_cdhash)).is_ok());
        let wrong = [
            Claim::CdHash(vec![0; 20]),
            Claim::Certificate {
                leaf_der: b"not our cert".to_vec(),
            },
        ];
        for claim in wrong {
            let err = confirm_running(&code, claim).unwrap_err();
            assert!(err.contains("OSStatus -67050"), "{err}");
        }
    }
}
