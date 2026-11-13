//! 真进程测试的夹具：每个测试一个短的临时家目录、真起 `iwork-daemon`（`--dev-trust`）、结束时收干净。
//!
//! 收干净的保证：[`Daemon`] 丢弃时 `kill -9` 并 `wait`（`exec` 换过程序的仍是同一个子进程）；[`Sandbox`] 丢弃时删目录。
//! 测试里新起的后台一律经 [`Sandbox::spawn`]，并且 `Daemon` 变量声明在 `Sandbox` 之后（先丢弃）。

#![allow(dead_code)]

use std::ffi::OsString;
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, ExitStatus, Stdio};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use iwork_ipc::client::{Client, ClientConfig, ConnectError, Incoming};
use iwork_ipc::peer::{SelfIdentity, TrustPolicy};
use iwork_ipc::rendezvous::{self, Phase, StartupStatus};
use iwork_ipc::{LocalConnector, LocalEndpoint};
use iwork_proto::pb;

pub const DAEMON: &str = env!("CARGO_BIN_EXE_iwork-daemon");
/// [`Sandbox::wait_ready`] 的总上限是没进展时限的几倍。
const WAIT_READY_TOTAL_FACTOR: u32 = 3;

/// 拷一个之后要被执行的文件。写交给子进程（`cp`）做：本进程若开着它的写句柄，同一时刻别的测试线程 fork
/// 出的子进程会继承这个句柄直到它 exec，期间执行这个文件在 Linux 上报 ETXTBSY（CI 实测）。
pub fn copy_exe(src: &Path, dst: &Path) {
    run_ok(Command::new("cp").arg(src).arg(dst));
}

/// 写一个可执行脚本（理由同 [`copy_exe`]）：正文先写进旁边一个不会被执行的文件，再拷过去。
pub fn write_script(dst: &Path, text: &str) {
    let src = dst.with_extension("src");
    std::fs::write(&src, text).unwrap();
    copy_exe(&src, dst);
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(dst, std::fs::Permissions::from_mode(0o755)).unwrap();
}

/// 往之后要被执行的文件末尾追加字节（理由同 [`copy_exe`]）。
pub fn append_exe(dst: &Path, text: &str) {
    run_ok(
        Command::new("sh")
            .args(["-c", "printf %s \"$1\" >> \"$2\"", "sh", text])
            .arg(dst),
    );
}

fn run_ok(cmd: &mut Command) {
    let out = cmd.output().unwrap();
    assert!(
        out.status.success(),
        "{cmd:?}：{}",
        String::from_utf8_lossy(&out.stderr)
    );
}

/// 一个测试的家目录（`/tmp/iwt-XXXXXX`：macOS 的 socket 路径上限 103 字节，默认临时目录太长）。
pub struct Sandbox {
    dir: Option<tempfile::TempDir>,
    pub home: PathBuf,
}

impl Sandbox {
    pub fn new() -> Self {
        let dir = tempfile::Builder::new()
            .prefix("iwt-")
            .tempdir_in("/tmp")
            .expect("建临时家目录");
        let home = std::fs::canonicalize(dir.path()).unwrap();
        Self {
            dir: Some(dir),
            home,
        }
    }

    /// socket 目录（会合点）。没设 `XDG_RUNTIME_DIR`，所以和数据目录的 `run/` 是同一个。
    pub fn run_dir(&self) -> PathBuf {
        iwork_ipc::endpoint::unix_run_dir(|_| None, Some(self.home.clone())).unwrap()
    }

    /// 数据目录。
    pub fn data_root(&self) -> PathBuf {
        if cfg!(target_os = "macos") {
            self.home.join("Library/Application Support/iWork")
        } else {
            self.home.join(".local/share/iwork")
        }
    }

    pub fn db(&self) -> PathBuf {
        self.data_root().join("iwork.db")
    }

    pub fn status(&self) -> Option<StartupStatus> {
        rendezvous::read_status(&self.run_dir())
    }

    /// 后台的命令行：家目录指进来，`XDG_*`、`IWORK_DATA_DIR` 清掉。
    pub fn command(&self, exe: &Path, args: &[&str]) -> Command {
        daemon_command(&self.home, exe, args)
    }

    pub fn spawn(&self, args: &[&str], env: &[(&str, &str)]) -> Daemon {
        self.spawn_exe(Path::new(DAEMON), args, env)
    }

