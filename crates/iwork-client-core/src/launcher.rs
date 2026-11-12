//! 连接后台服务，连不上就拉起（T04 方案 2.7；设计 4「拉起」；卡范围 1、验收 2）。
//!
//! - 只有 `ConnectError::Io`（socket 不存在、拒绝连接）才拉起；别的错误说明对面有东西在听，原样交给调用方。
//! - 拉不拉照 2.2 角色表的「客户端」一列，判据是会合点的 `relaunchable`，客户端没有自己的判断（[`should_launch`]）。
//! - 一个绝对截止时间管全程：等连接尝试、起进程、退避睡眠都受它约束，到点立刻返回诊断数据。
//! - 一次连接尝试是一个独立任务，到点不取消：下一次调用接着等这同一次尝试，同一时刻最多一次尝试在飞
//!   （对端身份校验在阻塞线程里算哈希，丢掉 future 也停不下来）。
//! - 同一个 `Launcher` 两次拉起至少隔 2s。客户端不结束任何进程。
//!
//! `Launcher` 要留着复用（它记着上次拉起的时刻和没出结果的连接尝试），不要每次重连都新建。

use std::future::Future;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use iwork_ipc::LocalEndpoint;
use iwork_ipc::client::{Client, ClientConfig, ConnectError, Incoming};
use iwork_ipc::peer::SelfIdentity;
use iwork_ipc::rendezvous::{
    self, ExeStamp, LockInfo, Phase, Refusal, Relaunchable, StartupStatus, relaunchable,
};
use iwork_ipc::transport::Endpoint;
use iwork_proto::pb;
use tokio::task::JoinHandle;
use tokio::time::Instant;

use crate::session::dial::{DialError, Dialer};

/// 默认的总预算（需求 14：客户端重开 1s 内恢复现场）。
pub const BUDGET: Duration = Duration::from_secs(1);
/// 同一个 `Launcher` 两次拉起之间至少隔这么久（防止重连循环连着起一串马上就退出的进程）。
pub const LAUNCH_GAP: Duration = Duration::from_secs(2);
/// 拉起后第一次重连前等多久；之后翻倍。
pub const FIRST_RETRY: Duration = Duration::from_millis(10);

/// 一次连接尝试（真实现是 `Client::connect`；测试换成假的）。返回的 future 要是 `'static`：它被放进独立任务。
pub trait Connect: Send + Sync + 'static {
    fn connect(
        &self,
    ) -> impl Future<Output = Result<(Client, Incoming), ConnectError>> + Send + 'static;
}

/// 起后台进程（真实现是 [`DetachedSpawn`]；测试换成记数的假实现）。
pub trait Spawn: Send + 'static {
    fn spawn(&mut self, exe: &Path, args: &[&str]) -> std::io::Result<()>;
}

/// 连本机的后台：`LocalConnector` + 双向身份校验 + 握手。
#[derive(Clone)]
pub struct IpcConnect {
    pub endpoint: Endpoint,
    pub own: SelfIdentity,
    pub cfg: ClientConfig,
}

impl Connect for IpcConnect {
    fn connect(
        &self,
    ) -> impl Future<Output = Result<(Client, Incoming), ConnectError>> + Send + 'static {
        let this = self.clone();
        async move {
            Client::connect(
                &iwork_ipc::LocalConnector,
                &this.endpoint,
                &this.own,
                this.cfg,
            )
            .await
        }
    }
}

/// 脱离方式起后台：新会话（`setsid`）、标准流接 `/dev/null`、工作目录 `/`、环境变量原样继承；另起一个线程收尸。
/// 客户端退出不带走它（新会话、不共用管道）。
#[derive(Debug, Default, Clone, Copy)]
pub struct DetachedSpawn;

impl Spawn for DetachedSpawn {
    #[cfg(unix)]
    fn spawn(&mut self, exe: &Path, args: &[&str]) -> std::io::Result<()> {
        use std::os::unix::process::CommandExt;
        use std::process::{Command, Stdio};
        let mut cmd = Command::new(exe);
        cmd.args(args)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .current_dir("/");
        // SAFETY：pre_exec 里只调 async-signal-safe 的 setsid
        unsafe {
            cmd.pre_exec(|| {
                if libc::setsid() == -1 {
                    return Err(std::io::Error::last_os_error());
                }
                Ok(())
            });
        }
        let mut child = cmd.spawn()?;
        std::thread::Builder::new()
            .name("iwork-daemon-reaper".into())
            .spawn(move || {
                let _ = child.wait();
            })?;
        Ok(())
    }

    #[cfg(not(unix))]
    fn spawn(&mut self, _exe: &Path, _args: &[&str]) -> std::io::Result<()> {
        // M1 不支持 Windows：身份那一步就返回 Environment，走不到这里（方案 2.15）
        Err(std::io::Error::other("M1 暂不支持 Windows"))
    }
}

