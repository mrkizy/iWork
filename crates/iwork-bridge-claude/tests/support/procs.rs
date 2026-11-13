//! 屏障类测试共用的进程工具：在原始 ACP 行上说话的 [`Wire`]、经中间 shell 起桥接（后台可以「死掉」）的
//! [`Orphanable`]、看进程状态与收尾的小函数。

use std::path::Path;
use std::time::{Duration, Instant};

use iwork_agent_acp::process::SupervisedProcess;
use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt, AsyncRead, AsyncWrite, AsyncWriteExt, BufReader, Lines};
use tokio::net::unix::pipe;

use super::acp::{BRIDGE, Rig, emit, init, opening, read_user};

// ---------- 原始 ACP 行 ----------

/// 一条 ACP 连接的两头：写请求、按 id 读响应。
pub struct Wire<W, R> {
    pub write: Option<W>,
    pub read: Lines<BufReader<R>>,
}

impl<W: AsyncWrite + Unpin, R: AsyncRead + Unpin> Wire<W, R> {
    pub fn new(write: W, read: R) -> Self {
        Self {
            write: Some(write),
            read: BufReader::new(read).lines(),
        }
    }

    pub async fn send(&mut self, v: Value) {
        let w = self.write.as_mut().expect("还没关");
        w.write_all(format!("{v}\n").as_bytes()).await.unwrap();
        w.flush().await.unwrap();
    }

    pub async fn response(&mut self, id: u64) -> Value {
        let deadline = Duration::from_secs(20);
        tokio::time::timeout(deadline, async {
            loop {
                let line = self
                    .read
                    .next_line()
                    .await
                    .unwrap()
                    .expect("桥接提前关了 stdout");
                let v: Value = serde_json::from_str(&line).unwrap();
                if v["id"] == id {
                    return v;
                }
            }
        })
        .await
        .expect("20 秒内有响应")
    }

    /// 握手、开会话（id 1、2），返回会话 id。
    pub async fn open(&mut self, rig: &Rig) -> Value {
        self.send(json!({ "jsonrpc": "2.0", "id": 1, "method": "initialize",
            "params": { "protocolVersion": 1, "clientCapabilities": {} } }))
            .await;
        assert!(self.response(1).await["result"].is_object());
        let meta = iwork_agent_acp::convert::session_setup_meta(&rig.spec()).unwrap();
        self.send(json!({ "jsonrpc": "2.0", "id": 2, "method": "session/new",
            "params": { "cwd": rig.work.path(), "mcpServers": [], "_meta": meta } }))
            .await;
        let opened = self.response(2).await;
        let session = opened["result"]["sessionId"].clone();
        assert!(session.is_string(), "{opened}");
        session
    }

    /// 握手、开会话，然后发一个 prompt（id 3），返回它的响应。
    pub async fn open_and_prompt(&mut self, rig: &Rig) -> Value {
        let session = self.open(rig).await;
        self.prompt(3, &session).await
    }

    pub async fn prompt(&mut self, id: u64, session: &Value) -> Value {
        self.send(
            json!({ "jsonrpc": "2.0", "id": id, "method": "session/prompt",
            "params": { "sessionId": session, "prompt": [{ "type": "text", "text": "hi" }] } }),
        )
        .await;
        self.response(id).await
    }

    /// 关掉写的那一头：桥接读到 stdin 结束（后台没了）。
    pub fn hang_up(&mut self) {
        drop(self.write.take());
    }
}

/// 这一轮因为后台任务事件按致命错误收掉，错误里带「会话已结束」。
pub fn assert_fatal(response: &Value) {
    assert_eq!(
        response["error"]["data"]["iwork"],
        json!({ "code": "cli_error", "reason": "background_task", "session_ended": true }),
        "{response}"
    );
}

// ---------- 假 CLI 与进程 ----------

