//! 安装清单 `iwork-manifest.json`（T03 方案 2.4a）：只给无证书构建用，只登记这一版两个二进制的映像身份。
//!
//! 由 `scripts/write-install-manifest.sh` 在打包时生成，放在两个二进制旁边。启动时读一次（[`super::SelfIdentity::detect`]），
//! 之后不再碰磁盘。能改它的人也能直接换掉二进制，所以不另加签名。

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

use serde::Deserialize;

use super::ImageId;

/// 清单文件名，与两个二进制同目录。
pub const MANIFEST_FILE: &str = "iwork-manifest.json";

const SUPPORTED_VERSION: u32 = 1;

/// 解析好的清单。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Manifest {
    pub app_version: String,
    /// 二进制名 → 它的映像身份（一个二进制在 macOS 上可能同时登记 sha256 与 cdhash）。
    pub binaries: BTreeMap<String, Vec<ImageId>>,
    all: BTreeSet<ImageId>,
}

#[derive(Debug, thiserror::Error)]
pub enum ManifestError {
    #[error("读清单 {path} 失败：{source}")]
    Read {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("清单不是合法 JSON 或缺字段：{0}")]
    Json(#[from] serde_json::Error),
    #[error("清单版本 {found} 不支持，只认 {SUPPORTED_VERSION}")]
    Version { found: u32 },
    #[error("清单里 {binary} 的条目 {entry:?} 格式不对：{why}")]
    Entry {
        binary: String,
        entry: String,
        why: &'static str,
    },
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Raw {
    manifest_version: u32,
    app_version: String,
    binaries: BTreeMap<String, Vec<String>>,
}

impl Manifest {
    pub fn load(path: &Path) -> Result<Manifest, ManifestError> {
        let text = std::fs::read_to_string(path).map_err(|source| ManifestError::Read {
            path: path.to_path_buf(),
            source,
        })?;
        Self::parse(&text)
    }

    pub fn parse(text: &str) -> Result<Manifest, ManifestError> {
        let raw: Raw = serde_json::from_str(text)?;
        if raw.manifest_version != SUPPORTED_VERSION {
            return Err(ManifestError::Version {
                found: raw.manifest_version,
            });
        }
        let mut binaries = BTreeMap::new();
        let mut all = BTreeSet::new();
        for (binary, entries) in raw.binaries {
            let mut ids = Vec::with_capacity(entries.len());
            for entry in entries {
                let id = parse_entry(&entry).map_err(|why| ManifestError::Entry {
                    binary: binary.clone(),
                    entry: entry.clone(),
                    why,
                })?;
                all.insert(id.clone());
                ids.push(id);
            }
            binaries.insert(binary, ids);
        }
        Ok(Manifest {
            app_version: raw.app_version,
            binaries,
            all,
        })
    }

    /// 映像是否登记在清单里（不分是哪个二进制）。
    pub fn contains(&self, id: &ImageId) -> bool {
        self.all.contains(id)
    }

    /// 条目总数（审计用）。
    pub fn entries(&self) -> usize {
        self.all.len()
    }
}

fn parse_entry(entry: &str) -> Result<ImageId, &'static str> {
    let (tag, hex_part) = entry.split_once(':').ok_or("缺少 `标签:` 前缀")?;
    let bytes = hex::decode(hex_part).map_err(|_| "不是十六进制")?;
    match tag {
        "sha256" => {
            let arr: [u8; 32] = bytes.try_into().map_err(|_| "sha256 必须是 32 字节")?;
            Ok(ImageId::Sha256(arr))
        }
        // cdhash 是代码目录哈希截到 20 字节（SHA-1 或截断的 SHA-256）。
        "cdhash" if bytes.len() == 20 => Ok(ImageId::CdHash(bytes)),
        "cdhash" => Err("cdhash 必须是 20 字节"),
        _ => Err("未知标签，只认 sha256、cdhash"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SHA: &str = "sha256:00112233445566778899aabbccddeeff00112233445566778899aabbccddeeff";
    const CD: &str = "cdhash:00112233445566778899aabbccddeeff00112233";

    fn doc(entries: &str) -> String {
        format!(
            r#"{{"manifest_version":1,"app_version":"0.2.0","binaries":{{"iwork-daemon":[{entries}],"iwork-client":[]}}}}"#
        )
    }

    #[test]
    fn parses_both_tags_and_answers_membership() {
        let m = Manifest::parse(&doc(&format!("\"{SHA}\",\"{CD}\""))).unwrap();
        assert_eq!(m.app_version, "0.2.0");
        assert_eq!(m.entries(), 2);
        assert!(m.contains(&ImageId::Sha256(
            hex::decode(&SHA[7..]).unwrap().try_into().unwrap()
        )));
        assert!(m.contains(&ImageId::CdHash(hex::decode(&CD[7..]).unwrap())));
        assert!(!m.contains(&ImageId::Sha256([0; 32])));
    }

    #[test]
    fn manifest_parse_rejects_unknown_tag() {
        let err = Manifest::parse(&doc("\"md5:0011\"")).unwrap_err();
        assert!(matches!(err, ManifestError::Entry { why, .. } if why.contains("未知标签")));
    }

    #[test]
    fn rejects_wrong_length_bad_hex_version_and_unknown_fields() {
        assert!(Manifest::parse(&doc("\"sha256:0011\"")).is_err());
        assert!(Manifest::parse(&doc("\"sha256:zz\"")).is_err());
        assert!(Manifest::parse(&doc("\"cdhash:0011\"")).is_err());
        let v2 = doc("").replace("\"manifest_version\":1", "\"manifest_version\":2");
        assert!(matches!(
            Manifest::parse(&v2),
            Err(ManifestError::Version { found: 2 })
        ));
        let extra = doc("").replace("\"app_version\"", "\"signers\":[],\"app_version\"");
        assert!(matches!(
            Manifest::parse(&extra),
            Err(ManifestError::Json(_))
        ));
    }
}
