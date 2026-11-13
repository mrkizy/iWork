//! 对端身份与判定（设计 3.2；T03 方案 2.4）。
//!
//! 分两层：
//! - 平台代码（`linux.rs` / `macos.rs` / `windows.rs`）只负责把操作系统给的事实装进 [`PeerIdentity`]，
//!   并保证装进来的是**运行中映像**的身份，而不是路径上那个文件的（方案 2.4b）；
//! - [`judge_peer`] 是唯一的判定入口：纯函数，三平台同一份，服务端验客户端、客户端验服务端都调它。
//!
//! 信任根（自己的证书哈希或安装清单）只在进程启动时由 [`SelfIdentity::detect`] 定下一次，之后不读磁盘：
//! 运行中没有任何输入能扩大它（方案 2.4、2.4a）。

use std::fmt;
use std::path::{Path, PathBuf};

mod manifest;
pub use manifest::{MANIFEST_FILE, Manifest, ManifestError};

#[cfg(target_os = "linux")]
pub(crate) mod linux;
#[cfg(target_os = "macos")]
pub(crate) mod macos;
#[cfg(windows)]
pub(crate) mod windows;
// Windows 映像规则里的纯函数：各平台都编译，好让单测在任何主机上跑。
#[cfg_attr(not(windows), allow(dead_code))]
mod windows_image;

#[cfg(target_os = "linux")]
use linux as platform;
#[cfg(target_os = "macos")]
use macos as platform;
#[cfg(windows)]
use windows as platform;

/// 操作系统给出的用户身份。
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum UserId {
    /// Unix 的 uid。
    Uid(u32),
    /// Windows 的用户 SID 文本（`S-1-5-21-…`）。
    Sid(String),
}

impl fmt::Display for UserId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Uid(uid) => write!(f, "uid:{uid}"),
            Self::Sid(sid) => write!(f, "sid:{sid}"),
        }
    }
}

/// 映像身份的两种形式。清单里按同样的标签写：`sha256:<hex>`、`cdhash:<hex>`。
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum ImageId {
    /// 映像文件全文的 SHA-256（Linux、Windows 未签名）。
    Sha256([u8; 32]),
    /// macOS 代码目录哈希（ad-hoc 签名的运行中映像）。
    CdHash(Vec<u8>),
}

impl fmt::Display for ImageId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Sha256(h) => write!(f, "sha256:{}", hex::encode(h)),
            Self::CdHash(h) => write!(f, "cdhash:{}", hex::encode(h)),
        }
    }
}

/// 进程正在运行的映像的身份。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SignerIdentity {
    /// 有效的代码签名，带叶证书 DER 的 SHA-256（macOS、Windows）。
    Certificate { leaf_sha256: [u8; 32] },
    /// 没有证书：Linux 的映像哈希、macOS 的 ad-hoc cdhash、Windows 的未签名文件哈希。
    Image(ImageId),
    /// 取不到：进程已退出、`/proc` 不可读、签名完整性校验失败、映像绑定没有被证明。按拒绝处理。
    Unavailable { reason: String },
}

impl SignerIdentity {
    /// 审计用的摘要：变体 + 哈希前 8 个十六进制位。不记完整哈希，够排查「两边是不是同一个」。
    pub fn summary(&self) -> SignerSummary {
        SignerSummary(match self {
            Self::Certificate { leaf_sha256 } => {
                format!("certificate:{}", short_hex(leaf_sha256))
            }
            Self::Image(ImageId::Sha256(h)) => format!("sha256:{}", short_hex(h)),
            Self::Image(ImageId::CdHash(h)) => format!("cdhash:{}", short_hex(h)),
            Self::Unavailable { reason } => format!("unavailable:{reason}"),
        })
    }
}

fn short_hex(bytes: &[u8]) -> String {
    let mut s = hex::encode(bytes);
    s.truncate(8);
    s
}

/// [`SignerIdentity`] 的审计摘要，见 [`SignerIdentity::summary`]。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SignerSummary(pub String);

