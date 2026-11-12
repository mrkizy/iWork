//! 自行重启的判据（T04 方案 2.9）。只有纯函数与「包的形态」登记表；什么时候问、问完做什么在
//! `tasks::install_watch`。
//!
//! 判据只有一条：磁盘上的后台程序**不是我正在运行的这一份**（或我的信任材料换了），并且磁盘上有一个**装齐的**包
//! → 重启。拆成两问：
//! - 第一问 [`same_as_running`]（便宜）：参照物是启动时从运行映像取的身份，不设基线，后台什么时候起来都判得对。
//! - 第二问 [`installed_complete`]（贵）：按磁盘上是哪种包判，登记表两行都试，有一行说装齐就是装齐。
//!
//! 再加一种包的形态 = [`forms`] 里加一行。

use std::path::Path;

use iwork_ipc::peer::{self, ImageId, MANIFEST_FILE, Manifest, SelfIdentity, SignerIdentity, Want};

pub mod manifest_form;
#[cfg(target_os = "macos")]
pub mod signed_bundle_form;

/// 磁盘上的清单。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DiskManifest {
    Missing,
    Unparsable,
    Parsed(Manifest),
}

/// 第一问要看的磁盘上的样子。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DiskView {
    /// 磁盘上后台程序文件的身份（只取和运行身份同一种的）；读不了（没有、写了一半）是 `None`。
    pub daemon: Option<Vec<ImageId>>,
    pub manifest: DiskManifest,
}

/// 读第一问要的磁盘状态：后台程序文件的身份（只取和 `running` 同一种）与它旁边的清单。
pub fn read_disk(daemon_path: &Path, running: &ImageId) -> DiskView {
    let daemon = peer::file_image_ids(daemon_path, Want::SameKindAs(running)).ok();
    let manifest_path = daemon_path
        .parent()
        .map(|d| d.join(MANIFEST_FILE))
        .unwrap_or_default();
    let manifest = match std::fs::read_to_string(&manifest_path) {
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => DiskManifest::Missing,
        Err(_) => DiskManifest::Unparsable,
        Ok(text) => match Manifest::parse(&text) {
            Ok(m) => DiskManifest::Parsed(m),
            Err(_) => DiskManifest::Unparsable,
        },
    };
    DiskView { daemon, manifest }
}

/// 第一问（纯函数）：磁盘上的和我还是同一份吗？
///
/// 同一份 ⇔ 运行身份在磁盘后台文件的身份里，且（我有证书，或磁盘清单与我启动时读进内存的清单相同——都没有也算相同）。
/// 「信任材料换没换」只对无证书构建有意义：清单换了不重启就不认新客户端。
pub fn same_as_running(running: &ImageId, own: &SelfIdentity, disk: &DiskView) -> bool {
    let daemon_is_me = disk
        .daemon
        .as_ref()
        .is_some_and(|ids| ids.contains(running));
    if !daemon_is_me {
        return false;
    }
    match own.signer {
        SignerIdentity::Certificate { .. } => true,
        SignerIdentity::Image(_) | SignerIdentity::Unavailable { .. } => {
            match (&own.manifest, &disk.manifest) {
                (Some(mine), DiskManifest::Parsed(theirs)) => mine == theirs,
                (None, DiskManifest::Missing | DiskManifest::Unparsable) => true,
                (Some(_), DiskManifest::Missing | DiskManifest::Unparsable)
                | (None, DiskManifest::Parsed(_)) => false,
            }
        }
    }
}

/// 一种包的形态对磁盘上的安装怎么看。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FormVerdict {
    NotThisForm,
    /// 是这种形态但没装齐；带上卡在哪（日志用）。
    Incomplete(String),
    Complete,
}

/// 一种包的形态。只读，每次都完整地查，不记上次的结果。
pub trait PackageForm: Send + Sync {
    fn name(&self) -> &'static str;
    fn check(&self, exe_dir: &Path) -> FormVerdict;
}

/// 包的形态登记表：带清单的包（全平台）、整包签名的包（macOS）。
pub fn forms() -> Vec<Box<dyn PackageForm>> {
    #[allow(unused_mut)]
    let mut v: Vec<Box<dyn PackageForm>> = vec![Box::new(manifest_form::ManifestForm)];
    #[cfg(target_os = "macos")]
    v.push(Box::new(signed_bundle_form::SignedBundleForm));
    v
}

/// 第二问的结果：有没有一行判装齐，以及各行卡在哪（「安装中」太久时记 warn 用）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Completeness {
    pub complete: bool,
    pub per_form: Vec<(&'static str, FormVerdict)>,
}

/// 第二问：登记表每一行都试，有一行 `Complete` 即装齐。
pub fn installed_complete(forms: &[Box<dyn PackageForm>], exe_dir: &Path) -> Completeness {
    let per_form: Vec<_> = forms.iter().map(|f| (f.name(), f.check(exe_dir))).collect();
    Completeness {
        complete: per_form.iter().any(|(_, v)| *v == FormVerdict::Complete),
        per_form,
    }
}

/// 判定结果。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Verdict {
    Unchanged,
    Installing,
    Restart,
}

