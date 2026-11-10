//! 按行读运行时的输出（ACP 一行一条 JSON-RPC 消息），单行设长度上限。

use tokio::io::{AsyncBufRead, AsyncBufReadExt};

/// 读一行（不含换行符）。
///
/// - 返回 `Ok(None)`：流已结束且没有剩余字节；
/// - 流结束时最后一行没有换行符，照常返回这一行；
/// - 单行超过 `limit` 字节：`InvalidData`，超出的部分不读进内存；
/// - 不是 UTF-8：`InvalidData`。
pub(crate) async fn read_frame<R: AsyncBufRead + Unpin>(
    reader: &mut R,
    limit: usize,
) -> std::io::Result<Option<String>> {
    let mut line = Vec::new();
    loop {
        let chunk = reader.fill_buf().await?;
        if chunk.is_empty() {
            return if line.is_empty() {
                Ok(None)
            } else {
                into_text(line).map(Some)
            };
        }
        let (part, newline) = match chunk.iter().position(|b| *b == b'\n') {
            Some(i) => (&chunk[..i], true),
            None => (chunk, false),
        };
        if line.len() + part.len() > limit {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                format!("运行时发来的单条消息超过 {limit} 字节"),
            ));
        }
        line.extend_from_slice(part);
        let used = part.len() + usize::from(newline);
        reader.consume(used);
        if newline {
            return into_text(line).map(Some);
        }
    }
}

fn into_text(line: Vec<u8>) -> std::io::Result<String> {
    String::from_utf8(line).map_err(|_| {
        std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "运行时发来的消息不是 UTF-8",
        )
    })
}

#[cfg(test)]
mod tests {
    use tokio::io::BufReader;

    use super::*;

    /// 用很小的缓冲区读，让一行跨好几次 fill_buf。
    async fn frames(
        input: &[u8],
        limit: usize,
        buffer: usize,
    ) -> Vec<std::io::Result<Option<String>>> {
        let mut reader = BufReader::with_capacity(buffer, input);
        let mut out = Vec::new();
        loop {
            let frame = read_frame(&mut reader, limit).await;
            let stop = !matches!(frame, Ok(Some(_)));
            out.push(frame);
            if stop {
                return out;
            }
        }
    }

    fn texts(out: &[std::io::Result<Option<String>>]) -> Vec<Option<String>> {
        out.iter().map(|r| r.as_ref().unwrap().clone()).collect()
    }

    #[tokio::test]
    async fn lines_within_limit_across_small_buffers() {
        let out = frames(b"{\"a\":1}\n\xe4\xbd\xa0\xe5\xa5\xbd\n", 64, 3).await;
        assert_eq!(
            texts(&out),
            [Some("{\"a\":1}".into()), Some("你好".into()), None]
        );
    }

    #[tokio::test]
    async fn exactly_the_limit_is_accepted_one_more_is_rejected() {
        let ok = frames(b"12345\n", 5, 2).await;
        assert_eq!(texts(&ok), [Some("12345".into()), None]);
        let too_long = frames(b"123456\n", 5, 2).await;
        let err = too_long[0].as_ref().unwrap_err();
        assert_eq!(err.kind(), std::io::ErrorKind::InvalidData);
        assert!(err.to_string().contains("超过 5 字节"), "{err}");
    }

    #[tokio::test]
    async fn oversize_without_newline_is_rejected_before_reading_it_all() {
        let input = vec![b'x'; 1 << 20];
        let mut reader = BufReader::with_capacity(4096, input.as_slice());
        let err = read_frame(&mut reader, 10_000).await.unwrap_err();
        assert!(err.to_string().contains("超过 10000 字节"));
        // 只读到超限为止：源里还剩下绝大部分没读
        let unread = reader.get_ref().len();
        assert!(
            unread >= (1 << 20) - 10_000 - 4096,
            "只剩 {unread} 字节没读"
        );
    }

    #[tokio::test]
    async fn last_line_without_newline_is_returned() {
        let out = frames(b"a\nlast", 64, 2).await;
        assert_eq!(texts(&out), [Some("a".into()), Some("last".into()), None]);
    }

    #[tokio::test]
    async fn invalid_utf8_is_rejected_with_or_without_newline() {
        for input in [&b"\xff\xfe\n"[..], &b"ok\xff"[..]] {
            let out = frames(input, 64, 2).await;
            let err = out[0].as_ref().unwrap_err();
            assert_eq!(err.kind(), std::io::ErrorKind::InvalidData);
            assert!(err.to_string().contains("不是 UTF-8"), "{err}");
        }
    }
}
