//! 开会话：从建会话目录到可以发第一条用户消息（T06 方案 3.3、3.5）。
//!
//! 次序（3.5，S0 实测后改定）：
//!
//! 1. 清扫上次被杀留下的会话目录，建这一条的目录；确认 hook 程序在而且可执行（3.2）。
//! 2. 生成令牌，算三个期限（4.3），`build_launch` 出计划，写设置与 MCP 配置（0600），绑定 hook 通道。
//! 3. 起 CLI，等 SessionStart 报到（[`STARTUP_LIMIT`]）。没来、或 CLI 先退出 → 开会话失败。
//! 4. 发 `initialize` 控制请求，等它的响应（同样 [`STARTUP_LIMIT`]），取 `current_permission_mode`：
//!    不是选定配置的 `dontAsk`、没有这个键、等不到 → `isolation_broken`（设置文件被整个忽略时 CLI 报 `default`）。
//! 5. 这两步都过了才返回；第一条用户消息由调用方之后再发。
//!
//! 哪一步失败都杀掉 CLI、停掉 hook 通道、删掉会话目录，不留东西；开会话的 future 被丢掉（调用方超时、请求被
//! 取消）也一样：会话目录自己负责删（`SessionDir` 的 Drop），CLI 带 `kill_on_drop`，hook 通道丢掉即停。
//!
//! 开会话期间 CLI 的每一行都先解析：不合协议（不是 JSON 对象、超过 8 MiB）就失败，和映射里一样不信任这条输出
//! （`cli_exited`）。合协议的别的行（`system` 事件等）原样留在 [`OpenSession::early_lines`]，由调用方交给映射；
//! 控制响应一律不留原文（里面可能有 `account`）：`initialize` 的只取权限模式这一个键，对不上编号的只记编号。

use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use tokio::sync::mpsc::UnboundedReceiver;
use tokio::time::Instant;

use crate::cli::events::{self, CliEvent, ControlResponse, LineError};
use crate::cli::facts::IsolationBroken;
use crate::cli::input::control_request_line;
use crate::cli::process::{CliProcess, ReadError};
use crate::deadlines::deadlines;
use crate::hook_server::pending::{BackendAnswer, Pending};
use crate::hook_server::{HookEvent, HookServer, Serving, Token};
use crate::launch::session_dir::{self, SessionDir};
use crate::launch::settings::SELECTED;
use crate::launch::{BridgeEnv, Channel, Opening, build_launch};
use crate::setup::{Setup, SetupInvalid};

/// 报到与 `initialize` 响应各自最多等多久（3.5）。
pub const STARTUP_LIMIT: Duration = Duration::from_secs(20);

/// 桥接发的 `initialize` 控制请求的编号。
pub const INITIALIZE_REQUEST_ID: &str = "iwork-initialize";

/// 开会话期间留下的 CLI 输出行最多几行；再多只计数（这时还没有用户消息，只会是 `system` 一类的事件）。
pub const MAX_EARLY_LINES: usize = 1000;

/// 开会话期间就有工具调用来问（不该发生：还没发过用户消息）时回给模型的话。
pub const REASON_NOT_OPEN: &str = "iWork 会话还没开好，这次调用没有执行";

/// 开会话要的东西。
#[derive(Debug)]
pub struct OpenRequest<'a> {
    /// 数据目录（后台经 `IWORK_DATA_DIR` 给，3.3）
    pub data_dir: &'a Path,
    pub setup: &'a Setup,
    pub opening: &'a Opening,
    pub bridge: &'a BridgeEnv,
    /// PreToolUse hook 的 timeout（产品里是 `HOOK_TIMEOUT_SECS`），三个期限由它算
    pub hook_timeout: Duration,
    /// 报到与 `initialize` 各自的上限（产品里是 [`STARTUP_LIMIT`]；测试调小）
    pub startup_limit: Duration,
}

/// 开好的会话：CLI、hook 通道、会话目录，交给 ACP 端接着用。
pub struct OpenSession {
    pub cli: CliProcess,
    /// hook 通道的事件（报到之后的工具调用、兜底、hook 断开）
    pub hook_events: UnboundedReceiver<HookEvent>,
    pub pending: Arc<Pending>,
    /// 丢掉即停 hook 通道
    pub serving: Serving,
    pub dir: SessionDir,
    /// 开会话期间 CLI 吐的、不是 `initialize` 响应的行，按到达次序
    pub early_lines: Vec<String>,
    /// 超出 [`MAX_EARLY_LINES`] 没留下的行数
    pub early_lines_dropped: usize,
}

/// 开会话失败。[`OpenError::code`] 是给后台的错误码（方案 2.6 第 7 条）。
#[derive(Debug)]
pub enum OpenError {
    /// 会话目录、设置文件、socket 建不起来，或 hook 程序不在（说明里点名路径）
    ChannelFailed(String),
    SetupInvalid(SetupInvalid),
    /// CLI 起不来
    Spawn(String),
    /// 报到没在时限内来
    HookInactive,
    /// 开会话期间 CLI 退出了
    CliExited {
        code: Option<i32>,
    },
    /// 权限模式不是选定配置的，或拿不到（3.5）
    IsolationBroken(IsolationBroken),
    /// CLI 输出了不合协议的行（不是 JSON 对象、超长）：不信任它的输出
    BadLine(LineError),
}