/// 纯函数：两问的答案 → 结论。
pub fn judge(same: bool, complete: bool) -> Verdict {
    match (same, complete) {
        (true, _) => Verdict::Unchanged,
        (false, true) => Verdict::Restart,
        (false, false) => Verdict::Installing,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn id(b: u8) -> ImageId {
        ImageId::Sha256([b; 32])
    }

    fn manifest(b: u8) -> Manifest {
        Manifest::parse(&format!(
            r#"{{"manifest_version":1,"app_version":"1","binaries":{{"iwork-daemon":["{}"]}}}}"#,
            id(b)
        ))
        .unwrap()
    }

    fn own(signer: SignerIdentity, m: Option<Manifest>) -> SelfIdentity {
        SelfIdentity {
            user: iwork_ipc::peer::UserId::Uid(1),
            signer,
            exe_dir: PathBuf::from("/opt/iwork"),
            manifest: m,
        }
    }

    fn disk(daemon: Option<u8>, m: DiskManifest) -> DiskView {
        DiskView {
            daemon: daemon.map(|b| vec![id(b)]),
            manifest: m,
        }
    }

    /// 方案 S7 的表：期望值逐行按判据写出。
    #[test]
    fn same_as_running_table() {
        let me = id(0xA);
        let image = |m| own(SignerIdentity::Image(id(0xA)), m);
        let cert = own(
            SignerIdentity::Certificate {
                leaf_sha256: [1; 32],
            },
            None,
        );
        let parsed = |b| DiskManifest::Parsed(manifest(b));
        let rows: Vec<(&str, SelfIdentity, DiskView, bool)> = vec![
            (
                "后台文件是我且清单相同",
                image(Some(manifest(1))),
                disk(Some(0xA), parsed(1)),
                true,
            ),
            (
                "后台文件不是我",
                image(Some(manifest(1))),
                disk(Some(0xB), parsed(1)),
                false,
            ),
            (
                "后台文件读不了",
                image(Some(manifest(1))),
                disk(None, parsed(1)),
                false,
            ),
            (
                "无证书：清单换了",
                image(Some(manifest(1))),
                disk(Some(0xA), parsed(2)),
                false,
            ),
            (
                "无证书：清单没了",
                image(Some(manifest(1))),
                disk(Some(0xA), DiskManifest::Missing),
                false,
            ),
            (
                "无证书：清单坏了",
                image(Some(manifest(1))),
                disk(Some(0xA), DiskManifest::Unparsable),
                false,
            ),
            (
                "无证书：清单新出现",
                image(None),
                disk(Some(0xA), parsed(1)),
                false,
            ),
            (
                "无证书：都没有清单",
                image(None),
                disk(Some(0xA), DiskManifest::Missing),
                true,
            ),
            (
                "有证书：清单怎么变都不影响",
                cert.clone(),
                disk(Some(0xA), parsed(2)),
                true,
            ),
            (
                "有证书：后台文件不是我",
                cert,
                disk(Some(0xB), DiskManifest::Missing),
                false,
            ),
            // S7a：磁盘清单与内存里的相同，并不妨碍「后台文件不是我」
            (
                "后台文件不是我、清单相同",
                image(Some(manifest(1))),
                disk(Some(0xB), parsed(1)),
                false,
            ),
        ];
        for (name, own, disk, want) in rows {
            assert_eq!(same_as_running(&me, &own, &disk), want, "{name}");
        }
    }

    #[test]
    fn judge_table() {
        assert_eq!(judge(true, true), Verdict::Unchanged);
        assert_eq!(judge(true, false), Verdict::Unchanged);
        assert_eq!(judge(false, true), Verdict::Restart);
        assert_eq!(judge(false, false), Verdict::Installing);
    }

    struct Fixed(&'static str, FormVerdict);
    impl PackageForm for Fixed {
        fn name(&self) -> &'static str {
            self.0
        }
        fn check(&self, _: &Path) -> FormVerdict {
            self.1.clone()
        }
    }

    /// 登记表每一行都试：任一行 Complete 即装齐，与顺序无关；都不是 / 都没装齐 → 没装齐。
    #[test]
    fn installed_complete_tries_every_form() {
        use FormVerdict::*;
        let inc = || Incomplete("x".into());
        let cases: Vec<(Vec<FormVerdict>, bool)> = vec![
            (vec![NotThisForm, NotThisForm], false),
            (vec![inc(), NotThisForm], false),
            (vec![NotThisForm, Complete], true),
            (vec![Complete, NotThisForm], true),
            (vec![inc(), Complete], true),
            (vec![inc(), inc()], false),
        ];
        for (verdicts, want) in cases {
            let forms: Vec<Box<dyn PackageForm>> = verdicts
                .iter()
                .map(|v| Box::new(Fixed("f", v.clone())) as Box<dyn PackageForm>)
                .collect();
            let c = installed_complete(&forms, Path::new("/x"));
            assert_eq!(c.complete, want, "{verdicts:?}");
            assert_eq!(c.per_form.len(), verdicts.len());
        }
    }
}