impl fmt::Display for SignerSummary {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// 操作系统告诉我们的对端身份。服务端看到的是客户端，客户端看到的是服务端。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PeerIdentity {
    pub pid: u32,
    pub user: UserId,
    /// 对端可执行文件的规范化绝对路径。审计用；判定只在无证书分支用它查「同安装目录」。
    pub exe: Option<PathBuf>,
    pub signer: SignerIdentity,
}

/// 本进程自己的身份与信任根。只由 [`SelfIdentity::detect`] 在启动时建一次（测试可直接构造）。
#[derive(Debug, Clone)]
pub struct SelfIdentity {
    pub user: UserId,
    /// 自己是哪一种，决定走哪条判据：有证书只认同一张叶证书，没有证书只认清单。
    pub signer: SignerIdentity,
    /// 安装目录 = 自己的可执行文件所在目录（规范化）。
    pub exe_dir: PathBuf,
    /// 启动时读进内存的安装清单；之后磁盘上的清单怎么变都不影响。有证书的构建不读。
    pub manifest: Option<Manifest>,
}

impl SelfIdentity {
    /// 算出本进程的身份，并在无证书时读一次安装清单；清单审计当场记下。只在进程启动时调用一次。
    ///
    /// 算不出自己是谁（取不到可执行文件、自己的签名完整性坏了）返回错误：不知道自己是谁就没法判断别人，
    /// 调用方应拒绝启动。清单缺失或解析失败不算错误，记一条日志、`manifest = None`，结果是无证书的对端都连不上。
    ///
    /// 当场记要求调用前已装好日志订阅者；装不了的（后台要先拿单实例锁才写文件日志）用 [`Self::detect_deferred`]。
    pub fn detect() -> Result<SelfIdentity, PeerError> {
        let (own, audit) = Self::detect_deferred()?;
        audit.record();
        Ok(own)
    }

    /// 同 [`Self::detect`]，但清单审计不当场记，交给调用方在日志就绪后 [`StartupAudit::record`]。
    pub fn detect_deferred() -> Result<(SelfIdentity, StartupAudit), PeerError> {
        let user = platform::current_user()?;
        let (signer, exe) = platform::own_image()?;
        if let SignerIdentity::Unavailable { reason } = &signer {
            return Err(PeerError::SelfIdentity(format!(
                "本进程映像身份取不到：{reason}"
            )));
        }
        let exe_dir = exe
            .parent()
            .ok_or_else(|| {
                PeerError::SelfIdentity(format!("可执行文件没有所在目录：{}", exe.display()))
            })?
            .to_path_buf();
        let (manifest, audit) = match signer {
            SignerIdentity::Certificate { .. } => (None, StartupAudit(ManifestOutcome::NotRead)),
            _ => load_manifest_at_start(&exe_dir, &signer),
        };
        Ok((
            SelfIdentity {
                user,
                signer,
                exe_dir,
                manifest,
            },
            audit,
        ))
    }
}

/// 启动时读安装清单的结果，等日志就绪后记成审计（T03 方案 2.12：清单审计启动时记一次）。
///
/// 为什么要留着：tracing 不缓存、不重放事件，没有订阅者时发出的审计直接丢。后台算身份（启动表第 1 步）时文件日志
/// 还没装（拿到单实例锁才装），当场记就丢了。记的是启动那一刻的快照，不重读清单，信任根也不受影响。
#[must_use = "启动时的清单审计要在日志就绪后 record()，丢掉就没有记录了"]
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StartupAudit(ManifestOutcome);

#[derive(Debug, Clone, PartialEq, Eq)]
enum ManifestOutcome {
    /// 有证书的构建不读清单，没有要记的。
    NotRead,
    Loaded {
        path: PathBuf,
        app_version: String,
        entries: usize,
        self_listed: bool,
    },
    Unavailable {
        path: PathBuf,
        reason: String,
    },
}