impl OpenError {
    pub fn code(&self) -> &'static str {
        match self {
            Self::ChannelFailed(_) => "channel_failed",
            Self::SetupInvalid(_) => "setup_invalid",
            Self::Spawn(_) | Self::CliExited { .. } | Self::BadLine(_) => "cli_exited",
            Self::HookInactive => "hook_inactive",
            Self::IsolationBroken(_) => "isolation_broken",
        }
    }
}

impl std::fmt::Display for OpenError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::ChannelFailed(why) => write!(f, "审批通道建立失败：{why}"),
            Self::SetupInvalid(e) => write!(f, "会话配置不对：{}", e.field),
            Self::Spawn(why) => write!(f, "CLI 起不来：{why}"),
            Self::HookInactive => f.write_str("CLI 没在时限内向审批通道报到"),
            Self::CliExited { code } => write!(f, "开会话期间 CLI 退出了（退出码 {code:?}）"),
            Self::IsolationBroken(e) => write!(f, "隔离失效：{e:?}"),
            Self::BadLine(e) => write!(f, "CLI 的输出不合协议：{e:?}"),
        }
    }
}

impl std::error::Error for OpenError {}

/// 开会话。失败时什么都不留（CLI 杀掉、通道停掉、目录删掉）。要在 tokio 运行时里调用。
pub async fn open(req: OpenRequest<'_>) -> Result<OpenSession, OpenError> {
    match session_dir::sweep(req.data_dir) {
        Ok(0) => {}
        Ok(n) => tracing::info!(removed = n, "清掉了上次留下的会话目录"),
        Err(e) => tracing::warn!(error = %e, "清扫会话目录失败，接着开会话"),
    }
    check_hook_program(&req.bridge.hook_program)?;
    let dir = SessionDir::create(req.data_dir).map_err(|e| OpenError::ChannelFailed(e.0))?;
    // 会话目录自己负责删（`SessionDir` 的 Drop）：出错返回、这个 future 被丢掉，都在这里删掉；成功时交给会话
    let parts = open_in(&dir, &req).await?;
    Ok(parts.into_session(dir))
}

/// hook 程序在、是普通文件、可执行（3.2）。不在的话报到永远不会来，提前说清楚。
fn check_hook_program(path: &str) -> Result<(), OpenError> {
    use std::os::unix::fs::PermissionsExt;
    let meta = std::fs::metadata(path)
        .map_err(|e| OpenError::ChannelFailed(format!("hook 程序读不了（{e}）：{path}")))?;
    if !meta.is_file() || meta.permissions().mode() & 0o111 == 0 {
        return Err(OpenError::ChannelFailed(format!(
            "hook 程序不是可执行文件：{path}"
        )));
    }
    Ok(())
}

/// 除会话目录以外开好的部分。
struct Parts {
    cli: CliProcess,
    hook_events: UnboundedReceiver<HookEvent>,
    pending: Arc<Pending>,
    serving: Serving,
    early: Early,
}

impl Parts {
    fn into_session(self, dir: SessionDir) -> OpenSession {
        OpenSession {
            cli: self.cli,
            hook_events: self.hook_events,
            pending: self.pending,
            serving: self.serving,
            dir,
            early_lines: self.early.lines,
            early_lines_dropped: self.early.dropped,
        }
    }
}

#[derive(Default)]
struct Early {
    lines: Vec<String>,
    dropped: usize,
}

impl Early {
    fn keep(&mut self, line: String) {
        if self.lines.len() < MAX_EARLY_LINES {
            self.lines.push(line);
        } else {
            self.dropped += 1;
        }
    }
}

async fn open_in(dir: &SessionDir, req: &OpenRequest<'_>) -> Result<Parts, OpenError> {
    let token = Token::generate().map_err(|e| OpenError::ChannelFailed(e.to_string()))?;
    let limits = deadlines(req.hook_timeout);
    let channel = Channel {
        dir: dir.path().to_path_buf(),
        token_hex: token.hex(),
        bridge_pid: std::process::id(),
        hook_deadline: limits.hook,
        hook_timeout: req.hook_timeout,
    };
    let plan = build_launch(req.setup, req.opening, req.bridge, &channel)
        .map_err(OpenError::SetupInvalid)?;
    let json = |v: &serde_json::Value| serde_json::to_vec_pretty(v).expect("JSON 值总能序列化");
    dir.write_config(&json(&plan.settings), &json(&plan.mcp))
        .map_err(|e| OpenError::ChannelFailed(e.0))?;
    let (server, mut hook_events) = HookServer::bind(&dir.files().socket, token, limits.bridge)
        .map_err(|e| OpenError::ChannelFailed(e.0))?;
    let pending = server.pending();
    let serving = server.spawn();
    let mut cli = CliProcess::spawn(&plan)
        .map_err(|e| OpenError::Spawn(format!("{e}（{}）", plan.program.display())))?;
    // 收尾线程要知道给谁发 SIGTERM（5.4a 第 5 条第二种）
    crate::reaper::register_cli(cli.id());
    let mut early = Early::default();
    let result = handshake(
        &mut cli,
        &mut hook_events,
        &pending,
        &mut early,
        req.startup_limit,
    )
    .await;
    match result {
        Ok(()) => Ok(Parts {
            cli,
            hook_events,
            pending,
            serving,
            early,
        }),
        Err(e) => {
            if let Err(kill) = cli.kill().await {
                tracing::warn!(error = %kill, "开会话失败后杀 CLI 也失败了");
            }
            Err(e)
        }
    }
}

