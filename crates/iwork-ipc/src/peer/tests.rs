//! judge_peer 全分支单测（方案 7.1 验收 1、2）。三平台都跑：判定与平台无关。

use super::*;

const DIR: &str = "/opt/iwork";

fn sha(b: u8) -> ImageId {
    ImageId::Sha256([b; 32])
}

fn manifest_with(ids: &[ImageId]) -> Manifest {
    let entries: Vec<String> = ids.iter().map(|id| format!("\"{id}\"")).collect();
    Manifest::parse(&format!(
        r#"{{"manifest_version":1,"app_version":"1.0.0","binaries":{{"iwork-daemon":[{}]}}}}"#,
        entries.join(",")
    ))
    .unwrap()
}

fn own_image(manifest: Option<Manifest>) -> SelfIdentity {
    SelfIdentity {
        user: UserId::Uid(501),
        signer: SignerIdentity::Image(sha(1)),
        exe_dir: PathBuf::from(DIR),
        manifest,
    }
}

fn own_cert(leaf: u8) -> SelfIdentity {
    SelfIdentity {
        user: UserId::Uid(501),
        signer: SignerIdentity::Certificate {
            leaf_sha256: [leaf; 32],
        },
        exe_dir: PathBuf::from(DIR),
        manifest: None,
    }
}

fn peer(user: UserId, signer: SignerIdentity) -> PeerIdentity {
    PeerIdentity {
        pid: 42,
        user,
        exe: Some(PathBuf::from(DIR).join("iwork-client")),
        signer,
    }
}

const ACCEPT: PeerVerdict = PeerVerdict::Accepted {
    signature_checked: true,
};

fn rejected_kind(v: PeerVerdict) -> &'static str {
    match v {
        PeerVerdict::Rejected(r) => r.kind(),
        PeerVerdict::Accepted { .. } => "Accepted",
    }
}

#[test]
fn judge_peer_same_user_same_image_in_manifest_accepted() {
    let own = own_image(Some(manifest_with(&[sha(1), sha(2)])));
    let p = peer(UserId::Uid(501), SignerIdentity::Image(sha(2)));
    assert_eq!(judge_peer(&own, &p, TrustPolicy::RequireSigner), ACCEPT);
}

#[test]
fn judge_peer_different_uid_rejected() {
    let own = own_image(Some(manifest_with(&[sha(2)])));
    let p = peer(UserId::Uid(502), SignerIdentity::Image(sha(2)));
    assert_eq!(
        judge_peer(&own, &p, TrustPolicy::RequireSigner),
        PeerVerdict::Rejected(RejectReason::DifferentUser {
            peer: UserId::Uid(502),
            own: UserId::Uid(501)
        })
    );
}

#[test]
fn judge_peer_different_sid_rejected_same_sid_accepted() {
    let mut own = own_cert(7);
    own.user = UserId::Sid("S-1-5-21-1-1-1-1001".into());
    let cert = SignerIdentity::Certificate {
        leaf_sha256: [7; 32],
    };
    let other = peer(UserId::Sid("S-1-5-21-1-1-1-1002".into()), cert.clone());
    assert_eq!(
        rejected_kind(judge_peer(&own, &other, TrustPolicy::RequireSigner)),
        "DifferentUser"
    );
    let same = peer(UserId::Sid("S-1-5-21-1-1-1-1001".into()), cert);
    assert_eq!(judge_peer(&own, &same, TrustPolicy::RequireSigner), ACCEPT);
}

#[cfg(feature = "dev")]
#[test]
fn judge_peer_dev_trust_skips_signer_but_not_user() {
    let own = own_image(None);
    let unsigned = peer(UserId::Uid(501), SignerIdentity::Image(sha(9)));
    assert_eq!(
        judge_peer(&own, &unsigned, TrustPolicy::DevTrust),
        PeerVerdict::Accepted {
            signature_checked: false
        }
    );
    let other_user = peer(UserId::Uid(0), SignerIdentity::Image(sha(9)));
    assert_eq!(
        rejected_kind(judge_peer(&own, &other_user, TrustPolicy::DevTrust)),
        "DifferentUser"
    );
}

