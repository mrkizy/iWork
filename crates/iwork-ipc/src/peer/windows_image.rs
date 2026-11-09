//! Windows 映像身份里不碰系统调用的规则（方案 2.4 Windows 行、2.4b、Q1）。
//! 单独成文件，非 Windows 也编译、跑单测；系统调用在 `windows.rs`。

use super::SignerIdentity;

/// Windows 的映像绑定有没有被实测证明。
///
/// 2.4b 的绑定依赖一条没实测过的事实：对运行中的进程改名它的 exe 后，`QueryFullProcessImageNameW`
/// 返回**新名**。N2（docs/plans/T03.md 8.2，脚本 `tools/t0/iwork-t03-windows-authenticode.ps1`）证明这一点、
/// 结论写进 `docs/design/t0-findings.md` 之前必须是 `false`。为 `false` 时有证书、无证书两个分支一律
/// `Unavailable`：本进程身份取不到 → `SelfIdentity::detect` 失败 → Windows M1 暂不支持（Q1 已决定）。
pub(crate) const IMAGE_BINDING_VERIFIED: bool = false;

pub(crate) const UNVERIFIED_REASON: &str = "image binding unverified (N2 未实测)";

/// 平台取到的映像身份交出去之前的最后一道门，见 [`IMAGE_BINDING_VERIFIED`]。
pub(crate) fn gate(signer: SignerIdentity) -> SignerIdentity {
    if IMAGE_BINDING_VERIFIED {
        signer
    } else {
        SignerIdentity::Unavailable {
            reason: UNVERIFIED_REASON.into(),
        }
    }
}

// WinVerifyTrust 的返回码。值与 windows-sys 的同名常量相同，Windows 上有一条对拍测试。
const ERROR_SUCCESS: i32 = 0;
const CERT_E_UNTRUSTEDROOT: i32 = 0x800B_0109_u32 as i32;
const TRUST_E_NOSIGNATURE: i32 = 0x800B_0100_u32 as i32;

/// `WinVerifyTrust` 返回码归成三类（方案 2.4 Windows 行）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum TrustCheck {
    /// 签名完整。链不受信（自签证书）也算：判据是「和我同一把私钥签的」，不是「被微软信任的」。
    Signed,
    /// 没有签名：按同一个文件句柄读全文算 SHA-256，走安装清单。
    Unsigned,
    /// 其他一切（摘要不符、格式不认识……）：完整性存疑，按取不到处理。
    Invalid,
}

pub(crate) fn classify_trust_result(code: i32) -> TrustCheck {
    match code {
        ERROR_SUCCESS | CERT_E_UNTRUSTEDROOT => TrustCheck::Signed,
        TRUST_E_NOSIGNATURE => TrustCheck::Unsigned,
        _ => TrustCheck::Invalid,
    }
}

/// 2.4b 第③步：开句柄前查的映像名、开句柄后再查的映像名、按句柄反查的路径，三者一致才算句柄打开的就是运行中的映像。
/// 三个都是 NT 路径（`\Device\HarddiskVolumeN\…`）。NTFS 默认不分大小写，按大写比；
/// 8.3 短名与长名不会被当成一致，结果是拒绝（不安全的方向不会错）。
pub(crate) fn image_paths_agree(before: &str, after: &str, by_handle: &str) -> bool {
    let same = |a: &str, b: &str| a.to_uppercase() == b.to_uppercase();
    same(before, after) && same(after, by_handle)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unverified_binding_turns_every_branch_unavailable() {
        // 改 IMAGE_BINDING_VERIFIED 之前先跑 N2、写 t0-findings，再同步改这条测试。
        let leaf = SignerIdentity::Certificate {
            leaf_sha256: [7; 32],
        };
        let image = SignerIdentity::Image(super::super::ImageId::Sha256([9; 32]));
        for signer in [leaf, image] {
            assert_eq!(
                gate(signer),
                SignerIdentity::Unavailable {
                    reason: UNVERIFIED_REASON.into()
                }
            );
        }
    }

    #[test]
    fn trust_codes_map_to_signed_unsigned_or_invalid() {
        assert_eq!(classify_trust_result(0), TrustCheck::Signed);
        assert_eq!(
            classify_trust_result(0x800B_0109_u32 as i32),
            TrustCheck::Signed
        );
        assert_eq!(
            classify_trust_result(0x800B_0100_u32 as i32),
            TrustCheck::Unsigned
        );
        // TRUST_E_BAD_DIGEST：文件被改过。
        assert_eq!(
            classify_trust_result(0x8009_6010_u32 as i32),
            TrustCheck::Invalid
        );
        // TRUST_E_SUBJECT_FORM_UNKNOWN：不是 PE。
        assert_eq!(
            classify_trust_result(0x800B_0003_u32 as i32),
            TrustCheck::Invalid
        );
    }

    #[test]
    fn image_paths_must_agree_before_open_after_open_and_by_handle() {
        let p = r"\Device\HarddiskVolume3\iWork\iwork-daemon.exe";
        assert!(image_paths_agree(p, p, p));
        assert!(image_paths_agree(
            p,
            p,
            r"\Device\HarddiskVolume3\IWORK\iwork-daemon.exe"
        ));
        let renamed = r"\Device\HarddiskVolume3\iWork\iwork-daemon.old.exe";
        assert!(!image_paths_agree(p, renamed, p), "两次查名之间被改名");
        assert!(
            !image_paths_agree(p, p, renamed),
            "句柄打开的不是同一个文件"
        );
    }

    #[cfg(windows)]
    #[test]
    fn trust_code_values_match_windows_sys() {
        use windows_sys::Win32::Foundation as f;
        assert_eq!(ERROR_SUCCESS as u32, f::ERROR_SUCCESS);
        assert_eq!(CERT_E_UNTRUSTEDROOT, f::CERT_E_UNTRUSTEDROOT);
        assert_eq!(TRUST_E_NOSIGNATURE, f::TRUST_E_NOSIGNATURE);
    }
}