/// `Launcher` 的配置。
#[derive(Debug, Clone)]
pub struct LaunchConfig {
    /// 后台的可执行文件（与客户端同一目录，T03 2.4）。
    pub daemon_exe: PathBuf,
    pub budget: Duration,
    /// 会合点目录（socket 目录）：状态文件、锁文件在这里。Windows 没有。
    pub run_dir: Option<PathBuf>,
    /// 诊断页显示的端点。
    pub endpoint: Result<PathBuf, String>,
    /// 拉起时带的参数（开发构建的 `--dev-trust`）。
    pub args: Vec<&'static str>,
}

/// 连接失败。
#[derive(Debug)]
pub enum LaunchError {
    /// 有后台但连不上：版本不一致、握手超时等。原样交给 T09 按 T03 2.4c 处理，不拉起。
    Connect(ConnectError),
    /// 拉不起来或预算内没连上：带一份诊断。
    NotReady(Box<Diagnostics>),
    /// 本机环境不行：身份算不出（含 M1 的 Windows）、端点目录不安全。
    Environment(String),
}

/// 现在能看到的全部诊断数据。纯读取，不拉起、不加锁、不结束进程。
#[derive(Debug, Clone, PartialEq)]
pub struct Diagnostics {
    /// 停在哪一步（「连接中」「拉起失败」「等后台就绪」「同一个程序已经说过起不来」）。
    pub stage: &'static str,
    pub endpoint: Result<PathBuf, String>,
    pub lock: Option<LockInfo>,
    /// `kill(pid, 0)`；只是提示，pid 可能被复用。
    pub lock_pid_alive: Option<bool>,
    pub status: Option<StartupStatus>,
    pub client_version: String,
    pub client_protocol_major: u32,
    pub last_error: Option<String>,
    pub spawn_error: Option<String>,
}

/// 读会合点，凑一份诊断。
pub fn diagnose(
    stage: &'static str,
    run_dir: Option<&Path>,
    endpoint: Result<PathBuf, String>,
    last_error: Option<String>,
    spawn_error: Option<String>,
) -> Diagnostics {
    let lock = run_dir.and_then(rendezvous::read_lock_info);
    let lock_pid_alive = lock.as_ref().map(|l| pid_alive(l.pid));
    Diagnostics {
        stage,
        endpoint,
        lock,
        lock_pid_alive,
        status: run_dir.and_then(rendezvous::read_status),
        client_version: env!("CARGO_PKG_VERSION").into(),
        client_protocol_major: iwork_proto::PROTOCOL_MAJOR,
        last_error,
        spawn_error,
    }
}

#[cfg(unix)]
fn pid_alive(pid: u32) -> bool {
    // SAFETY：信号 0 只探测，不发信号
    unsafe { libc::kill(pid as i32, 0) == 0 }
}

#[cfg(not(unix))]
fn pid_alive(_pid: u32) -> bool {
    false
}

impl Diagnostics {
    /// 诊断页的数据（T09 的 `store::Diagnostics`）：一句说明 + 若干行 + 「装回更新的那一版」提示。
    pub fn to_view(&self) -> crate::store::Diagnostics {
        let mut lines: Vec<(String, String)> = Vec::new();
        let mut push = |k: &str, v: String| lines.push((k.into(), v));
        push("阶段", self.stage.into());
        match &self.endpoint {
            Ok(p) => push("端点", p.display().to_string()),
            Err(e) => push("端点", e.clone()),
        }
        if let Some(l) = &self.lock {
            let alive = match self.lock_pid_alive {
                Some(true) => "在运行",
                Some(false) => "已不在",
                None => "未知",
            };
            push(
                "后台进程",
                format!("pid {}（{alive}），版本 {}", l.pid, l.version),
            );
        }
        let mut newer = false;
        let mut reason = String::from("后台服务还没就绪");
        if let Some(s) = &self.status {
            push("日志目录", s.log_dir.display().to_string());
            push("启动阶段", phase_text(&s.phase));
            if let Phase::Refused(r) = &s.phase {
                newer = r.written_by_newer();
                match r {
                    Refusal::ProtocolMajorMismatch {
                        database,
                        program,
                        backup,
                    } => {
                        reason = "数据库与程序的协议主版本不一致，后台服务拒绝启动".into();
                        push("数据库主版本", database.to_string());
                        push("程序主版本", program.to_string());
                        push("备份", backup.display().to_string());
                    }
                    Refusal::StoreOpenFailed { message } => {
                        reason = "后台服务拒绝打开数据库".into();
                        push("原因", message.clone());
                    }
                }
            }
            if let Phase::Exited {
                message: Some(m), ..
            } = &s.phase
            {
                push("退出原因", m.clone());
            }
            if s.not_cleared > 0 {
                push(
                    "上次退出",
                    format!("有 {} 个成员进程没能及时结束", s.not_cleared),
                );
            }
        }
        if let Some(e) = &self.spawn_error {
            reason = "拉不起后台服务".into();
            push("拉起失败", e.clone());
        }
        if let Some(e) = &self.last_error {
            push("最后一次连接", e.clone());
        }
        push(
            "客户端",
            format!(
                "{}（协议 v{}）",
                self.client_version, self.client_protocol_major
            ),
        );
        crate::store::Diagnostics {
            reason,
            lines,
            newer_schema: newer,
        }
    }
}