    pub fn spawn_exe(&self, exe: &Path, args: &[&str], env: &[(&str, &str)]) -> Daemon {
        spawn_in(&self.home, exe, args, env)
    }
}

/// [`Sandbox::command`] 的本体（不借 `Sandbox`，给要 `'static` 的地方用，如 `Launcher` 的拉起实现）。
pub fn daemon_command(home: &Path, exe: &Path, args: &[&str]) -> Command {
    let mut c = Command::new(exe);
    c.arg("--dev-trust").args(args);
    c.env("HOME", home);
    for k in [
        "XDG_RUNTIME_DIR",
        "XDG_DATA_HOME",
        "XDG_CONFIG_HOME",
        "IWORK_DATA_DIR",
        "IWORK_DEV_FAIL_AT",
        "IWORK_DEV_PAUSE_AT",
        "IWORK_DEV_RESUME_FILE",
        "IWORK_LOG",
    ] {
        c.env_remove(k);
    }
    c.stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped());
    c
}

/// [`Sandbox::spawn_exe`] 的本体。
pub fn spawn_in(home: &Path, exe: &Path, args: &[&str], env: &[(&str, &str)]) -> Daemon {
    let mut c = daemon_command(home, exe, args);
    for (k, v) in env {
        c.env(k, v);
    }
    let mut child = c.spawn().expect("起后台");
    let lines = Arc::new(Mutex::new(Vec::new()));
    let stderr = child.stderr.take().unwrap();
    let sink = lines.clone();
    std::thread::spawn(move || {
        for line in BufReader::new(stderr).lines().map_while(Result::ok) {
            sink.lock().unwrap().push(line);
        }
    });
    Daemon {
        child: Some(child),
        stderr: lines,
    }
}

impl Sandbox {
    /// 起一次、等它自己退出（启动失败一类的用例）。
    pub fn run_to_exit(&self, args: &[&str], env: &[(&str, &str)]) -> (ExitStatus, String) {
        let mut d = self.spawn(args, env);
        let status = d.wait_exit(Duration::from_secs(20));
        (status, d.stderr())
    }

    /// 等状态文件变成 `Ready` 且是 `pid` 写的。
    ///
    /// 时限按进展算：状态文件里的（pid, 阶段）一变就重新计时，`stall` 内毫无变化才算卡住；慢机器上启动步骤
    /// 一步步在走就接着等，直到总上限 `stall × WAIT_READY_TOTAL_FACTOR`。失败时打印每次变化的时间点，
    /// 分得清是「停在某一步」还是「一直在走、只是慢」。
    pub fn wait_ready(&self, pid: u32, stall: Duration) {
        let started = Instant::now();
        let total = stall * WAIT_READY_TOTAL_FACTOR;
        let stage = |s: &Option<StartupStatus>| s.as_ref().map(|s| (s.pid, s.phase.clone()));
        let mut last = stage(&self.status());
        let mut last_change = started;
        let mut history = vec![(Duration::ZERO, last.clone())];
        loop {
            let now = self.status();
            if let Some(s) = &now
                && s.pid == pid
                && s.phase == Phase::Ready
            {
                return;
            }
            if stage(&now) != last {
                last = stage(&now);
                last_change = Instant::now();
                history.push((started.elapsed(), last.clone()));
            }
            let stalled = last_change.elapsed() >= stall;
            assert!(
                !stalled && started.elapsed() < total,
                "没等到 pid {pid} 就绪（{}）；(pid, 阶段) 的变化：{history:#?}；最终状态：{now:?}",
                if stalled {
                    format!("{stall:?} 内状态没有任何变化")
                } else {
                    format!("一直在变，但超过总上限 {total:?}")
                }
            );
            std::thread::sleep(Duration::from_millis(20));
        }
    }

    /// 以 dev 身份连后台并握手。
    pub async fn connect(&self) -> Result<(Client, Incoming), ConnectError> {
        let own = SelfIdentity::detect().expect("测试进程的身份");
        let endpoint = LocalEndpoint::in_run_dir(&self.run_dir()).unwrap();
        let mut cfg = ClientConfig::new(pb::ClientKind::DevTool, "test");
        cfg.trust = TrustPolicy::DevTrust;
        Client::connect(&LocalConnector, endpoint.endpoint(), &own, cfg).await
    }

