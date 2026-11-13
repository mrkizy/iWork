//! 出包前的核对（T04p 方案 4.4）：一个装配好的包，能不能发布。三件事，都用运行时的同一段代码：
//!
//! 1. 恰好是一种形态：[`super::forms`] 的每一行 `check`，恰好一行 `Complete`、别的都是 `NotThisForm`
//!    （T04 方案 2.9 第二问；另一行是 `Incomplete` 也不放过）。
//! 2. 装上之后两个程序能互认：对包里的后台与客户端各取「磁盘上是谁签的」（[`peer::file_signer`]），再照
//!    [`SelfIdentity::detect`] 的规矩把清单读进来（有证书的不读），用 [`judge_peer`] 两个方向各判一次（T03 方案 2.4）。
//! 3. 给了预期的叶证书时：两个程序都是那张证书签的。
//!
//! 不改 [`super::PackageForm`] 的职责：它只回答「磁盘上的包齐不齐」，能不能互认由第 2 件用信任规则本身回答。
//! 第 2 件是拿磁盘上的文件按规则推的，不是真的握一次手（方案 L7）。

use std::path::Path;

use iwork_ipc::peer::{
    self, MANIFEST_FILE, Manifest, PeerIdentity, PeerVerdict, SelfIdentity, SignerIdentity,
    TrustPolicy, UserId, judge_peer,
};

use super::{CLIENT_NAME, DAEMON_NAME, FormVerdict, forms, installed_complete, program_file};

/// 一件事的结论。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Finding {
    /// 「形态」「互认」「证书」。
    pub item: &'static str,
    pub passed: bool,
    /// 给人看的一行：各形态的结论、两个方向的判定、两个程序的签名者。
    pub detail: String,
}

/// 核对 `dir`（包里放程序的目录：macOS 是 `X.app/Contents/MacOS`，Linux 是成品目录）。`expect_leaf` 给了就核第 3 件。
/// 全部 `passed` 才能发布。
pub fn check_package(dir: &Path, expect_leaf: Option<&[u8; 32]>) -> Vec<Finding> {
    let signers = [DAEMON_NAME, CLIENT_NAME].map(|name| {
        let signer = peer::file_signer(&dir.join(program_file(name)));
        (name, signer)
    });
    let mut findings = vec![one_form(dir), mutual_trust(dir, &signers)];
    if let Some(leaf) = expect_leaf {
        findings.push(expected_leaf(&signers, leaf));
    }
    findings
}

/// 第 1 件：恰好一行 `Complete`，别的都是 `NotThisForm`。
fn one_form(dir: &Path) -> Finding {
    let per_form = installed_complete(&forms(), dir).per_form;
    let complete = per_form
        .iter()
        .filter(|(_, v)| *v == FormVerdict::Complete)
        .count();
    let others_absent = per_form
        .iter()
        .all(|(_, v)| matches!(v, FormVerdict::Complete | FormVerdict::NotThisForm));
    let detail = per_form
        .iter()
        .map(|(name, v)| format!("{name}={}", verdict_text(v)))
        .collect::<Vec<_>>()
        .join(" ");
    Finding {
        item: "形态",
        passed: complete == 1 && others_absent,
        detail,
    }
}

fn verdict_text(v: &FormVerdict) -> String {
    match v {
        FormVerdict::NotThisForm => "NotThisForm".into(),
        FormVerdict::Complete => "Complete".into(),
        FormVerdict::Incomplete(why) => format!("Incomplete({why})"),
    }
}

/// 第 2 件：两个方向各判一次，都要「接受、验过签名」。
fn mutual_trust(dir: &Path, signers: &[(&'static str, SignerIdentity); 2]) -> Finding {
    // 包是同一个账号装的：两端的用户按定义相同。这里只比签名者，用户这一条给同一个值
    let user = UserId::Uid(0);
    let manifest = Manifest::load(&dir.join(MANIFEST_FILE)).ok();
    let identity_of = |signer: &SignerIdentity| SelfIdentity {
        user: user.clone(),
        signer: signer.clone(),
        exe_dir: dir.to_path_buf(),
        // 照 SelfIdentity::detect：有证书的构建不读清单
        manifest: match signer {
            SignerIdentity::Certificate { .. } => None,
            _ => manifest.clone(),
        },
    };
    let peer_of = |name: &str, signer: &SignerIdentity| PeerIdentity {
        pid: 0,
        user: user.clone(),
        exe: Some(dir.join(program_file(name))),
        signer: signer.clone(),
    };
    let [(a, a_signer), (b, b_signer)] = signers;
    let directions = [
        (
            *a,
            *b,
            judge_peer(
                &identity_of(a_signer),
                &peer_of(b, b_signer),
                TrustPolicy::RequireSigner,
            ),
        ),
        (
            *b,
            *a,
            judge_peer(
                &identity_of(b_signer),
                &peer_of(a, a_signer),
                TrustPolicy::RequireSigner,
            ),
        ),
    ];
    let passed = directions.iter().all(|(_, _, v)| {
        *v == PeerVerdict::Accepted {
            signature_checked: true,
        }
    });
    let mut detail = signers
        .iter()
        .map(|(name, s)| format!("{name}={}", signer_text(s)))
        .collect::<Vec<_>>();
    detail.extend(
        directions
            .iter()
            .map(|(own, other, v)| format!("{own} 看 {other}：{v:?}")),
    );
    Finding {
        item: "互认",
        passed,
        detail: detail.join("；"),
    }
}

/// 第 3 件：两个程序的签名者都是证书，叶证书都等于预期的。
fn expected_leaf(signers: &[(&'static str, SignerIdentity); 2], leaf: &[u8; 32]) -> Finding {
    let passed = signers.iter().all(
        |(_, s)| matches!(s, SignerIdentity::Certificate { leaf_sha256 } if leaf_sha256 == leaf),
    );
    Finding {
        item: "证书",
        passed,
        detail: format!(
            "预期 {}；{}",
            hex::encode(leaf),
            signers
                .iter()
                .map(|(name, s)| format!("{name}={}", signer_text(s)))
                .collect::<Vec<_>>()
                .join("；")
        ),
    }
}

/// 签名者的完整写法：证书写出叶证书的完整 SHA-256（公开证书的摘要，P3 要拿它对拍），映像写出身份。
fn signer_text(s: &SignerIdentity) -> String {
    match s {
        SignerIdentity::Certificate { leaf_sha256 } => {
            format!("certificate:{}", hex::encode(leaf_sha256))
        }
        SignerIdentity::Image(id) => id.to_string(),
        SignerIdentity::Unavailable { reason } => format!("unavailable:{reason}"),
    }
}
