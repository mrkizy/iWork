//! 桥接的屏障测试（T06 方案 5.4a「由什么保证」，真实进程）：测试当后台，起真实的桥接程序和假 CLI；假 CLI 把
//! 收到的每个信号记下来（`log_signals_until_term`）。致命错误用后台任务事件触发（D5：看到就按 `cli_error` 停会话）。
//!
//! - 桥接自己要停、后台还在：桥接不退出、不给 CLI 发信号，这期间的 hook 都被拒，等后台来清；
//! - 后台一直不来：`TEARDOWN_WAIT` 之后桥接自己收拾 CLI；
//! - 后台没了（桥接的组成了孤儿组）：桥接活过系统发的 SIGHUP，给 CLI 发 SIGTERM，最后整个组没了；
//! - 取消一开始记的那一次范围，让 CLI 退出之后它起的命令照样被清掉。
//!
//! 每个测试结束时把自己起的进程亲手收掉（`Leftover`），不碰别的进程。
#![cfg(unix)]

mod support;

use std::os::unix::process::CommandExt as _;
use std::path::Path;
use std::time::{Duration, Instant};

use iwork_agent_acp::process::SupervisedProcess;
use iwork_bridge_claude::deadlines::{BRIDGE_TERM_WAIT, TEARDOWN_WAIT};

use iwork_ports::agent::{AgentSession, ContentBlock, UpdateStream};
use serde_json::{Value, json};
use support::acp::{BRIDGE, Rig, emit, init, opening, read_user};
use tokio::io::{AsyncBufReadExt, AsyncRead, AsyncWrite, AsyncWriteExt, BufReader, Lines};
use tokio::net::unix::pipe;

// ---------- 原始 ACP 行 ----------

/// 一条 ACP 连接的两头：写请求、按 id 读响应。
struct Wire<W, R> {
    write: Option<W>,
    read: Lines<BufReader<R>>,
}

impl<W: AsyncWrite + Unpin, R: AsyncRead + Unpin> Wire<W, R> {
    fn new(write: W, read: R) -> Self {
        Self {
            write: Some(write),
            read: BufReader::new(read).lines(),
        }
    }

    async fn send(&mut self, v: Value) {
        let w = self.write.as_mut().expect("还没关");
        w.write_all(format!("{v}\n").as_bytes()).await.unwrap();
        w.flush().await.unwrap();
    }