    /// 断言家目录里没有 iWork 的任何东西（参数错、身份失败一类的用例：不拿锁、不写状态、不建数据目录）。
    /// 不成立时把证据写进 panic：哪个目录在、家目录下的全部条目（类型、大小、修改时刻），以及会合点里状态文件与
    /// 锁文件的内容（锁文件里记着是哪个 pid 写的）。T09 的 CI 上红过一次而当时看不出是谁动的（run 43228211480）。
    #[track_caller]
    pub fn assert_untouched(&self) {
        let present: Vec<PathBuf> = [self.data_root(), self.run_dir()]
            .into_iter()
            .filter(|p| p.exists())
            .collect();
        if present.is_empty() {
            return;
        }
        let mut report = format!(
            "家目录里出现了 iWork 的东西：{present:?}\n家目录 {} 下的条目：\n",
            self.home.display()
        );
        list_tree(&self.home, &self.home, &mut report, 0);
        let run = self.run_dir();
        for name in [rendezvous::STATUS_FILE, rendezvous::LOCK_FILE] {
            let path = run.join(name);
            if let Ok(text) = std::fs::read_to_string(&path) {
                let cut: String = text.chars().take(2000).collect();
                report.push_str(&format!("--- {}：\n{cut}\n", path.display()));
            }
        }
        panic!("{report}");
    }
}

impl Drop for Sandbox {
    fn drop(&mut self) {
        if let Some(d) = self.dir.take() {
            let _ = d.close();
        }
    }
}

pub fn alive(pid: u32) -> bool {
    // SAFETY：kill 的第二个参数为 0 只探测，不发信号
    unsafe { libc::kill(pid as i32, 0) == 0 }
}

/// 递归列出 `dir` 下的条目（相对 `root`），最多 6 层、300 条。不跟随符号链接。
fn list_tree(root: &Path, dir: &Path, out: &mut String, depth: usize) {
    if depth > 6 || out.lines().count() > 300 {
        return;
    }
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    let mut entries: Vec<_> = entries.filter_map(Result::ok).collect();
    entries.sort_by_key(|e| e.file_name());
    for e in entries {
        let path = e.path();
        let Ok(meta) = std::fs::symlink_metadata(&path) else {
            continue;
        };
        let kind = if meta.is_dir() {
            "目录"
        } else if meta.file_type().is_symlink() {
            "链接"
        } else {
            "文件"
        };
        let modified = meta
            .modified()
            .ok()
            .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
            .map_or(0, |d| d.as_millis());
        out.push_str(&format!(
            "  {kind} {} {} 字节 mtime_ms={modified}\n",
            path.strip_prefix(root).unwrap_or(&path).display(),
            meta.len()
        ));
        if meta.is_dir() {
            list_tree(root, &path, out, depth + 1);
        }
    }
}

/// 日志里每次「后台服务就绪」的运行映像身份，按先后。
pub fn readies(sb: &Sandbox) -> Vec<String> {
    log_text(sb)
        .lines()
        .filter(|l| l.contains("后台服务就绪"))
        .filter_map(|l| {
            let v: serde_json::Value = serde_json::from_str(l).ok()?;
            v["fields"]["image"].as_str().map(str::to_owned)
        })
        .collect()
}

pub fn wait_readies(sb: &Sandbox, n: usize, limit: Duration) -> Vec<String> {
    let deadline = Instant::now() + limit;
    loop {
        let r = readies(sb);
        if r.len() >= n {
            return r;
        }
        assert!(
            Instant::now() < deadline,
            "{limit:?} 内没等到第 {n} 次就绪：{r:?}"
        );
        std::thread::sleep(Duration::from_millis(50));
    }
}

/// 等 `pid` 就绪，并等它的「后台服务就绪」那行日志写下来。后台先把状态文件写成 Ready、再记这行日志，日志还经异步
/// 写线程落盘：只等状态文件就去读日志里最后一次就绪，读到的可能还是前任的（CI run 43236780312）。
pub fn wait_ready_logged(sb: &Sandbox, pid: u32, readies_before: usize, limit: Duration) {
    sb.wait_ready(pid, limit);
    wait_readies(sb, readies_before + 1, limit);
}

pub fn kill9(pid: u32) {
    // SAFETY：只对本测试起的进程发信号
    unsafe {
        libc::kill(pid as i32, libc::SIGKILL);
    }
}

pub fn signal(pid: u32, sig: i32) {
    // SAFETY：同上
    unsafe {
        libc::kill(pid as i32, sig);
    }
}