impl StartupAudit {
    /// 把启动时的清单快照记成审计。按值取走：一次启动只记一次。
    pub fn record(self) {
        match self.0 {
            ManifestOutcome::NotRead => {}
            ManifestOutcome::Loaded {
                path,
                app_version,
                entries,
                self_listed,
            } => crate::audit::manifest_loaded(&path, &app_version, entries, self_listed),
            ManifestOutcome::Unavailable { path, reason } => {
                crate::audit::manifest_unavailable(&path, &reason)
            }
        }
    }
}

fn load_manifest_at_start(
    exe_dir: &Path,
    own: &SignerIdentity,
) -> (Option<Manifest>, StartupAudit) {
    let path = exe_dir.join(MANIFEST_FILE);
    match Manifest::load(&path) {
        Ok(m) => {
            let outcome = ManifestOutcome::Loaded {
                app_version: m.app_version.clone(),
                entries: m.entries(),
                self_listed: matches!(own, SignerIdentity::Image(id) if m.contains(id)),
                path,
            };
            (Some(m), StartupAudit(outcome))
        }
        Err(e) => {
            let outcome = ManifestOutcome::Unavailable {
                path,
                reason: e.to_string(),
            };
            (None, StartupAudit(outcome))
        }
    }
}

/// 判定策略。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TrustPolicy {
    /// 发布与默认：同用户、同签名者或同安装映像都查。
    RequireSigner,
    /// 只在 `dev` feature 下存在：跳过第 2 步（签名 / 映像），同用户照查。
    #[cfg(feature = "dev")]
    DevTrust,
}

/// 判定结果。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PeerVerdict {
    /// `signature_checked` 为 false 只可能出现在 dev 构建的 `DevTrust` 下。
    Accepted {
        signature_checked: bool,
    },
    Rejected(RejectReason),
}

/// 拒绝原因。进审计日志与 `ConnectError::PeerRejected`，不发给对端。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RejectReason {
    DifferentUser {
        peer: UserId,
        own: UserId,
    },
    SignerMismatch {
        peer: SignerSummary,
        own: SignerSummary,
    },
    ImageNotTrusted {
        image: ImageId,
    },
    NoManifest {
        expected_at: PathBuf,
    },
    OutsideInstallDir {
        exe: Option<PathBuf>,
        install_dir: PathBuf,
    },
    Unavailable {
        reason: String,
    },
}

impl RejectReason {
    /// 变体名，审计字段 `reason` 的前半段。
    pub fn kind(&self) -> &'static str {
        match self {
            Self::DifferentUser { .. } => "DifferentUser",
            Self::SignerMismatch { .. } => "SignerMismatch",
            Self::ImageNotTrusted { .. } => "ImageNotTrusted",
            Self::NoManifest { .. } => "NoManifest",
            Self::OutsideInstallDir { .. } => "OutsideInstallDir",
            Self::Unavailable { .. } => "Unavailable",
        }
    }
}

impl fmt::Display for RejectReason {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::DifferentUser { peer, own } => {
                write!(f, "DifferentUser: 对端 {peer}，本进程 {own}")
            }
            Self::SignerMismatch { peer, own } => {
                write!(f, "SignerMismatch: 对端 {peer}，本进程 {own}")
            }
            Self::ImageNotTrusted { image } => {
                let mut s = image.to_string();
                // 只记标签 + 前 8 位，和 SignerSummary 一致。
                if let Some(colon) = s.find(':') {
                    s.truncate(colon + 1 + 8);
                }
                write!(f, "ImageNotTrusted: {s} 不在安装清单里")
            }
            Self::NoManifest { expected_at } => {
                write!(
                    f,
                    "NoManifest: 无证书构建需要安装清单 {}",
                    expected_at.display()
                )
            }
            Self::OutsideInstallDir { exe, install_dir } => write!(
                f,
                "OutsideInstallDir: 对端 {} 不在安装目录 {}",
                exe.as_deref()
                    .map_or_else(|| "（路径未知）".into(), |p| p.display().to_string()),
                install_dir.display()
            ),
            Self::Unavailable { reason } => write!(f, "Unavailable: {reason}"),
        }
    }
}

