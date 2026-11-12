//! 起 CLI、写 stdin、按行读 stdout（T06 方案 2.2 `cli/process.rs`）。薄壳：怎么起由 [`LaunchPlan`] 定，
//! 这里只照做。
//!
//! - 环境：先清空，再原样设上 `LaunchPlan.env`（3.1a：桥接的环境加五个变量，P14 不筛选）。不从桥接自己的环境里
//!   再继承任何东西，CLI 看到的就是计划里那一份。
//! - stdout：按行读，一行最长 [`MAX_LINE_BYTES`]（events.rs 的同一个上限）。超了就停在上限附近报错，不把整行
//!   读进内存；之后这条输出流不再可信，调用方按致命处理。
//! - stdin：开会话的握手阶段直接写（[`CliProcess::write_line`]，一问一答，行很短）；会话开起来之后交给
//!   [`InputQueue`]：一个单独的任务往 stdin 写，会话循环只往队列里放。CLI 正在往 stdout 写一大行、不读 stdin 时，
//!   会话循环照样读它的 stdout、收 hook 事件，双方不会互等（审查 S6-3 1/F-1）。
//! - stderr：不进日志（里面可能有用户的内容），只留最后 [`STDERR_TAIL_BYTES`] 字节，回合结局判不出来时给诊断用。
//! - 结束进程树的做法归 S6-5（方案 5.4a）；这里只提供「杀掉 CLI 自己并等它退出」，开会话失败时用（那时还没有
//!   任何工具调用，没有它起的命令要清）。

use std::collections::VecDeque;
use std::io;
use std::process::{ExitStatus, Stdio};
use std::sync::{Arc, Mutex};

use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, ChildStdin, ChildStdout, Command};
use tokio::sync::{mpsc, watch};

use crate::cli::events::{LineError, MAX_LINE_BYTES};
use crate::launch::LaunchPlan;

/// 读一行出错。
#[derive(Debug)]
pub enum ReadError {
    Io(io::Error),
    /// 超长、不是 UTF-8：输出不合协议
    Line(LineError),
}

/// stderr 留多少字节（最后的部分）。
pub const STDERR_TAIL_BYTES: usize = 8 * 1024;

/// 一个跑着的 CLI。
pub struct CliProcess {
    child: Child,
    /// 交给 [`InputQueue`] 之后为 `None`
    stdin: Option<ChildStdin>,
    stdout: BufReader<ChildStdout>,
    /// 还没读到换行的半行（放在这里而不是局部变量：`next_line` 被取消时不丢已读的字节）
    partial: Vec<u8>,
    stderr_tail: Arc<Mutex<VecDeque<u8>>>,
}

impl CliProcess {
    /// 照计划起 CLI。要在 tokio 运行时里调用。
    pub fn spawn(plan: &LaunchPlan) -> io::Result<Self> {
        let mut child = Command::new(&plan.program)
            .args(&plan.args)
            .current_dir(&plan.cwd)
            .env_clear()
            .envs(plan.env.vars().iter().map(|(k, v)| (k, v)))
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            // 桥接这边丢掉它（出错提前返回）时不留下跑着的 CLI
            .kill_on_drop(true)
            .spawn()?;
        let stdin = child.stdin.take().expect("stdin 是 piped");
        let stdout = BufReader::new(child.stdout.take().expect("stdout 是 piped"));
        let mut stderr = child.stderr.take().expect("stderr 是 piped");
        let stderr_tail = Arc::new(Mutex::new(VecDeque::with_capacity(STDERR_TAIL_BYTES)));
        let tail = Arc::clone(&stderr_tail);
        // 一直读到 CLI 关掉 stderr，不读满管道 CLI 会卡住
        tokio::spawn(async move {
            let mut buf = [0u8; 4096];
            while let Ok(n) = stderr.read(&mut buf).await {
                if n == 0 {
                    break;
                }
                keep_tail(&mut tail.lock().expect("锁没有中毒"), &buf[..n]);
            }
        });
        Ok(Self {
            child,
            stdin: Some(stdin),
            stdout,
            partial: Vec::new(),
            stderr_tail,
        })
    }

    /// CLI 的进程号（已经被等到退出的返回 `None`）。
    pub fn id(&self) -> Option<u32> {
        self.child.id()
    }