/// 一轮开头（`init` 之后先做 `before`）就报后台任务（致命），等 300 毫秒（桥接收摊）后再起一次工具调用（看 hook
/// 是不是被拒），然后记信号。
pub fn fatal_script(marker: &Path, ignore_hup: bool, before: Vec<Value>) -> Value {
    let mut steps = opening();
    steps.extend([read_user(), init()]);
    steps.extend(before);
    steps.extend([
        emit(json!({ "type": "system", "subtype": "task_started", "task_id": "a", "is_backgrounded": true })),
        json!({ "step": "sleep_ms", "ms": 300 }),
        json!({ "step": "tool", "id": "toolu_late", "name": "Bash",
            "input": { "command": "touch", "marker": marker.display().to_string() } }),
    ]);
    steps.push(json!({ "step": "log_signals_until_term", "ignore_hup": ignore_hup }));
    Value::Array(steps)
}

/// 等假 CLI 的记录里出现 `kind` 这一笔。
pub async fn logged(rig: &Rig, kind: &str) -> Value {
    for _ in 0..2000 {
        if let Some(v) = rig.log().into_iter().find(|v| v["kind"] == kind) {
            return v;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    panic!("20 秒内假 CLI 没记 {kind}：{:?}", rig.log());
}

pub fn signals(rig: &Rig) -> Vec<String> {
    rig.log()
        .iter()
        .filter(|v| v["kind"] == "signal")
        .map(|v| v["signal"].as_str().unwrap().to_owned())
        .collect()
}

pub fn pid_of(v: &Value) -> i32 {
    v["pid"].as_i64().unwrap() as i32
}

/// 进程的状态：`'T'` 停着、`'Z'` 僵尸、`'R'` 别的活着的状态；没了为 `None`。
#[cfg(target_os = "macos")]
pub fn state(pid: i32) -> Option<char> {
    let size = std::mem::size_of::<libc::proc_bsdinfo>();
    // SAFETY: 全零是这个纯数据结构体的合法初值；指针与长度描述同一个结构体
    let info = unsafe {
        let mut info: libc::proc_bsdinfo = std::mem::zeroed();
        let n = libc::proc_pidinfo(
            pid,
            libc::PROC_PIDTBSDINFO,
            0,
            (&raw mut info).cast(),
            size as i32,
        );
        if n as usize != size {
            // 僵尸在 macOS 上读不到：看进程号还占不占着
            // SAFETY: 信号 0 只检查进程号
            return (libc::kill(pid, 0) == 0).then_some('Z');
        }
        info
    };
    Some(match info.pbi_status {
        libc::SSTOP => 'T',
        libc::SZOMB => 'Z',
        _ => 'R',
    })
}

#[cfg(target_os = "linux")]
pub fn state(pid: i32) -> Option<char> {
    let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    let c = stat[stat.rfind(')')? + 1..]
        .split_whitespace()
        .next()?
        .chars()
        .next()?;
    Some(match c {
        'T' | 't' => 'T',
        'Z' | 'X' => 'Z',
        _ => 'R',
    })
}

/// 父进程号；没了为 `None`。
#[cfg(target_os = "macos")]
pub fn ppid(pid: i32) -> Option<i32> {
    let size = std::mem::size_of::<libc::proc_bsdinfo>();
    // SAFETY: 全零是这个纯数据结构体的合法初值；指针与长度描述同一个结构体
    unsafe {
        let mut info: libc::proc_bsdinfo = std::mem::zeroed();
        let n = libc::proc_pidinfo(
            pid,
            libc::PROC_PIDTBSDINFO,
            0,
            (&raw mut info).cast(),
            size as i32,
        );
        (n as usize == size).then_some(info.pbi_ppid as i32)
    }
}

#[cfg(target_os = "linux")]
pub fn ppid(pid: i32) -> Option<i32> {
    let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    stat[stat.rfind(')')? + 1..]
        .split_whitespace()
        .nth(1)?
        .parse()
        .ok()
}

pub fn alive(pid: i32) -> bool {
    matches!(state(pid), Some('R' | 'T'))
}

pub async fn wait_until(what: &str, limit: Duration, mut done: impl FnMut() -> bool) {
    let started = Instant::now();
    while !done() {
        assert!(started.elapsed() < limit, "{limit:?} 内没等到：{what}");
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
}

pub fn signal(pid: i32, sig: libc::c_int) {
    // SAFETY: 只发信号；进程号是本测试起的进程
    unsafe { libc::kill(pid, sig) };
}

/// 本测试起的进程：结束时亲手收掉（停着的先放开，再杀）。
pub struct Leftover(pub Vec<i32>);

impl Drop for Leftover {
    fn drop(&mut self) {
        for pid in &self.0 {
            if alive(*pid) {
                signal(*pid, libc::SIGKILL);
            }
        }
    }
}

/// 后台以 `SupervisedProcess` 起桥接（和产品一样），在它的管道上说原始 ACP。stderr 读掉不管。
pub fn spawn_supervised(
    rig: &Rig,
) -> (
    SupervisedProcess,
    Wire<tokio::process::ChildStdin, tokio::process::ChildStdout>,
) {
    let (process, pipes) = SupervisedProcess::spawn(&rig.launch).unwrap();
    let mut stderr = pipes.stderr;
    tokio::spawn(async move {
        let _ = tokio::io::copy(&mut stderr, &mut tokio::io::sink()).await;
    });
    (process, Wire::new(pipes.stdin, pipes.stdout))
}

/// 桥接经一个中间 shell 起：后台（中间 shell）死掉时，桥接被系统收养，它的组成了孤儿组。管道是两个 FIFO，
/// 测试拿着另一头。
pub struct Orphanable {
    pub dir: tempfile::TempDir,
    pub shell: std::process::Child,
    pub wire: Wire<pipe::Sender, pipe::Receiver>,
    pub bridge: i32,
    /// [`Launch::StderrFifo`] 时 stderr FIFO 的读端：拿着、不读
    pub err_read: Option<std::fs::File>,
}

/// 模拟后台的 shell 归夹具收拾：不论测试成功结束还是断言失败中途 panic，丢掉夹具时都结束并回收它（审查 S6-5b
/// 3/F-2）。桥接这时看到父进程换了，自己收尾。
impl Drop for Orphanable {
    fn drop(&mut self) {
        if matches!(self.shell.try_wait(), Ok(None)) {
            let _ = self.shell.kill();
        }
        let _ = self.shell.wait();
    }
}

/// [`Orphanable`] 怎么起桥接。
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Launch {
    /// 中间 shell 起桥接并告诉它自己是后台（`IWORK_SUPERVISOR_PID=$$`），stderr 写进文件
    Normal,
    /// 同上，但 stderr 是一个测试拿着读端、从来不读的 FIFO（[`Orphanable::fill_stderr`] 把它填满）
    StderrFifo,
    /// 中间 shell 先退出，桥接这才 exec 起来：进 main 时父进程已经不是告诉它的那个后台了
    AfterSupervisorDied,
}

impl Orphanable {
    pub async fn start(rig: &Rig) -> Self {
        Self::start_with(rig, Launch::Normal).await
    }

    pub async fn start_with(rig: &Rig, launch: Launch) -> Self {
        let dir = tempfile::tempdir().unwrap();
        let fifos: &[&str] = match launch {
            Launch::StderrFifo => &["in", "out", "err"],
            _ => &["in", "out"],
        };
        for name in fifos {
            let path = std::ffi::CString::new(dir.path().join(name).to_str().unwrap()).unwrap();
            // SAFETY: 路径是以 0 结尾的字符串
            assert_eq!(unsafe { libc::mkfifo(path.as_ptr(), 0o600) }, 0);
        }
        // stderr 的 FIFO：先开读端（不阻塞），shell 开写端时才不会卡住；之后一直不读
        let err_read = (launch == Launch::StderrFifo).then(|| {
            use std::os::unix::fs::OpenOptionsExt;
            std::fs::OpenOptions::new()
                .read(true)
                .custom_flags(libc::O_NONBLOCK)
                .open(dir.path().join("err"))
                .unwrap()
        });
        let script = match launch {
            Launch::AfterSupervisorDied => {
                r#"S=$$; ( while kill -0 $S 2>/dev/null; do sleep 0.01; done; IWORK_SUPERVISOR_PID=$S exec "$B" acp <"$D/in" >"$D/out" 2>"$D/err" ) & echo $! >"$D/b.pid"; exit 0"#
            }
            _ => {
                r#"IWORK_SUPERVISOR_PID=$$ "$B" acp <"$D/in" >"$D/out" 2>"$D/err" & echo $! >"$D/b.pid"; while [ ! -e "$D/die" ]; do sleep 0.01; done"#
            }
        };
        let mut shell = std::process::Command::new("sh")
            .arg("-c")
            .arg(script)
            // 日志级别默认 debug；rig 的环境里给了就用它的
            .env("IWORK_BRIDGE_LOG", "debug")
            .envs(rig.launch.env.iter().map(|(k, v)| (k, v)))
            .env("B", BRIDGE)
            .env("D", dir.path())
            .spawn()
            .unwrap();
        if launch == Launch::AfterSupervisorDied {
            // 收掉退出的 shell（不收的话它是僵尸，`kill -0` 一直成功）
            shell.wait().unwrap();
        }
        let (inp, out) = (dir.path().join("in"), dir.path().join("out"));
        let write =
            tokio::task::spawn_blocking(move || std::fs::OpenOptions::new().write(true).open(inp));
        let read = tokio::task::spawn_blocking(move || std::fs::File::open(out));
        let (write, read) = (write.await.unwrap().unwrap(), read.await.unwrap().unwrap());
        let pid_file = dir.path().join("b.pid");
        let mut bridge = 0;
        for _ in 0..1000 {
            if let Ok(s) = std::fs::read_to_string(&pid_file)
                && let Ok(p) = s.trim().parse()
            {
                bridge = p;
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        assert!(bridge > 0, "桥接起来了");
        Self {
            dir,
            shell,
            wire: Wire::new(
                pipe::Sender::from_file(write).unwrap(),
                pipe::Receiver::from_file(read).unwrap(),
            ),
            bridge,
            err_read,
        }
    }

    /// 把桥接 stderr 的 FIFO 填满（只用于 [`Launch::StderrFifo`]）：另开一个不阻塞的写端，写到写不进为止。之后桥接
    /// 往 stderr 的阻塞写都会卡住。
    pub fn fill_stderr(&self) {
        use std::io::Write;
        use std::os::unix::fs::OpenOptionsExt;
        let mut w = std::fs::OpenOptions::new()
            .write(true)
            .custom_flags(libc::O_NONBLOCK)
            .open(self.dir.path().join("err"))
            .unwrap();
        let chunk = [b'x'; 4096];
        let mut total = 0;
        loop {
            match w.write(&chunk) {
                Ok(n) => total += n,
                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => break,
                Err(e) => panic!("填 stderr 出错：{e}"),
            }
        }
        // 剩下不到一块的空间也填掉
        let one = [b'x'; 1];
        while w.write(&one).is_ok_and(|n| n == 1) {
            total += 1;
        }
        assert!(total > 0, "stderr 填进去了");
    }

    /// 后台死掉：中间 shell 退出，桥接被收养。
    pub fn daemon_dies(&mut self) {
        std::fs::write(self.dir.path().join("die"), b"").unwrap();
        self.shell.wait().unwrap();
    }

    pub fn stderr(&self) -> String {
        if self.err_read.is_some() {
            return String::new();
        }
        std::fs::read_to_string(self.dir.path().join("err")).unwrap_or_default()
    }

    /// 桥接的进程组里一个进程都没有了（僵尸也没有：被 init 回收了）。
    pub fn group_gone(&self) -> bool {
        // SAFETY: 信号 0 只检查组在不在
        unsafe { libc::killpg(self.bridge, 0) != 0 }
    }
}