/// 第 3、4 步：等报到，再发 `initialize` 核对权限模式。
async fn handshake(
    cli: &mut CliProcess,
    hook_events: &mut UnboundedReceiver<HookEvent>,
    pending: &Pending,
    early: &mut Early,
    limit: Duration,
) -> Result<(), OpenError> {
    // 报到
    let until = Instant::now() + limit;
    loop {
        tokio::select! {
            ev = hook_events.recv() => match ev {
                Some(HookEvent::SessionStart) => break,
                Some(other) => refuse_early(pending, other),
                None => return Err(OpenError::ChannelFailed("hook 通道提前停了".into())),
            },
            line = cli.next_line() => match line {
                Ok(Some(l)) => {
                    let _ = take_line(l, early)?;
                }
                Ok(None) | Err(ReadError::Io(_)) => return Err(exited(cli).await),
                Err(ReadError::Line(e)) => return Err(OpenError::BadLine(e)),
            },
            _ = tokio::time::sleep_until(until) => {
                tracing::warn!("CLI 没在时限内报到");
                return Err(OpenError::HookInactive);
            }
        }
    }
    // initialize：报到之后、第一条用户消息之前
    cli.write_line(&control_request_line(INITIALIZE_REQUEST_ID, "initialize"))
        .await
        .map_err(|_| OpenError::CliExited { code: None })?;
    let until = Instant::now() + limit;
    let response = loop {
        tokio::select! {
            line = cli.next_line() => match line {
                Ok(Some(l)) => {
                    if let Some(r) = take_line(l, early)?
                        && r.request_id.as_deref() == Some(INITIALIZE_REQUEST_ID)
                    {
                        break Some(r);
                    }
                }
                Ok(None) | Err(ReadError::Io(_)) => return Err(exited(cli).await),
                Err(ReadError::Line(e)) => return Err(OpenError::BadLine(e)),
            },
            ev = hook_events.recv() => match ev {
                Some(other) => refuse_early(pending, other),
                None => return Err(OpenError::ChannelFailed("hook 通道提前停了".into())),
            },
            _ = tokio::time::sleep_until(until) => break None,
        }
    };
    let reported = response
        .filter(|r| r.success)
        .and_then(|r| r.current_permission_mode);
    tracing::info!(permission_mode = ?reported, "initialize 响应里的权限模式");
    if reported.as_deref() != Some(SELECTED.default_mode) {
        return Err(OpenError::IsolationBroken(
            IsolationBroken::PermissionMode { reported },
        ));
    }
    Ok(())
}

/// 开会话期间读到的一行：先解析，不合协议就失败。控制响应只把解析出的字段交回（原文不留，里面可能有
/// `account`）；对不上编号的由调用方丢掉，这里记一笔编号。别的行留给映射。
fn take_line(line: String, early: &mut Early) -> Result<Option<ControlResponse>, OpenError> {
    match events::parse_line(&line).map_err(OpenError::BadLine)? {
        CliEvent::ControlResponse(r) => {
            if r.request_id.as_deref() != Some(INITIALIZE_REQUEST_ID) {
                tracing::warn!(request_id = ?r.request_id, "开会话期间来了对不上编号的控制响应，丢掉");
            }
            Ok(Some(r))
        }
        _ => {
            early.keep(line);
            Ok(None)
        }
    }
}

/// 开会话期间 hook 通道上来的别的事：工具调用答拒绝（还没发过用户消息，不该有），别的只记一笔。
fn refuse_early(pending: &Pending, event: HookEvent) {
    match event {
        HookEvent::Call { id, .. } => {
            tracing::warn!(call = id.0, "开会话期间来了工具调用，答拒绝");
            pending.answer(
                id,
                BackendAnswer::Reject {
                    message: REASON_NOT_OPEN.into(),
                },
            );
        }
        other => tracing::warn!(event = ?other, "开会话期间 hook 通道上的事件"),
    }
}

/// CLI 关了 stdout：等它退出，取退出码。
async fn exited(cli: &mut CliProcess) -> OpenError {
    let code = tokio::time::timeout(Duration::from_secs(5), cli.wait())
        .await
        .ok()
        .and_then(Result::ok)
        .and_then(|s| s.code());
    tracing::warn!(?code, "开会话期间 CLI 退出了");
    OpenError::CliExited { code }
}