    /// 写一行（自动加换行符）并刷出。只在开会话的握手阶段用；之后用 [`Self::input_queue`]。
    pub async fn write_line(&mut self, line: &str) -> io::Result<()> {
        let stdin = self
            .stdin
            .as_mut()
            .ok_or_else(|| io::Error::other("stdin 已经交给输入队列"))?;
        stdin.write_all(line.as_bytes()).await?;
        stdin.write_all(b"\n").await?;
        stdin.flush().await
    }

    /// 把 stdin 交给一个单独的写任务，返回往里放行的队列。只能调一次。
    pub fn input_queue(&mut self) -> InputQueue {
        let mut stdin = self.stdin.take().expect("输入队列只建一次");
        let (tx, mut rx) = mpsc::channel::<Vec<u8>>(INPUT_QUEUE_LINES);
        let (failed_tx, failed) = watch::channel(false);
        tokio::spawn(async move {
            while let Some(mut bytes) = rx.recv().await {
                bytes.push(b'\n');
                if stdin.write_all(&bytes).await.is_err() || stdin.flush().await.is_err() {
                    let _ = failed_tx.send(true);
                    return;
                }
            }
            // 队列关了（会话结束）：丢掉 stdin，CLI 读到输入结束
        });
        InputQueue { tx, failed }
    }

    /// 读下一行（不含换行符）；CLI 关了 stdout 返回 `None`（最后没换行的半行照样当一行返回）。
    /// 取消安全（可以放进 `select!`）：已经读进来的字节留在 `partial` 里。
    pub async fn next_line(&mut self) -> Result<Option<String>, ReadError> {
        loop {
            let buf = self.stdout.fill_buf().await.map_err(ReadError::Io)?;
            if buf.is_empty() {
                if self.partial.is_empty() {
                    return Ok(None);
                }
                return finish(std::mem::take(&mut self.partial)).map(Some);
            }
            let (take, newline) = match buf.iter().position(|b| *b == b'\n') {
                Some(i) => (i, true),
                None => (buf.len(), false),
            };
            // 先看长度再拷：超长的那一段不进内存
            let len = self.partial.len() + take;
            if len > MAX_LINE_BYTES {
                return Err(ReadError::Line(LineError::TooLong { bytes: len }));
            }
            self.partial.extend_from_slice(&buf[..take]);
            self.stdout.consume(take + usize::from(newline));
            if newline {
                return finish(std::mem::take(&mut self.partial)).map(Some);
            }
        }
    }

    /// 等 CLI 退出。取消安全。
    pub async fn wait(&mut self) -> io::Result<ExitStatus> {
        self.child.wait().await
    }

    /// 杀掉 CLI 并等它退出。只管 CLI 自己（见模块说明）。
    pub async fn kill(&mut self) -> io::Result<()> {
        self.child.kill().await
    }

    /// 请 CLI 自己收拾、退出（SIGTERM，k3：它会结束自己的前台命令），等 `limit`；没退就杀掉。只管 CLI 自己。
    #[cfg(unix)]
    pub async fn terminate(&mut self, limit: std::time::Duration) {
        if let Some(pid) = self.child.id().and_then(|p| libc::pid_t::try_from(p).ok()) {
            // SAFETY: kill 只读两个整数参数；进程还没被等到，进程号不会是别人的
            unsafe { libc::kill(pid, libc::SIGTERM) };
            if tokio::time::timeout(limit, self.child.wait()).await.is_ok() {
                return;
            }
        }
        let _ = self.child.kill().await;
    }

    /// stderr 的最后一段（按 UTF-8 宽松解码）。只给诊断用，不进日志。
    pub fn stderr_tail(&self) -> String {
        let tail = self.stderr_tail.lock().expect("锁没有中毒");
        String::from_utf8_lossy(&tail.iter().copied().collect::<Vec<_>>()).into_owned()
    }
}

/// 输入队列里最多排几行。正常时只有一条用户消息加一条控制请求；排满说明 CLI 一直不读 stdin。
pub const INPUT_QUEUE_LINES: usize = 8;

/// 一行输入的上限（字节，不含换行符）：和读 CLI 输出用同一个数。
pub const MAX_INPUT_LINE_BYTES: usize = MAX_LINE_BYTES;

