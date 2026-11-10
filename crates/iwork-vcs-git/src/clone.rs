//! `clone`：`git clone --progress`，进度推给接收方（技术设计 8.1、9.3）。
//!
//! 不接触凭据：认证全由用户自己的 git 凭据助手完成，这里不读、不存、不传任何凭据。

use std::collections::VecDeque;
use std::path::Path;
use std::process::Stdio;

use iwork_ports::vcs::{ProgressSink, VcsError};
use tokio::io::AsyncReadExt;

use crate::command::GitCommand;
use crate::progress::{ProgressParser, is_progress_line};

/// 失败时保留的错误输出行数（只留非进度行）。
const ERROR_TAIL_LINES: usize = 20;

/// 把 `url` 克隆到 `dest`。
///
/// - `dest` 已存在且非空时返回 [`VcsError::TargetNotEmpty`]，不动它；父目录不存在时先建好。
/// - 失败返回 [`VcsError::CloneFailed`]（可重试），带 git 的错误输出，地址里的用户信息已遮掉。
///   git 失败时会删掉自己建的目录，所以同一个 `dest` 可以直接重试。
pub(crate) async fn clone(
    git: &GitCommand,
    url: &str,
    dest: &Path,
    sink: &dyn ProgressSink,
) -> Result<(), VcsError> {
    if is_non_empty(dest)? {
        return Err(VcsError::TargetNotEmpty {
            path: dest.to_owned(),
        });
    }
    let parent = dest
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .ok_or_else(|| clone_failed(format!("目标路径 {} 没有父目录", dest.display())))?;
    std::fs::create_dir_all(parent)
        .map_err(|e| clone_failed(format!("创建 {} 失败：{e}", parent.display())))?;

    let mut child = git
        .command(parent)
        // 没有终端可问：不让 git 卡在终端提示上，凭据助手照常生效
        .env("GIT_TERMINAL_PROMPT", "0")
        // `--`：地址以 `-` 开头时也只当地址，不会被当成选项（如 `--upload-pack=…`）
        .args(["clone", "--progress", "--"])
        .arg(url)
        .arg(dest)
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| clone_failed(format!("启动 git 失败：{e}")))?;
    let mut stderr = child
        .stderr
        .take()
        .ok_or_else(|| clone_failed("拿不到 git 的错误输出".to_owned()))?;

    let mut parser = ProgressParser::default();
    let mut lines = LineSplitter::default();
    let mut tail: VecDeque<String> = VecDeque::with_capacity(ERROR_TAIL_LINES);
    let mut buf = [0u8; 4096];
    loop {
        let n = stderr
            .read(&mut buf)
            .await
            .map_err(|e| clone_failed(format!("读取 git 输出失败：{e}")))?;
        let eof = n == 0;
        for line in lines.push(&buf[..n], eof) {
            if let Some(p) = parser.feed(&line) {
                sink.progress(p);
            } else if !is_progress_line(&line) && !line.trim().is_empty() {
                if tail.len() == ERROR_TAIL_LINES {
                    tail.pop_front();
                }
                tail.push_back(line);
            }
        }
        if eof {
            break;
        }
    }
    let status = child
        .wait()
        .await
        .map_err(|e| clone_failed(format!("等待 git 结束失败：{e}")))?;
    if !status.success() {
        let output = Vec::from(tail).join("\n");
        return Err(clone_failed(redact(&output, url)));
    }
    if let Some(p) = parser.finish() {
        sink.progress(p);
    }
    Ok(())
}

fn is_non_empty(dest: &Path) -> Result<bool, VcsError> {
    match std::fs::read_dir(dest) {
        Ok(mut entries) => Ok(entries.next().is_some()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(false),
        // 是文件或读不了：都不能往里克隆
        Err(_) => Ok(dest.exists()),
    }
}

fn clone_failed(message: String) -> VcsError {
    VcsError::CloneFailed { message }
}

/// 遮掉错误输出里的地址：原样出现的整个地址换成 `<url>`，其他 `scheme://用户信息@` 里的用户信息
/// 换成 `***`（地址可能带 token，见端口 `VcsError` 的约定）。
pub(crate) fn redact(output: &str, url: &str) -> String {
    let replaced = if url.is_empty() {
        output.to_owned()
    } else {
        output.replace(url, "<url>")
    };
    let mut out = String::with_capacity(replaced.len());
    let mut rest = replaced.as_str();
    while let Some(i) = rest.find("://") {
        let (head, tail) = rest.split_at(i + 3);
        out.push_str(head);
        let authority_end = tail
            .find(|c: char| c == '/' || c.is_whitespace() || c == '\'' || c == '"')
            .unwrap_or(tail.len());
        match tail[..authority_end].rfind('@') {
            Some(at) => {
                out.push_str("***");
                rest = &tail[at..];
            }
            None => rest = tail,
        }
    }
    out.push_str(rest);
    out
}

/// 把字节流按 `\r` / `\n` 切成行（git 的进度用 `\r` 原地刷新）。
#[derive(Debug, Default)]
struct LineSplitter {
    pending: Vec<u8>,
}

impl LineSplitter {
    fn push(&mut self, bytes: &[u8], eof: bool) -> Vec<String> {
        let mut lines = Vec::new();
        for &b in bytes {
            if b == b'\r' || b == b'\n' {
                if !self.pending.is_empty() {
                    lines.push(String::from_utf8_lossy(&self.pending).into_owned());
                    self.pending.clear();
                }
            } else {
                self.pending.push(b);
            }
        }
        if eof && !self.pending.is_empty() {
            lines.push(String::from_utf8_lossy(&self.pending).into_owned());
            self.pending.clear();
        }
        lines
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn redact_hides_whole_url_and_any_userinfo() {
        let url = "https://alice:tok3n@example.com/r.git";
        let out = redact(
            "fatal: unable to access 'https://alice:tok3n@example.com/r.git/': 403\n\
             fatal: also https://bob:secret@other.example/x and ssh://git@host/y",
            url,
        );
        assert!(!out.contains("tok3n"), "{out}");
        assert!(!out.contains("secret"), "{out}");
        assert!(!out.contains("bob"), "{out}");
        assert!(out.contains("<url>"), "{out}");
        assert!(out.contains("https://***@other.example/x"), "{out}");
        assert!(out.contains("ssh://***@host/y"), "{out}");
        // 对照：不带用户信息的地址与普通文字原样保留
        assert_eq!(
            redact(
                "see https://example.com/a for help",
                "https://example.com/other.git"
            ),
            "see https://example.com/a for help"
        );
    }

    #[test]
    fn splitter_handles_carriage_returns_and_chunk_boundaries() {
        let mut s = LineSplitter::default();
        assert_eq!(
            s.push(b"Receiving: 1%\rRecei", false),
            vec!["Receiving: 1%"]
        );
        assert_eq!(s.push(b"ving: 2%\r\nfatal", false), vec!["Receiving: 2%"]);
        assert_eq!(s.push(b"", true), vec!["fatal"]);
    }
}