/// 取身份时的系统错误（取不到 uid 本身）。取不到映像身份不走这里，而是 `SignerIdentity::Unavailable`。
#[derive(Debug, thiserror::Error)]
pub enum PeerError {
    #[error("取对端凭据失败：{0}")]
    Credentials(#[source] std::io::Error),
    #[error("取本进程身份失败：{0}")]
    SelfIdentity(String),
    #[error("取 {path} 的映像身份失败：{reason}")]
    Image { path: PathBuf, reason: String },
}

// ---------------------------------------------------------------- 安装检查用的三个只读函数（T04 方案 2.9）
// 它们不参与对端判定：SelfIdentity、PeerIdentity、judge_peer 一律不动。两种身份分开取、分开用：
// `running_image_id` 是「我正在运行的这一份」，`file_image_ids` 是「磁盘上现在那个文件」，不混用。

/// 本进程**运行中映像**的身份。Linux：`/proc/self/exe` 的全文 SHA-256；macOS：`SecCodeCopySelf` 的动态代码对象的
/// cdhash，有没有证书都取。和 [`SelfIdentity::detect`] 走同一段取运行映像的代码、同一处绑定验证，不读路径上的文件。
///
/// 只在启动时取一次：运行中途路径上的文件被换掉后，macOS 上再取可能报错（不会取成新文件）。
/// Windows（M1 不支持）总是返回错误。
pub fn running_image_id() -> Result<ImageId, PeerError> {
    platform::running_image_id()
}

/// [`file_image_ids`] 要取哪几种身份。
#[derive(Debug, Clone, Copy)]
pub enum Want<'a> {
    /// 只要和这个身份同一种的那一个（判断「磁盘上的是不是我」时传运行映像的身份）。macOS 上要 cdhash 时只读签名，
    /// 不算全文哈希。
    SameKindAs(&'a ImageId),
    /// 这个平台能给的全部（核对清单登记项时用）。
    All,
}

/// 磁盘上一个程序文件的身份，只算调用方要的那几种。Linux、Windows 只有全文 SHA-256；macOS 另有签名里的 cdhash
/// （未签名的文件没有）。文件不存在、读不了、签名结构坏了都是错误（半个文件就是这样）。
pub fn file_image_ids(path: &Path, want: Want<'_>) -> Result<Vec<ImageId>, PeerError> {
    let (sha256, cdhash) = match want {
        Want::All => (true, true),
        Want::SameKindAs(ImageId::Sha256(_)) => (true, false),
        Want::SameKindAs(ImageId::CdHash(_)) => (false, true),
    };
    let fail = |reason: String| PeerError::Image {
        path: path.to_path_buf(),
        reason,
    };
    let mut ids = Vec::new();
    if sha256 {
        ids.push(ImageId::Sha256(
            sha256_file(path).map_err(|e| fail(e.to_string()))?,
        ));
    }
    #[cfg(target_os = "macos")]
    if cdhash && let Some(h) = macos::file_cdhash(path).map_err(fail)? {
        ids.push(ImageId::CdHash(h));
    }
    #[cfg(not(target_os = "macos"))]
    let _ = cdhash;
    Ok(ids)
}

/// 磁盘上一个程序文件是谁签的：有证书 → `Certificate{叶证书的 SHA-256}`；没有 → `Image(映像身份)`；验不过、
/// 读不了 → `Unavailable`。和 [`SelfIdentity::detect`] 判「有没有证书、叶证书是谁」是同一段代码，只是对象从运行中的
/// 映像换成磁盘上的文件（T04p 方案 4.4：出包前按信任规则核包里的两个程序）。
///
/// macOS：读签名信息里的证书或 cdhash，钉成代码要求后对文件做全量静态校验（不验包资源，同 [`SelfIdentity::detect`]）。
/// Linux：文件全文的 SHA-256。Windows（M1 不支持）：`Unavailable`。
pub fn file_signer(path: &Path) -> SignerIdentity {
    platform::file_signer(path)
}

/// macOS：对 .app 做整包静态验签（严格模式、含嵌套代码、全部架构）。每次都完整重查，不记上次的结果。
#[cfg(target_os = "macos")]
pub fn verify_bundle(app: &Path) -> Result<(), PeerError> {
    macos::verify_bundle(app).map_err(|reason| PeerError::Image {
        path: app.to_path_buf(),
        reason,
    })
}

fn sha256_file(path: &Path) -> std::io::Result<[u8; 32]> {
    use sha2::{Digest, Sha256};
    use std::io::Read;
    let mut file = std::fs::File::open(path)?;
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

/// 唯一的判定入口。纯函数，三平台同一份，两端同一份。
///
/// 规则按顺序，第一条命中即返回（方案 2.4）：
/// 1. 用户不同 → 拒。任何 policy 都查。
/// 2. `DevTrust` → 接受，`signature_checked = false`。
/// 3. 对端映像身份取不到 → 拒。
/// 4. 自己有证书：对端必须是同一张叶证书。清单对这条分支没有任何作用。
/// 5. 自己无证书：要有清单、对端在安装目录里、对端映像在清单里。
pub fn judge_peer(own: &SelfIdentity, peer: &PeerIdentity, policy: TrustPolicy) -> PeerVerdict {
    use PeerVerdict::{Accepted, Rejected};
    if peer.user != own.user {
        return Rejected(RejectReason::DifferentUser {
            peer: peer.user.clone(),
            own: own.user.clone(),
        });
    }
    match policy {
        TrustPolicy::RequireSigner => {}
        #[cfg(feature = "dev")]
        TrustPolicy::DevTrust => {
            return Accepted {
                signature_checked: false,
            };
        }
    }
    if let SignerIdentity::Unavailable { reason } = &peer.signer {
        return Rejected(RejectReason::Unavailable {
            reason: reason.clone(),
        });
    }
    let mismatch = || {
        Rejected(RejectReason::SignerMismatch {
            peer: peer.signer.summary(),
            own: own.signer.summary(),
        })
    };
    match &own.signer {
        SignerIdentity::Certificate {
            leaf_sha256: own_leaf,
        } => match &peer.signer {
            SignerIdentity::Certificate { leaf_sha256 } if leaf_sha256 == own_leaf => Accepted {
                signature_checked: true,
            },
            _ => mismatch(),
        },
        SignerIdentity::Image(_) => {
            let Some(manifest) = &own.manifest else {
                return Rejected(RejectReason::NoManifest {
                    expected_at: own.exe_dir.join(MANIFEST_FILE),
                });
            };
            let in_install_dir = peer
                .exe
                .as_deref()
                .and_then(Path::parent)
                .is_some_and(|dir| dir == own.exe_dir);
            if !in_install_dir {
                return Rejected(RejectReason::OutsideInstallDir {
                    exe: peer.exe.clone(),
                    install_dir: own.exe_dir.clone(),
                });
            }
            match &peer.signer {
                SignerIdentity::Image(id) if manifest.contains(id) => Accepted {
                    signature_checked: true,
                },
                SignerIdentity::Image(id) => {
                    Rejected(RejectReason::ImageNotTrusted { image: id.clone() })
                }
                _ => mismatch(),
            }
        }
        // detect() 不会建出这样的 SelfIdentity；测试直接构造时也按拒绝处理，不放行。
        SignerIdentity::Unavailable { reason } => Rejected(RejectReason::Unavailable {
            reason: format!("本进程身份不可用：{reason}"),
        }),
    }
}

#[cfg(test)]
mod tests;

#[cfg(windows)]
pub(crate) use platform::current_user_sid;
#[cfg(windows)]
pub(crate) use platform::identify_process;
#[cfg(unix)]
pub(crate) use platform::identify_socket_peer;