    async fn response(&mut self, id: u64) -> Value {
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
    async fn open(&mut self, rig: &Rig) -> Value {
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
    async fn open_and_prompt(&mut self, rig: &Rig) -> Value {
        let session = self.open(rig).await;
        self.prompt(3, &session).await
    }

    async fn prompt(&mut self, id: u64, session: &Value) -> Value {
        self.send(
            json!({ "jsonrpc": "2.0", "id": id, "method": "session/prompt",
            "params": { "sessionId": session, "prompt": [{ "type": "text", "text": "hi" }] } }),
        )
        .await;
        self.response(id).await
    }

    /// 关掉写的那一头：桥接读到 stdin 结束（后台没了）。
    fn hang_up(&mut self) {
        drop(self.write.take());
    }
}

/// 这一轮因为后台任务事件按致命错误收掉，错误里带「会话已结束」。
fn assert_fatal(response: &Value) {
    assert_eq!(
        response["error"]["data"]["iwork"],
        json!({ "code": "cli_error", "reason": "background_task", "session_ended": true }),
        "{response}"
    );
}

// ---------- 假 CLI 与进程 ----------

/// 一轮开头（`init` 之后先做 `before`）就报后台任务（致命），等 300 毫秒（桥接收摊）后再起一次工具调用（看 hook
/// 是不是被拒），然后记信号。
fn fatal_script(marker: &Path, ignore_hup: bool, before: Vec<Value>) -> Value {
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
async fn logged(rig: &Rig, kind: &str) -> Value {
    for _ in 0..2000 {
        if let Some(v) = rig.log().into_iter().find(|v| v["kind"] == kind) {
            return v;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    panic!("20 秒内假 CLI 没记 {kind}：{:?}", rig.log());
}

fn signals(rig: &Rig) -> Vec<String> {
    rig.log()
        .iter()
        .filter(|v| v["kind"] == "signal")
        .map(|v| v["signal"].as_str().unwrap().to_owned())
        .collect()
}

fn pid_of(v: &Value) -> i32 {
    v["pid"].as_i64().unwrap() as i32
}

/// 进程的状态：`'T'` 停着、`'Z'` 僵尸、`'R'` 别的活着的状态；没了为 `None`。
#[cfg(target_os = "macos")]
fn state(pid: i32) -> Option<char> {
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
fn state(pid: i32) -> Option<char> {
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
fn ppid(pid: i32) -> Option<i32> {
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
fn ppid(pid: i32) -> Option<i32> {
    let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    stat[stat.rfind(')')? + 1..]
        .split_whitespace()
        .nth(1)?
        .parse()
        .ok()
}

fn alive(pid: i32) -> bool {
    matches!(state(pid), Some('R' | 'T'))
}

async fn wait_until(what: &str, limit: Duration, mut done: impl FnMut() -> bool) {
    let started = Instant::now();
    while !done() {
        assert!(started.elapsed() < limit, "{limit:?} 内没等到：{what}");
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
}

fn signal(pid: i32, sig: libc::c_int) {
    // SAFETY: 只发信号；进程号是本测试起的进程
    unsafe { libc::kill(pid, sig) };
}

/// 本测试起的进程：结束时亲手收掉（停着的先放开，再杀）。
struct Leftover(Vec<i32>);

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
fn spawn_supervised(
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

// ---------- 后台还在 ----------

/// 致命错误之后：桥接和假 CLI 都还活着，假 CLI 没收到任何信号，这期间来的 hook 请求被拒；后台清树之后两者都没了。
#[tokio::test(flavor = "multi_thread")]
async fn fatal_stop_waits_for_daemon() {
    let marker_dir = tempfile::tempdir().unwrap();
    let marker = marker_dir.path().join("ran");
    let rig = Rig::new(fatal_script(&marker, false, vec![]));
    let (process, mut wire) = spawn_supervised(&rig);
    let bridge = process.pid() as i32;
    assert_fatal(&wire.open_and_prompt(&rig).await);
    let cli = pid_of(&logged(&rig, "started").await);
    let _cleanup = Leftover(vec![cli]);
    let tool = logged(&rig, "tool").await;
    assert_ne!(tool["decision"], "allow", "收摊之后来的 hook 被拒：{tool}");
    assert!(!marker.exists());
    logged(&rig, "watching_signals").await;
    // 等一段（小于 TEARDOWN_WAIT）：两者都在，CLI 没收到信号
    tokio::time::sleep(Duration::from_secs(1)).await;
    assert!(alive(bridge) && alive(cli), "桥接不退出、不杀 CLI");
    assert_eq!(signals(&rig), Vec::<String>::new(), "CLI 没收到任何信号");
    assert!(process.kill_tree_and_wait().await);
    assert!(!alive(cli) && !alive(bridge));
}

/// 后台一直不来清：`TEARDOWN_WAIT` 之后桥接走自己收尾那条路（方案 5.4a 第 5 条第二种）：给 CLI 发 SIGTERM，
/// 最多等 `BRIDGE_TERM_WAIT`，再对自己的进程组发 SIGKILL。CLI 直接起、留在组里的子进程跟着一起没了，桥接自己也退出。
#[tokio::test(flavor = "multi_thread")]
async fn fatal_stop_times_out_then_self_cleans() {
    let marker_dir = tempfile::tempdir().unwrap();
    let rig = Rig::new(fatal_script(
        &marker_dir.path().join("ran"),
        false,
        vec![json!({ "step": "spawn", "command": "exec sleep 30", "same_group": true })],
    ));
    let (process, mut wire) = spawn_supervised(&rig);
    let bridge = process.pid() as i32;
    let fatal_at = Instant::now();
    assert_fatal(&wire.open_and_prompt(&rig).await);
    let cli = pid_of(&logged(&rig, "started").await);
    let child = pid_of(&logged(&rig, "spawned").await);
    let _cleanup = Leftover(vec![child, cli, bridge]);
    let limit = TEARDOWN_WAIT + BRIDGE_TERM_WAIT + Duration::from_secs(5);
    wait_until("CLI 收到 SIGTERM", limit, || {
        signals(&rig).contains(&"SIGTERM".into())
    })
    .await;
    assert!(
        fatal_at.elapsed() >= TEARDOWN_WAIT,
        "等满了 TEARDOWN_WAIT 才动手"
    );
    assert_eq!(signals(&rig), ["SIGTERM"]);
    assert!(
        process
            .wait_exit(BRIDGE_TERM_WAIT + Duration::from_secs(5))
            .await
            .is_some(),
        "桥接自己退出，不等连接断"
    );
    wait_until(
        "CLI 和组里的子进程都没了",
        Duration::from_secs(5),
        || !alive(cli) && !alive(child),
    )
    .await;
}

/// 停放期里 CLI 的 stdin 一直开着：会读 stdin、读到结束就退出的 CLI 在后台来清之前不会自己走掉，它另立会话的
/// 命令顺父链找得到，清树时一起清掉（审查 S6-5b 1/F-1）。
#[tokio::test(flavor = "multi_thread")]
async fn parked_cli_keeps_stdin_until_daemon_clears() {
    let mut steps = opening();
    steps.extend([
        read_user(),
        init(),
        json!({ "step": "spawn", "command": "exec sleep 30" }),
        emit(json!({ "type": "system", "subtype": "task_started", "task_id": "a", "is_backgrounded": true })),
        support::acp::wait_eof(),
        json!({ "step": "exit", "code": 0 }),
    ]);
    let rig = Rig::new(Value::Array(steps));
    let (process, mut wire) = spawn_supervised(&rig);
    assert_fatal(&wire.open_and_prompt(&rig).await);
    let cli = pid_of(&logged(&rig, "started").await);
    let cmd = pid_of(&logged(&rig, "spawned").await);
    let _cleanup = Leftover(vec![cmd, cli]);
    // 后台还没来（小于 TEARDOWN_WAIT）：CLI 没读到 stdin 结束，还活着
    tokio::time::sleep(Duration::from_secs(1)).await;
    assert!(alive(cli), "停放期 CLI 的 stdin 没关，它没自己退出");
    assert!(process.kill_tree_and_wait().await);
    assert!(!alive(cmd), "另立会话的命令被清掉了");
}

/// 方案 5.4a 第 5 条「错误写不出去」：后台把读桥接输出的那一头关了（stdin 还开着），致命错误写不出去。桥接不等
/// `TEARDOWN_WAIT`，当场走自己收尾那条路，CLI 跟着没了。改动前的代码这一格就是对的（连接写不出去就结束），
/// 这条守着它。
#[tokio::test(flavor = "multi_thread")]
async fn unwritable_error_self_cleans() {
    let marker_dir = tempfile::tempdir().unwrap();
    // 致命事件之前等一会儿：测试先把读的那一头关掉
    let rig = Rig::new(fatal_script(
        &marker_dir.path().join("ran"),
        false,
        vec![json!({ "step": "sleep_ms", "ms": 500 })],
    ));
    let (process, mut wire) = spawn_supervised(&rig);
    let session = wire.open(&rig).await;
    wire.send(
        json!({ "jsonrpc": "2.0", "id": 3, "method": "session/prompt",
        "params": { "sessionId": session, "prompt": [{ "type": "text", "text": "hi" }] } }),
    )
    .await;
    let Wire { write, read } = wire;
    drop(read);
    let fatal_at = Instant::now();
    let cli = pid_of(&logged(&rig, "started").await);
    let _cleanup = Leftover(vec![cli, process.pid() as i32]);
    assert!(
        process
            .wait_exit(TEARDOWN_WAIT + BRIDGE_TERM_WAIT + Duration::from_secs(5))
            .await
            .is_some(),
        "桥接退出了"
    );
    assert!(
        fatal_at.elapsed() < TEARDOWN_WAIT,
        "写不出去当场收尾，不等 TEARDOWN_WAIT：{:?}",
        fatal_at.elapsed()
    );
    // CLI 这时多半还在 sleep：收到 SIGTERM 按默认动作结束，来不及记，所以只看它没了
    wait_until("CLI 没了", Duration::from_secs(5), || !alive(cli)).await;
    drop(write);
}

/// 后台不读桥接的输出（管道写满，错误写不出去也报不了错，只是卡住）：`TEARDOWN_WAIT` 到了照样自己收尾，
/// 不被卡住的写拖住。
#[tokio::test(flavor = "multi_thread")]
async fn stdout_full_still_self_cleans() {
    let marker_dir = tempfile::tempdir().unwrap();
    let chunk = "x".repeat(8 * 1024);
    let before: Vec<Value> = (0..64)
        .map(|i| support::acp::text(&format!("m{i}"), &chunk))
        .collect();
    let rig = Rig::new(fatal_script(&marker_dir.path().join("ran"), false, before));
    let (process, mut wire) = spawn_supervised(&rig);
    let session = wire.open(&rig).await;
    // 发了 prompt 之后再也不读
    wire.send(
        json!({ "jsonrpc": "2.0", "id": 3, "method": "session/prompt",
        "params": { "sessionId": session, "prompt": [{ "type": "text", "text": "hi" }] } }),
    )
    .await;
    let cli = pid_of(&logged(&rig, "started").await);
    let _cleanup = Leftover(vec![cli, process.pid() as i32]);
    logged(&rig, "watching_signals").await;
    assert!(
        process
            .wait_exit(TEARDOWN_WAIT + BRIDGE_TERM_WAIT + Duration::from_secs(5))
            .await
            .is_some(),
        "桥接退出了"
    );
    assert!(
        signals(&rig).contains(&"SIGTERM".into()),
        "{:?}",
        signals(&rig)
    );
    wait_until("CLI 没了", Duration::from_secs(5), || !alive(cli)).await;
    drop(wire);
}

// ---------- 后台没了 ----------

/// 桥接经一个中间 shell 起：后台（中间 shell）死掉时，桥接被系统收养，它的组成了孤儿组。管道是两个 FIFO，
/// 测试拿着另一头。
struct Orphanable {
    dir: tempfile::TempDir,
    shell: std::process::Child,
    wire: Wire<pipe::Sender, pipe::Receiver>,
    bridge: i32,
}

impl Orphanable {
    async fn start(rig: &Rig) -> Self {
        let dir = tempfile::tempdir().unwrap();
        for name in ["in", "out"] {
            let path = std::ffi::CString::new(dir.path().join(name).to_str().unwrap()).unwrap();
            // SAFETY: 路径是以 0 结尾的字符串
            assert_eq!(unsafe { libc::mkfifo(path.as_ptr(), 0o600) }, 0);
        }
        let shell = std::process::Command::new("sh")
            .arg("-c")
            .arg(r#""$B" acp <"$D/in" >"$D/out" 2>"$D/err" & echo $! >"$D/b.pid"; while [ ! -e "$D/die" ]; do sleep 0.01; done"#)
            .envs(rig.launch.env.iter().map(|(k, v)| (k, v)))
            .env("B", BRIDGE)
            .env("D", dir.path())
            .env("IWORK_BRIDGE_LOG", "debug")
            .spawn()
            .unwrap();
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
        }
    }

    /// 后台死掉：中间 shell 退出，桥接被收养。
    fn daemon_dies(&mut self) {
        std::fs::write(self.dir.path().join("die"), b"").unwrap();
        self.shell.wait().unwrap();
    }

    fn stderr(&self) -> String {
        std::fs::read_to_string(self.dir.path().join("err")).unwrap_or_default()
    }

    /// 桥接的进程组里一个进程都没有了（僵尸也没有：被 init 回收了）。
    fn group_gone(&self) -> bool {
        // SAFETY: 信号 0 只检查组在不在
        unsafe { libc::killpg(self.bridge, 0) != 0 }
    }
}

/// 后台死在冻结中途（CLI 停着）：系统给桥接的组发 SIGHUP、SIGCONT。桥接没被 SIGHUP 杀掉，读到 stdin 结束后
/// 给 CLI 发 SIGTERM、对自己的组发 SIGKILL，整个组没了。CLI 对 SIGHUP 分两格：默认动作（被 SIGHUP 结束）与忽略。
async fn daemon_dies_with_cli(ignore_hup: bool) {
    let marker_dir = tempfile::tempdir().unwrap();
    let rig = Rig::new(fatal_script(
        &marker_dir.path().join("ran"),
        ignore_hup,
        vec![],
    ));
    let mut b = Orphanable::start(&rig).await;
    assert_fatal(&b.wire.open_and_prompt(&rig).await);
    let cli = pid_of(&logged(&rig, "started").await);
    let _cleanup = Leftover(vec![cli, b.bridge]);
    logged(&rig, "watching_signals").await;
    // 后台开始冻结：CLI 停住了，然后后台死掉
    signal(cli, libc::SIGSTOP);
    wait_until("CLI 停住", Duration::from_secs(5), || {
        state(cli) == Some('T')
    })
    .await;
    b.daemon_dies();
    let stderr = || b.stderr();
    wait_until("桥接收到 SIGHUP", Duration::from_secs(5), || {
        stderr().contains("收到 SIGHUP")
    })
    .await;
    assert!(alive(b.bridge), "桥接没被 SIGHUP 杀掉");
    if ignore_hup {
        wait_until("CLI 记下 SIGHUP", Duration::from_secs(5), || {
            signals(&rig).contains(&"SIGHUP".into())
        })
        .await;
        assert!(alive(cli), "忽略 SIGHUP 的 CLI 还活着");
    } else {
        wait_until("CLI 被 SIGHUP 结束", Duration::from_secs(5), || {
            !alive(cli)
        })
        .await;
    }
    b.wire.hang_up();
    wait_until(
        "整个组没了",
        BRIDGE_TERM_WAIT + Duration::from_secs(5),
        || b.group_gone(),
    )
    .await;
    if ignore_hup {
        assert!(
            signals(&rig).contains(&"SIGTERM".into()),
            "桥接给 CLI 发了 SIGTERM：{:?}",
            signals(&rig)
        );
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn daemon_dies_cli_default_hup() {
    daemon_dies_with_cli(false).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn daemon_dies_cli_ignores_hup() {
    daemon_dies_with_cli(true).await;
}

/// 组里有停着的成员（桥接、CLI 都被冻住）时后台死掉：系统给根组发 SIGHUP、SIGCONT，桥接醒过来走完自己收尾那条路。
/// 场景里另有一个早就另立会话、已经被停住的后代（代表 Bash 命令，2/F-2）：它的组从建起来就是孤儿组，后台死时不会
/// 「新变成」孤儿组。照实际结果断言（方案的预期）：它没收到 SIGHUP、SIGCONT，一直停着。这是已知限制「后台服务自己
/// 没了」的一部分。
#[tokio::test(flavor = "multi_thread")]
async fn daemon_dies_mid_freeze() {
    let side = tempfile::tempdir().unwrap();
    let sig_log = side.path().join("cmd-sig");
    // CLI 正常干活时起的命令：另立会话，装了记录 SIGHUP、SIGCONT 的处理函数
    let command = format!(
        r#"exec perl -e '$SIG{{HUP}} = sub {{ open my $f, ">>", "{l}"; print $f "HUP\n" }};
            $SIG{{CONT}} = sub {{ open my $f, ">>", "{l}"; print $f "CONT\n" }}; sleep 1 while 1'"#,
        l = sig_log.display()
    );
    let rig = Rig::new(fatal_script(
        &side.path().join("ran"),
        true,
        vec![json!({ "step": "spawn", "command": command })],
    ));
    let mut b = Orphanable::start(&rig).await;
    assert_fatal(&b.wire.open_and_prompt(&rig).await);
    let cli = pid_of(&logged(&rig, "started").await);
    let cmd = pid_of(&logged(&rig, "spawned").await);
    let _cleanup = Leftover(vec![cmd, cli, b.bridge]);
    logged(&rig, "watching_signals").await;
    // 后台冻结：命令、CLI、桥接都停住，然后后台死掉
    for pid in [cmd, cli, b.bridge] {
        signal(pid, libc::SIGSTOP);
    }
    wait_until("都停住", Duration::from_secs(5), || {
        [cmd, cli, b.bridge].iter().all(|p| state(*p) == Some('T'))
    })
    .await;
    b.daemon_dies();
    let stderr = || b.stderr();
    wait_until(
        "桥接醒过来、收到 SIGHUP",
        Duration::from_secs(5),
        || stderr().contains("收到 SIGHUP"),
    )
    .await;
    b.wire.hang_up();
    wait_until(
        "桥接的组没了",
        BRIDGE_TERM_WAIT + Duration::from_secs(5),
        || b.group_gone(),
    )
    .await;
    // 命令：照实际结果断言
    tokio::time::sleep(Duration::from_millis(300)).await;
    let got = std::fs::read_to_string(&sig_log).unwrap_or_default();
    assert_eq!(
        (state(cmd), got.as_str()),
        (Some('T'), ""),
        "另立会话的命令没收到 SIGHUP/SIGCONT，一直停着"
    );
}

// ---------- 取消 ----------

/// 取消一开始记一次范围（5.4a「什么时候记」）：CLI 起了一条另立会话的命令，收到 interrupt 就自己退出、不收拾
/// 命令（命令被系统收养，父链断了）。后台清树时它照样被清掉，靠的是取消一开始记下的那一次。
/// 采样每 2 秒才记一次：这里从开会话到 CLI 退出不到 2 秒，中间没有采样。
#[tokio::test(flavor = "multi_thread")]
async fn cancel_records_scope_first() {
    let mut steps = opening();
    steps.extend([
        read_user(),
        init(),
        json!({ "step": "spawn", "command": "exec sleep 30" }),
        // 等 interrupt 控制请求，然后不出 result、直接退出
        read_user(),
        json!({ "step": "exit", "code": 0 }),
    ]);
    let rig = Rig::new(Value::Array(steps));
    let s = rig
        .open(rig.spec(), support::acp::Handler(Default::default()))
        .await
        .unwrap();
    let mut updates = s
        .prompt(vec![ContentBlock::Text("hi".into())])
        .await
        .unwrap();
    let cmd = pid_of(&logged(&rig, "spawned").await);
    let _cleanup = Leftover(vec![cmd]);
    assert!(alive(cmd));
    s.cancel().await.unwrap();
    let _ = tokio::time::timeout(Duration::from_secs(10), async {
        while updates.next().await.is_some() {}
    })
    .await;
    wait_until("命令被清掉", Duration::from_secs(5), || !alive(cmd)).await;
}

// ---------- 后台自己清（不经测试调用） ----------

/// 回合里出致命错误：错误带「会话已结束」，后台的 ACP 客户端见到就清树（5.4a 第 5 条）。CLI 起的、另立会话的命令
/// 不等桥接的 `TEARDOWN_WAIT`，就被清掉。
#[tokio::test(flavor = "multi_thread")]
async fn fatal_error_clears_the_tree() {
    let side = tempfile::tempdir().unwrap();
    let rig = Rig::new(fatal_script(
        &side.path().join("ran"),
        false,
        vec![json!({ "step": "spawn", "command": "exec sleep 30" })],
    ));
    let s = rig
        .open(rig.spec(), support::acp::Handler(Default::default()))
        .await
        .unwrap();
    let started = Instant::now();
    let updates = support::acp::turn(&s, "hi").await.unwrap();
    assert!(
        matches!(
            updates.last(),
            Some(iwork_ports::agent::AgentUpdate::Failed(_))
        ),
        "{updates:?}"
    );
    let cmd = pid_of(&logged(&rig, "spawned").await);
    let cli = pid_of(&logged(&rig, "started").await);
    let _cleanup = Leftover(vec![cmd, cli]);
    wait_until("命令和 CLI 被清掉", TEARDOWN_WAIT, || {
        !alive(cmd) && !alive(cli)
    })
    .await;
    assert!(
        started.elapsed() < TEARDOWN_WAIT,
        "是后台清的，不是桥接到点自己收拾的"
    );
}

/// 桥接自己崩了（空闲时被 SIGKILL）：后台看到它的输出到头就清树。CLI 还活着、没被停过，它和它起的命令都被清掉。
#[tokio::test(flavor = "multi_thread")]
async fn bridge_crash_clears_the_tree() {
    let mut steps = opening();
    steps.extend([
        read_user(),
        init(),
        json!({ "step": "spawn", "command": "exec sleep 30" }),
        support::acp::result_ok(),
        json!({ "step": "log_signals_until_term" }),
    ]);
    let rig = Rig::new(Value::Array(steps));
    let s = rig
        .open(rig.spec(), support::acp::Handler(Default::default()))
        .await
        .unwrap();
    support::acp::turn(&s, "hi").await.unwrap();
    let cmd = pid_of(&logged(&rig, "spawned").await);
    let cli = pid_of(&logged(&rig, "started").await);
    let bridge = ppid(cli).expect("CLI 的父进程是桥接");
    let _cleanup = Leftover(vec![cmd, cli]);
    signal(bridge, libc::SIGKILL);
    wait_until("CLI 和命令被清掉", Duration::from_secs(5), || {
        !alive(cmd) && !alive(cli)
    })
    .await;
    drop(s);
}

// ---------- 回合进行中后台没了 ----------

/// 回合正在跑（CLI 不出结果）时后台关了连接：桥接照样收摊、给 CLI 发 SIGTERM、退出，不等那一轮的答复
/// （会话任务不挂在连接上，连接收尾不等它；还在等答复的 prompt 任务随连接一起丢掉）。
#[tokio::test(flavor = "multi_thread")]
async fn hang_up_mid_turn_exits() {
    let mut steps = opening();
    steps.extend([
        read_user(),
        init(),
        json!({ "step": "log_signals_until_term" }),
    ]);
    let rig = Rig::new(Value::Array(steps));
    let (process, mut wire) = spawn_supervised(&rig);
    let bridge = process.pid() as i32;
    wire.send(json!({ "jsonrpc": "2.0", "id": 1, "method": "initialize",
        "params": { "protocolVersion": 1, "clientCapabilities": {} } }))
        .await;
    assert!(wire.response(1).await["result"].is_object());
    let meta = iwork_agent_acp::convert::session_setup_meta(&rig.spec()).unwrap();
    wire.send(json!({ "jsonrpc": "2.0", "id": 2, "method": "session/new",
        "params": { "cwd": rig.work.path(), "mcpServers": [], "_meta": meta } }))
        .await;
    let session = wire.response(2).await["result"]["sessionId"].clone();
    assert!(session.is_string());
    // 发 prompt，不等答复：CLI 收下这一轮就一直不出结果
    wire.send(
        json!({ "jsonrpc": "2.0", "id": 3, "method": "session/prompt",
        "params": { "sessionId": session, "prompt": [{ "type": "text", "text": "hi" }] } }),
    )
    .await;
    let cli = pid_of(&logged(&rig, "started").await);
    let _cleanup = Leftover(vec![cli, bridge]);
    logged(&rig, "watching_signals").await;
    wire.hang_up();
    assert!(
        process
            .wait_exit(BRIDGE_TERM_WAIT + Duration::from_secs(5))
            .await
            .is_some(),
        "回合进行中连接断了，桥接照样退出"
    );
    assert!(
        signals(&rig).contains(&"SIGTERM".into()),
        "桥接给 CLI 发了 SIGTERM：{:?}",
        signals(&rig)
    );
}

/// 别的进程继承到了桥接 stdin 的写端（macOS 上 std 建管道和设 close-on-exec 是两步，后台同时起两个子进程时，
/// 后起的那个可能把先起那个的管道一起继承走）：桥接自己一启动就关掉继承来的句柄，不把它们带进 CLI。所以后台关掉
/// 自己那一端，桥接照样读到 stdin 结束、退出，不被别的桥接拖着。这里用另一个桥接当「别的进程」，让它确定地拿到
/// 第一个桥接 stdin 写端的副本。
#[tokio::test(flavor = "multi_thread")]
async fn inherited_stdin_copy_does_not_keep_bridge_alive() {
    use std::os::fd::AsRawFd;
    let rig = Rig::new(Value::Array(opening()));
    let (process, mut wire) = spawn_supervised(&rig);
    let first = process.pid() as i32;
    // 第一个桥接 stdin 写端的副本，不带 close-on-exec：下面起的进程会继承它
    // SAFETY: dup 只读一个整数参数；句柄是本测试拿着的管道写端
    let copy = unsafe { libc::dup(wire.write.as_ref().unwrap().as_raw_fd()) };
    assert!(copy > 2, "{}", std::io::Error::last_os_error());
    let mut other = std::process::Command::new(BRIDGE)
        .arg("acp")
        .envs(rig.launch.env.iter().map(|(k, v)| (k, v)))
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .process_group(0)
        .spawn()
        .unwrap();
    // SAFETY: 关本测试 dup 出来的句柄，只关一次
    unsafe { libc::close(copy) };
    let _cleanup = Leftover(vec![first, other.id() as i32]);
    // 等另一个桥接起来、走过启动（它那时候拿着副本；修好之后它一启动就关掉）
    tokio::time::sleep(Duration::from_millis(500)).await;
    assert!(
        other.try_wait().unwrap().is_none(),
        "另一个桥接还活着（它的 stdin 还开着）"
    );
    wire.hang_up();
    let exited = process.wait_exit(Duration::from_secs(10)).await.is_some();
    drop(other.stdin.take());
    let _ = other.wait();
    assert!(
        exited,
        "后台关了连接，第一个桥接就退出，不被继承了它 stdin 写端的进程拖着"
    );
}
