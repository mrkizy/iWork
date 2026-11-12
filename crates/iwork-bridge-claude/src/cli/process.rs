//! 起 CLI、写 stdin、按行读 stdout（T06 方案 2.2 `cli/process.rs`）。薄壳：怎么起由 [`LaunchPlan`] 定，
//! 这里只照做。
//!
//! - 环境：先清空，再原样设上 `LaunchPlan.env`（3.1a：桥接的环境加五个变量，P14 不筛选）。不从桥接自己的环境里
//!   再继承任何东西，CLI 看到的就是计划里那一份。
//! - stderr：不进日志（里面可能有用户的内容），只留最后 [`STDERR_TAIL_BYTES`] 字节，回合结局判不出来时给诊断用。
//! - 结束进程树的做法归 S6-5（方案 5.4a）；这里只提供「杀掉 CLI 自己并等它退出」，开会话失败时用（那时还没有
//!   任何工具调用，没有它起的命令要清）。

use std::collections::VecDeque;
use std::io;
use std::process::{ExitStatus, Stdio};
use std::sync::{Arc, Mutex};

use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader, Lines};
use tokio::process::{Child, ChildStdin, ChildStdout, Command};

use crate::launch::LaunchPlan;

/// stderr 留多少字节（最后的部分）。
pub const STDERR_TAIL_BYTES: usize = 8 * 1024;

/// 一个跑着的 CLI。
pub struct CliProcess {
    child: Child,
    stdin: ChildStdin,
    stdout: Lines<BufReader<ChildStdout>>,
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
        let stdout = BufReader::new(child.stdout.take().expect("stdout 是 piped")).lines();
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
            stdin,
            stdout,
            stderr_tail,
        })
    }

    /// CLI 的进程号（已经被等到退出的返回 `None`）。
    pub fn id(&self) -> Option<u32> {
        self.child.id()
    }

    /// 写一行（自动加换行符）并刷出。
    pub async fn write_line(&mut self, line: &str) -> io::Result<()> {
        self.stdin.write_all(line.as_bytes()).await?;
        self.stdin.write_all(b"\n").await?;
        self.stdin.flush().await
    }

    /// 读下一行；CLI 关了 stdout 返回 `None`。取消安全（可以放进 `select!`）。
    pub async fn next_line(&mut self) -> io::Result<Option<String>> {
        self.stdout.next_line().await
    }

    /// 等 CLI 退出。取消安全。
    pub async fn wait(&mut self) -> io::Result<ExitStatus> {
        self.child.wait().await
    }

    /// 杀掉 CLI 并等它退出。只管 CLI 自己（见模块说明）。
    pub async fn kill(&mut self) -> io::Result<()> {
        self.child.kill().await
    }

    /// stderr 的最后一段（按 UTF-8 宽松解码）。只给诊断用，不进日志。
    pub fn stderr_tail(&self) -> String {
        let tail = self.stderr_tail.lock().expect("锁没有中毒");
        String::from_utf8_lossy(&tail.iter().copied().collect::<Vec<_>>()).into_owned()
    }
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