/// 往 CLI 的 stdin 放一行没放成。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InputError {
    /// 这一行超过 [`MAX_INPUT_LINE_BYTES`]：不写，调用方回 `setup_invalid`
    TooLarge { bytes: usize },
    /// 队列排满了：CLI 一直不读 stdin
    Backlogged,
    /// 写任务已经停了（写出错，CLI 多半没了）
    Closed,
}

/// 给 CLI 的输入队列（见模块说明）。丢掉它就关掉 CLI 的 stdin。
pub struct InputQueue {
    tx: mpsc::Sender<Vec<u8>>,
    failed: watch::Receiver<bool>,
}

impl InputQueue {
    /// 放一行（不含换行符），不等它写完。
    pub fn send(&self, line: &str) -> Result<(), InputError> {
        if line.len() > MAX_INPUT_LINE_BYTES {
            return Err(InputError::TooLarge { bytes: line.len() });
        }
        if *self.failed.borrow() {
            return Err(InputError::Closed);
        }
        self.tx
            .try_send(line.as_bytes().to_vec())
            .map_err(|e| match e {
                mpsc::error::TrySendError::Full(_) => InputError::Backlogged,
                mpsc::error::TrySendError::Closed(_) => InputError::Closed,
            })
    }

    /// 写任务写出错时返回（之后 CLI 收不到输入了）；一直没出错就一直等。取消安全。
    pub async fn failed(&mut self) {
        if self.failed.wait_for(|f| *f).await.is_err() {
            std::future::pending::<()>().await;
        }
    }
}

/// 一行的字节 → 文字：去掉行尾的 `\r`，不是 UTF-8 的按坏行处理。
fn finish(mut line: Vec<u8>) -> Result<String, ReadError> {
    if line.last() == Some(&b'\r') {
        line.pop();
    }
    String::from_utf8(line).map_err(|_| ReadError::Line(LineError::NotJson))
}

/// 追加到尾部缓冲，超过 [`STDERR_TAIL_BYTES`] 就从前面丢。
fn keep_tail(tail: &mut VecDeque<u8>, bytes: &[u8]) {
    tail.extend(bytes);
    let over = tail.len().saturating_sub(STDERR_TAIL_BYTES);
    tail.drain(..over);
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 超长的一行不放（调用方回 `setup_invalid`）；队列排满了报 `Backlogged`，不在这里等。
    #[test]
    fn input_queue_limits() {
        let (tx, _rx) = mpsc::channel(1);
        let (_failed_tx, failed) = watch::channel(false);
        let q = InputQueue { tx, failed };
        let big = "x".repeat(MAX_INPUT_LINE_BYTES + 1);
        assert_eq!(
            q.send(&big),
            Err(InputError::TooLarge {
                bytes: MAX_INPUT_LINE_BYTES + 1
            })
        );
        assert_eq!(q.send("first"), Ok(()));
        assert_eq!(q.send("second"), Err(InputError::Backlogged));
    }

    /// 写任务出过错：之后放不进去。
    #[test]
    fn input_queue_after_failure_is_closed() {
        let (tx, _rx) = mpsc::channel(8);
        let (failed_tx, failed) = watch::channel(false);
        let q = InputQueue { tx, failed };
        failed_tx.send(true).unwrap();
        assert_eq!(q.send("x"), Err(InputError::Closed));
    }

    #[test]
    fn line_bytes_to_text() {
        assert_eq!(finish(b"{}\r".to_vec()).unwrap(), "{}");
        assert_eq!(finish(b"{}".to_vec()).unwrap(), "{}");
        assert!(matches!(
            finish(vec![0xff, 0xfe]),
            Err(ReadError::Line(LineError::NotJson))
        ));
    }

    #[test]
    fn tail_keeps_only_the_last_bytes() {
        let mut t = VecDeque::new();
        keep_tail(&mut t, b"abc");
        assert_eq!(t.iter().copied().collect::<Vec<_>>(), b"abc");
        keep_tail(&mut t, &vec![b'x'; STDERR_TAIL_BYTES]);
        assert_eq!(t.len(), STDERR_TAIL_BYTES);
        assert!(t.iter().all(|b| *b == b'x'), "前面的 abc 被挤掉了");
        keep_tail(&mut t, b"end");
        assert_eq!(t.len(), STDERR_TAIL_BYTES);
        assert_eq!(t.iter().rev().take(3).copied().collect::<Vec<_>>(), b"dne");
    }
}