/// 一个后台进程。丢弃时 `kill -9` 并收尸。
pub struct Daemon {
    child: Option<Child>,
    stderr: Arc<Mutex<Vec<String>>>,
}

impl Daemon {
    pub fn pid(&self) -> u32 {
        self.child.as_ref().unwrap().id()
    }

    pub fn stderr(&self) -> String {
        self.stderr.lock().unwrap().join("\n")
    }

    /// 等 stderr 出现一行含 `needle` 的。
    pub fn wait_stderr(&self, needle: &str, limit: Duration) {
        let deadline = Instant::now() + limit;
        while !self.stderr().contains(needle) {
            assert!(
                Instant::now() < deadline,
                "{limit:?} 内 stderr 没出现「{needle}」：\n{}",
                self.stderr()
            );
            std::thread::sleep(Duration::from_millis(20));
        }
    }

    pub fn try_exit(&mut self) -> Option<ExitStatus> {
        self.child.as_mut().unwrap().try_wait().unwrap()
    }

    pub fn wait_exit(&mut self, limit: Duration) -> ExitStatus {
        let deadline = Instant::now() + limit;
        loop {
            if let Some(s) = self.try_exit() {
                return s;
            }
            assert!(
                Instant::now() < deadline,
                "{limit:?} 内后台没退出；stderr：\n{}",
                self.stderr()
            );
            std::thread::sleep(Duration::from_millis(20));
        }
    }

    pub fn kill9(&mut self) -> ExitStatus {
        kill9(self.pid());
        self.wait_exit(Duration::from_secs(10))
    }
}

impl Drop for Daemon {
    fn drop(&mut self) {
        if let Some(mut c) = self.child.take()
            && c.try_wait().ok().flatten().is_none()
        {
            let _ = c.kill();
            let _ = c.wait();
        }
        // 后台被 kill -9（或用例中途失败）时，开发构建的假工作起的进程组没人收：在这里收掉，不留进程
        for pgid in self.dev_pgids() {
            // SAFETY：只结束本测试的后台起的进程组
            unsafe {
                libc::kill(-(pgid as i32), libc::SIGKILL);
            }
        }
    }
}

/// 进程的退出码；被信号结束为 `None`。
pub fn code(s: ExitStatus) -> Option<i32> {
    s.code()
}

pub fn args(v: &[&str]) -> Vec<OsString> {
    v.iter().map(OsString::from).collect()
}

/// 异步测试用的运行时。
pub fn rt() -> tokio::runtime::Runtime {
    tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .unwrap()
}

/// 经 ipc 发 `ShutdownDaemon`（开发工具身份；dev 构建认它）。
pub fn shutdown_daemon(sb: &Sandbox, cancel_running: bool) -> pb::Response {
    rt().block_on(async {
        let (client, _incoming) = sb.connect().await.expect("握手");
        let r = client
            .request(pb::request::Body::Command(pb::Command {
                command_id: ulid::Ulid::generate().to_string(),
                kind: Some(pb::command::Kind::ShutdownDaemon(pb::ShutdownDaemon {
                    cancel_running,
                })),
            }))
            .await;
        // 错误响应在客户端那边以 `ClientError::Server` 出来；统一成响应，测试按 body 判
        match r {
            Ok(r) => r,
            Err(iwork_ipc::client::ClientError::Server(e)) => pb::Response {
                request_id: 0,
                body: Some(pb::response::Body::Error(e)),
            },
            Err(e) => panic!("请求失败：{e:?}"),
        }
    })
}

impl Daemon {
    /// 开发构建的假工作起的进程组（stderr 里的 `IWORK-DEV-PGID`）。
    pub fn dev_pgids(&self) -> Vec<u32> {
        self.stderr()
            .lines()
            .filter_map(|l| l.strip_prefix("IWORK-DEV-PGID "))
            .map(|n| n.trim().parse().unwrap())
            .collect()
    }
}

/// 进程组里还有没有进程。
pub fn group_alive(pgid: u32) -> bool {
    // SAFETY：信号 0 只探测
    unsafe { libc::kill(-(pgid as i32), 0) == 0 }
}

/// 日志文件的全部内容。
pub fn log_text(sb: &Sandbox) -> String {
    std::fs::read_to_string(sb.data_root().join("logs/daemon.log")).unwrap_or_default()
}