#[test]
fn judge_peer_signer_mismatch() {
    let own = own_cert(7);
    let p = peer(
        UserId::Uid(501),
        SignerIdentity::Certificate {
            leaf_sha256: [8; 32],
        },
    );
    match judge_peer(&own, &p, TrustPolicy::RequireSigner) {
        PeerVerdict::Rejected(RejectReason::SignerMismatch { peer, own }) => {
            assert_eq!(peer.0, "certificate:08080808");
            assert_eq!(own.0, "certificate:07070707");
        }
        v => panic!("应为 SignerMismatch，得到 {v:?}"),
    }
    let same = peer(
        UserId::Uid(501),
        SignerIdentity::Certificate {
            leaf_sha256: [7; 32],
        },
    );
    assert_eq!(judge_peer(&own, &same, TrustPolicy::RequireSigner), ACCEPT);
}

#[test]
fn judge_peer_certificate_ignores_manifest() {
    // 有证书的构建只认自己的叶证书：清单里写了对端的映像也不认。
    let mut own = own_cert(7);
    own.manifest = Some(manifest_with(&[sha(2)]));
    let p = peer(UserId::Uid(501), SignerIdentity::Image(sha(2)));
    assert_eq!(
        rejected_kind(judge_peer(&own, &p, TrustPolicy::RequireSigner)),
        "SignerMismatch"
    );
}

#[test]
fn judge_peer_mixed_cert_and_image_rejected() {
    let own = own_image(Some(manifest_with(&[sha(2)])));
    let p = peer(
        UserId::Uid(501),
        SignerIdentity::Certificate {
            leaf_sha256: [2; 32],
        },
    );
    assert_eq!(
        rejected_kind(judge_peer(&own, &p, TrustPolicy::RequireSigner)),
        "SignerMismatch"
    );
}

#[test]
fn judge_peer_image_without_manifest() {
    let own = own_image(None);
    let p = peer(UserId::Uid(501), SignerIdentity::Image(sha(1)));
    assert_eq!(
        judge_peer(&own, &p, TrustPolicy::RequireSigner),
        PeerVerdict::Rejected(RejectReason::NoManifest {
            expected_at: PathBuf::from(DIR).join(MANIFEST_FILE)
        })
    );
}

#[test]
fn judge_peer_image_not_in_manifest() {
    let own = own_image(Some(manifest_with(&[sha(1)])));
    let p = peer(UserId::Uid(501), SignerIdentity::Image(sha(3)));
    assert_eq!(
        judge_peer(&own, &p, TrustPolicy::RequireSigner),
        PeerVerdict::Rejected(RejectReason::ImageNotTrusted { image: sha(3) })
    );
}

#[test]
fn judge_peer_rejects_other_version_image() {
    // 旧后台的清单只登记 v1 的两个映像；v2 客户端的映像不在里面。
    let v1 = [sha(0x11), sha(0x12)];
    let own = own_image(Some(manifest_with(&v1)));
    let v2_client = peer(UserId::Uid(501), SignerIdentity::Image(sha(0x22)));
    assert_eq!(
        rejected_kind(judge_peer(&own, &v2_client, TrustPolicy::RequireSigner)),
        "ImageNotTrusted"
    );
    let v1_client = peer(UserId::Uid(501), SignerIdentity::Image(sha(0x12)));
    assert_eq!(
        judge_peer(&own, &v1_client, TrustPolicy::RequireSigner),
        ACCEPT
    );
}

#[test]
fn judge_peer_outside_install_dir() {
    let own = own_image(Some(manifest_with(&[sha(2)])));
    let mut p = peer(UserId::Uid(501), SignerIdentity::Image(sha(2)));
    p.exe = Some(PathBuf::from("/tmp/copy/iwork-client"));
    assert_eq!(
        rejected_kind(judge_peer(&own, &p, TrustPolicy::RequireSigner)),
        "OutsideInstallDir"
    );
    p.exe = None;
    assert_eq!(
        rejected_kind(judge_peer(&own, &p, TrustPolicy::RequireSigner)),
        "OutsideInstallDir"
    );
}

#[test]
fn judge_peer_unavailable_rejected() {
    for own in [own_cert(7), own_image(Some(manifest_with(&[sha(2)])))] {
        let p = peer(
            UserId::Uid(501),
            SignerIdentity::Unavailable {
                reason: "image binding unverified".into(),
            },
        );
        assert_eq!(
            judge_peer(&own, &p, TrustPolicy::RequireSigner),
            PeerVerdict::Rejected(RejectReason::Unavailable {
                reason: "image binding unverified".into()
            })
        );
    }
}

#[test]
fn reject_reason_display_names_variant_and_short_hash() {
    let r = RejectReason::ImageNotTrusted { image: sha(0xab) };
    let s = r.to_string();
    assert!(s.starts_with("ImageNotTrusted: sha256:abababab "), "{s}");
    assert!(!s.contains(&"ab".repeat(32)), "不记完整哈希：{s}");
}