fn phase_text(p: &Phase) -> String {
    match p {
        Phase::Starting { step } => format!("正在启动：{step}"),
        Phase::Ready => "就绪".into(),
        Phase::Stopping { reason } => format!("正在停止（{reason:?}）"),
        Phase::Refused(_) => "拒绝启动".into(),
        Phase::Exited { code, reason, .. } => match reason {
            Some(r) => format!("已退出（{code:?}，{r:?}）"),
            None => format!("已退出（{code:?}）"),
        },
    }
}

/// 2.2 角色表「客户端按需拉起」一列。读不到状态 → 拉；`NotWithThisProgram` 且程序没换过 → 不自动拉（留手动重试）。
pub fn should_launch(status: Option<&StartupStatus>, disk_exe: Option<ExeStamp>) -> bool {
    let Some(s) = status else { return true };
    match relaunchable(&s.phase) {
        Relaunchable::Yes | Relaunchable::NotUntilUserReturns => true,
        Relaunchable::NotWithThisProgram => disk_exe != Some(s.exe_stamp),
    }
}

type Attempt = JoinHandle<Result<(Client, Incoming), ConnectError>>;

/// 连接或拉起。
pub struct Launcher<C: Connect, S: Spawn> {
    connect: Arc<C>,
    spawner: S,
    cfg: LaunchConfig,
    /// 还没出结果的那次连接尝试。
    pending: Option<Attempt>,
    last_launch: Option<Instant>,
}

enum Waited {
    Done(Result<(Client, Incoming), ConnectError>),
    Deadline,
}

impl<C: Connect, S: Spawn> Launcher<C, S> {
    pub fn new(connect: C, spawner: S, cfg: LaunchConfig) -> Self {
        Self {
            connect: Arc::new(connect),
            spawner,
            cfg,
            pending: None,
            last_launch: None,
        }
    }

    fn diag(
        &self,
        stage: &'static str,
        last: Option<String>,
        spawn: Option<String>,
    ) -> LaunchError {
        LaunchError::NotReady(Box::new(diagnose(
            stage,
            self.cfg.run_dir.as_deref(),
            self.cfg.endpoint.clone(),
            last,
            spawn,
        )))
    }

    /// 等这次尝试（没有就开一次）出结果，或者到 `deadline`。到点不取消，留给下一次调用接着等。
    async fn wait_attempt(&mut self, deadline: Instant) -> Waited {
        let mut attempt = match self.pending.take() {
            Some(a) => a,
            None => tokio::spawn(self.connect.connect()),
        };
        match tokio::time::timeout_at(deadline, &mut attempt).await {
            Ok(Ok(r)) => Waited::Done(r),
            Ok(Err(join)) => Waited::Done(Err(ConnectError::Protocol(format!(
                "连接尝试异常结束：{join}"
            )))),
            Err(_) => {
                self.pending = Some(attempt);
                Waited::Deadline
            }
        }
    }

    /// 连接；连不上（且只有连不上）就拉起后台再连。从调用到返回不超过预算（外加一次调度的误差）。
    pub async fn connect_or_launch(&mut self) -> Result<(Client, Incoming), LaunchError> {
        self.run(false).await
    }

    /// 诊断页的「重试」：忽略「同一个程序已经说过起不来」，照样拉一次。
    pub async fn retry_now(&mut self) -> Result<(Client, Incoming), LaunchError> {
        self.run(true).await
    }

