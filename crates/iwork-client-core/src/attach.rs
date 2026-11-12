//! 附件的客户端部分（任务卡范围 5；设计 3.7）：20MB 检查在这里（不在网页，方案 2.5）；本机文件只传路径；
//! 粘贴的数据用 `Chunk` 分片上传。凭据与私钥的判定在后台（返回 NEEDS_CONFIRMATION / PRIVATE_KEY_REFUSED）。

use std::path::Path;

use bytes::Bytes;
use iwork_proto::limits::MAX_CHUNK_DATA;
use iwork_proto::pb;

/// 单个附件上限（ui-m1.md 第 2 节第 6 条）。
pub const ATTACHMENT_MAX_BYTES: u64 = 20 * 1024 * 1024;

pub fn too_large(name: &str) -> pb::IntentError {
    pb::IntentError {
        code: "attachment_too_large".into(),
        message: format!("「{name}」超过 20MB，不能作为附件"),
        retryable: false,
    }
}

pub fn check_size(name: &str, len: u64) -> Result<(), pb::IntentError> {
    if len > ATTACHMENT_MAX_BYTES {
        Err(too_large(name))
    } else {
        Ok(())
    }
}

fn file_name(path: &str) -> String {
    Path::new(path)
        .file_name()
        .map_or_else(|| path.to_owned(), |n| n.to_string_lossy().into_owned())
}

/// 拖入或选择的本机文件 → 每个一条 `UploadAttachment{local_path}`。任一超过 20MB 就整批不发。
pub fn path_commands(
    conversation_id: &str,
    paths: &[String],
) -> Result<Vec<pb::UploadAttachment>, pb::IntentError> {
    let mut out = Vec::with_capacity(paths.len());
    for p in paths {
        let name = file_name(p);
        let len = std::fs::metadata(p)
            .map_err(|e| pb::IntentError {
                code: "attachment_unreadable".into(),
                message: format!("读不到「{name}」：{e}"),
                retryable: false,
            })?
            .len();
        check_size(&name, len)?;
        out.push(pb::UploadAttachment {
            conversation_id: conversation_id.to_owned(),
            name,
            mime: String::new(),
            source: Some(pb::upload_attachment::Source::LocalPath(p.clone())),
        });
    }
    Ok(out)
}

/// 附件从哪来。本机文件（拖入、系统选择框）的路径都是壳给的，不经网页。
#[derive(Debug, Clone, PartialEq)]
pub enum Source {
    Paths {
        conversation_id: String,
        paths: Vec<String>,
    },
    Pasted(pb::AttachPasted),
}

/// 一批附件要发的东西：先发完分片，再逐条发命令。
pub struct Batch {
    pub chunks: Vec<pb::Chunk>,
    pub commands: Vec<pb::UploadAttachment>,
}

/// 检查大小并转成要发的分片与命令；任一不合格整批不发。
pub fn batch(source: Source) -> Result<Batch, pb::IntentError> {
    match source {
        Source::Paths {
            conversation_id,
            paths,
        } => path_commands(&conversation_id, &paths).map(|commands| Batch {
            chunks: Vec::new(),
            commands,
        }),
        Source::Pasted(x) => {
            check_size(&x.name, x.data.len() as u64)?;
            let upload_id = ulid::Ulid::generate().to_string();
            Ok(Batch {
                chunks: chunks(&upload_id, &x.data),
                commands: vec![pb::UploadAttachment {
                    conversation_id: x.conversation_id,
                    name: x.name,
                    mime: x.mime,
                    source: Some(pb::upload_attachment::Source::UploadId(upload_id)),
                }],
            })
        }
    }
}

/// 粘贴的数据切成不超过 1MiB 的分片；最后一片 `last = true`（空数据也发一片）。
pub fn chunks(upload_id: &str, data: &Bytes) -> Vec<pb::Chunk> {
    let max = MAX_CHUNK_DATA;
    let n = data.len().div_ceil(max).max(1);
    (0..n)
        .map(|i| {
            let start = i * max;
            let end = (start + max).min(data.len());
            pb::Chunk {
                upload_id: upload_id.to_owned(),
                offset: start as u64,
                data: data.slice(start..end),
                last: i + 1 == n,
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn attachment_over_20mb_rejected() {
        assert!(
            check_size("a.png", ATTACHMENT_MAX_BYTES).is_ok(),
            "对照：正好 20MB 可以"
        );
        let e = check_size("a.png", ATTACHMENT_MAX_BYTES + 1).unwrap_err();
        assert_eq!(e.code, "attachment_too_large");
        assert!(e.message.contains("a.png"));
    }

    #[test]
    fn oversized_path_rejects_whole_batch() {
        let dir = tempfile::tempdir().unwrap();
        let small = dir.path().join("small.txt");
        let big = dir.path().join("big.bin");
        std::fs::write(&small, b"hi").unwrap();
        let f = std::fs::File::create(&big).unwrap();
        f.set_len(ATTACHMENT_MAX_BYTES + 1).unwrap();
        let paths = [
            small.to_string_lossy().into_owned(),
            big.to_string_lossy().into_owned(),
        ];
        let e = path_commands("c1", &paths).unwrap_err();
        assert!(e.message.contains("big.bin"));
        let ok = path_commands("c1", &paths[..1]).unwrap();
        assert_eq!(ok[0].name, "small.txt");
    }

    #[test]
    fn chunks_cover_data_and_mark_last() {
        let data = Bytes::from(vec![7u8; MAX_CHUNK_DATA * 2 + 5]);
        let c = chunks("u", &data);
        assert_eq!(c.len(), 3);
        assert_eq!(c.iter().map(|x| x.data.len()).sum::<usize>(), data.len());
        assert!(c[2].last && !c[0].last);
        assert_eq!(c[1].offset, MAX_CHUNK_DATA as u64);
        assert_eq!(chunks("u", &Bytes::new()).len(), 1);
    }
}