    async fn run(&mut self, force: bool) -> Result<(Client, Incoming), LaunchError> {
        let deadline = Instant::now() + self.cfg.budget;
        let err = match self.wait_attempt(deadline).await {
            Waited::Done(Ok(c)) => return Ok(c),
            Waited::Done(Err(e)) => e,
            Waited::Deadline => return Err(self.diag("连接中", None, None)),
        };
        let ConnectError::Io(io) = err else {
            return Err(LaunchError::Connect(err));
        };
        let mut last = io.to_string();
        // 第 3 步：照角色表决定拉不拉
        let status = self
            .cfg
            .run_dir
            .as_deref()
            .and_then(rendezvous::read_status);
        let disk = ExeStamp::of(&self.cfg.daemon_exe).ok();
        if !force && !should_launch(status.as_ref(), disk) {
            return Err(self.diag("同一个程序已经说过起不来", Some(last), None));
        }
        // 第 4 步：拉起（同一个 Launcher 两次至少隔 2s）
        let now = Instant::now();
        if self
            .last_launch
            .is_none_or(|t| now.duration_since(t) >= LAUNCH_GAP)
        {
            self.last_launch = Some(now);
            if let Err(e) = self.spawner.spawn(&self.cfg.daemon_exe, &self.cfg.args) {
                return Err(self.diag(
                    "拉起失败",
                    Some(last),
                    Some(format!("{}：{e}", self.cfg.daemon_exe.display())),
                ));
            }
        }
        // 第 5 步：退避重连，每一处等待都以同一个截止时间为上界
        let mut delay = FIRST_RETRY;
        loop {
            let wake = (Instant::now() + delay).min(deadline);
            tokio::time::sleep_until(wake).await;
            if Instant::now() >= deadline {
                return Err(self.diag("等后台就绪", Some(last), None));
            }
            match self.wait_attempt(deadline).await {
                Waited::Done(Ok(c)) => return Ok(c),
                Waited::Done(Err(ConnectError::Io(e))) => last = e.to_string(),
                Waited::Done(Err(e)) => return Err(LaunchError::Connect(e)),
                Waited::Deadline => return Err(self.diag("等后台就绪", Some(last), None)),
            }
            delay *= 2;
        }
    }
}

/// 本机用户的 `Launcher` 配置：后台在客户端同一目录（T03 2.4）。端点目录不安全或算不出来 → `Environment`。
pub fn local(
    own: &SelfIdentity,
    client: ClientConfig,
) -> Result<Launcher<IpcConnect, DetachedSpawn>, LaunchError> {
    let ep =
        LocalEndpoint::for_current_user().map_err(|e| LaunchError::Environment(e.to_string()))?;
    Ok(for_endpoint(ep, own, client, own.exe_dir.join(DAEMON_FILE)))
}

/// 指定 run 目录（测试与开发工具用）。目录不安全 → `Environment`（S14：不自动修，诊断页显示路径与权限）。
#[cfg(unix)]
pub fn for_run_dir(
    run_dir: &Path,
    own: &SelfIdentity,
    client: ClientConfig,
    daemon_exe: PathBuf,
) -> Result<Launcher<IpcConnect, DetachedSpawn>, LaunchError> {
    let ep =
        LocalEndpoint::in_run_dir(run_dir).map_err(|e| LaunchError::Environment(e.to_string()))?;
    Ok(for_endpoint(ep, own, client, daemon_exe))
}

/// 指定端点（测试与开发工具用）。
pub fn for_endpoint(
    ep: LocalEndpoint,
    own: &SelfIdentity,
    client: ClientConfig,
    daemon_exe: PathBuf,
) -> Launcher<IpcConnect, DetachedSpawn> {
    let endpoint = ep.endpoint().clone();
    let shown = match &endpoint {
        Endpoint::Unix(p) => Ok(p.clone()),
        other => Err(format!("{other:?}")),
    };
    #[allow(unused_mut)]
    let mut args = Vec::new();
    #[cfg(feature = "dev")]
    if client.trust == iwork_ipc::peer::TrustPolicy::DevTrust {
        args.push("--dev-trust");
    }
    Launcher::new(
        IpcConnect {
            endpoint,
            own: own.clone(),
            cfg: client,
        },
        DetachedSpawn,
        LaunchConfig {
            daemon_exe,
            budget: BUDGET,
            run_dir: ep.run_dir().map(Path::to_path_buf),
            endpoint: shown,
            args,
        },
    )
}

/// 后台程序的文件名。
pub const DAEMON_FILE: &str = if cfg!(windows) {
    "iwork-daemon.exe"
} else {
    "iwork-daemon"
};

impl<C: Connect, S: Spawn> Dialer for Launcher<C, S> {
    async fn dial(&mut self) -> Result<(Client, Incoming), DialError> {
        self.connect_or_launch().await.map_err(|e| match e {
            LaunchError::Connect(c) => DialError::Connect(c),
            LaunchError::NotReady(d) => DialError::NotReady(d.to_view()),
            LaunchError::Environment(m) => DialError::Environment(m),
        })
    }
}

/// 客户端配置的便利构造（桌面客户端）。
pub fn desktop_config(client_version: &str) -> ClientConfig {
    ClientConfig::new(pb::ClientKind::Desktop, client_version)
}
