//! hook 服务端：桥接这一侧的审批通道（T06 方案 2.4、4.1、4.4、4.6）。
//!
//! 一次工具调用一条连接、一问一答。每条连接按 2.4 的校验顺序走：
//!
//! 1. 绑定（启动 CLI 之前，[`HookServer::bind`]）：路径已存在就中止，不删、不复用（4.6 第 6 行）。
//! 2. accept 之后先验对端用户：不是自己直接关连接，不读不写（第 5 行）。
//! 3. 头在 [`HEADER_READ_LIMIT`] 内读完；令牌定长逐字节比较，头里的进程号要等于连接的对端进程号。
//!    不过 → 答拒绝（第 4 行），记一条审计日志，日志里没有令牌。
//! 4. 报到（`session_start`）答允许并发出 [`HookEvent::SessionStart`]；工具调用登记等待项、发出
//!    [`HookEvent::Call`]，等后台答复、兜底到时、hook 断开、回合结束四者之一（[`pending`]）。
//!
//! 期限：桥接的兜底（`deadlines().bridge`）从登记那一刻算，把系统睡眠也算进去（取单调时钟与墙钟的较大者，
//! 同 hook 的看门狗，4.3）。到时答 hook 拒绝并发出 [`HookEvent::Backstop`]，结束会话由调用方做（第 13 行）。
//!
//! M1 不支持 Windows（AGENTS.md）：命名管道这一半不在这里，整个模块只在 unix 上编译。

pub mod pending;

use std::io;
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use iwork_claude_hook::client::Failure;
use iwork_claude_hook::deadline::{Started, next_sleep};
use iwork_claude_hook::peer::{self, PeerCred};
use iwork_claude_hook::wire::{self, Decision, HookKind, PREFIX_BYTES, RequestHeader, Response};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{UnixListener, UnixStream};
use tokio::sync::{mpsc, oneshot};
use tokio::task::JoinSet;

use self::pending::{CallId, Pending};

/// 请求头（和正文）最多等多久读完（2.4 第 4 步）。
pub const HEADER_READ_LIMIT: Duration = Duration::from_secs(2);

/// accept 出错后隔多久再试。
const ACCEPT_RETRY: Duration = Duration::from_millis(100);

/// 令牌校验没过时回给模型的话（4.6 第 4 行）。
pub const REASON_TOKEN_REJECTED: &str = "iWork 审批通道校验失败，这次调用没有执行";

/// 审计日志的 target：通道上被拒的连接都记在这里，字段里没有令牌。
pub const AUDIT_TARGET: &str = "iwork_bridge_claude::hook_audit";

/// 每条会话一个令牌：32 字节随机数，只在内存里；经环境变量交给 CLI（2.4）。`Debug` 不打印内容。
pub struct Token([u8; wire::TOKEN_BYTES]);

impl Token {
    pub fn generate() -> io::Result<Self> {
        let mut bytes = [0u8; wire::TOKEN_BYTES];
        getrandom::fill(&mut bytes).map_err(|e| io::Error::other(format!("取随机数失败：{e}")))?;
        Ok(Self(bytes))
    }

    /// 测试用：给定内容。
    pub fn from_bytes(bytes: [u8; wire::TOKEN_BYTES]) -> Self {
        Self(bytes)
    }

    /// 线上与环境变量里的写法（64 位小写十六进制）。
    pub fn hex(&self) -> String {
        wire::token_hex(&self.0)
    }

    fn matches(&self, got: &str) -> bool {
        wire::token_matches(&self.0, got)
    }
}

impl std::fmt::Debug for Token {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Token(<redacted>)")
    }
}

/// 交给会话一侧的事。
#[derive(Debug)]
pub enum HookEvent {
    /// SessionStart hook 报到了（3.5）
    SessionStart,
    /// 一次工具调用在等审批。`input` 是 Claude Code 给 hook 的 stdin 原文；答复经 [`Pending::answer`] 给
    Call { id: CallId, input: Vec<u8> },
    /// 桥接兜底到时，已经答了 hook 拒绝：调用方对 prompt 回 `approval_backstop` 并结束会话（4.3、4.6 第 13 行）
    Backstop { id: CallId },
    /// 后台的允许交给了连接任务，但写给 hook 时连接已经断了：动作没执行而后台记了交付，按兜底处理（4.4）
    AllowUndelivered { id: CallId },
}

/// 绑定失败：开会话失败 `channel_failed`（4.6 第 6 行）。说明里点名路径。
#[derive(Debug)]
pub struct BindError(pub String);

impl std::fmt::Display for BindError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for BindError {}

/// 一条会话的 hook 通道。
pub struct HookServer {
    listener: UnixListener,
    ctx: Arc<Ctx>,
}

struct Ctx {
    token: Token,
    user: u32,
    /// 桥接兜底期限，从登记等待项算起
    backstop: Duration,
    pending: Arc<Pending>,
    events: mpsc::UnboundedSender<HookEvent>,
}

impl HookServer {
    /// 在会话目录里绑定 socket。路径已存在（哪怕是个普通文件）就失败，不删、不复用。
    /// 要在 tokio 运行时里调用。
    pub fn bind(
        socket: &Path,
        token: Token,
        backstop: Duration,
    ) -> Result<(Self, mpsc::UnboundedReceiver<HookEvent>), BindError> {
        crate::launch::session_dir::check_socket_len(socket).map_err(|e| BindError(e.0))?;
        // bind(2) 在路径已存在时报 EADDRINUSE，自己不删任何东西
        let listener = UnixListener::bind(socket).map_err(|e| {
            BindError(format!(
                "审批通道的 socket 绑定不了（{e}）：{}",
                socket.display()
            ))
        })?;
        let (events, rx) = mpsc::unbounded_channel();
        let ctx = Arc::new(Ctx {
            token,
            user: peer::current_user(),
            backstop,
            pending: Arc::new(Pending::default()),
            events,
        });
        Ok((Self { listener, ctx }, rx))
    }

    /// 等待项表：后台答复、回合结束经它回答。
    pub fn pending(&self) -> Arc<Pending> {
        Arc::clone(&self.ctx.pending)
    }

    /// 接连接，直到这个 future 被丢弃。连接任务挂在它名下，一起丢弃时连接全部断开（hook 读到 EOF 按拒绝处理）。
    pub async fn serve(self) {
        let mut conns = JoinSet::new();
        loop {
            tokio::select! {
                accepted = self.listener.accept() => match accepted {
                    Ok((stream, _)) => {
                        conns.spawn(handle(stream, Arc::clone(&self.ctx)));
                    }
                    Err(e) => {
                        tracing::warn!(target: AUDIT_TARGET, error = %e, "hook 通道 accept 失败");
                        // 文件描述符用完之类的错会一直报：歇一下，不空转
                        tokio::time::sleep(ACCEPT_RETRY).await;
                    }
                },
                // 收掉跑完的连接任务，表不无限长
                Some(_) = conns.join_next(), if !conns.is_empty() => {}
            }
        }
    }
}

/// 一条连接的全过程。哪一步不对都只影响这一条连接。
async fn handle(mut conn: UnixStream, ctx: Arc<Ctx>) {
    let peer = match peer::peer_cred(&conn) {
        Ok(p) => p,
        Err(e) => {
            tracing::warn!(target: AUDIT_TARGET, error = %e, "取不到 hook 连接的对端凭据，关闭连接");
            return;
        }
    };
    if peer.user != ctx.user {
        tracing::warn!(target: AUDIT_TARGET, peer_user = peer.user, peer_pid = peer.pid, "别的用户连进审批通道，关闭连接");
        return;
    }
    let header = match tokio::time::timeout(HEADER_READ_LIMIT, read_header(&mut conn)).await {
        Ok(Ok(h)) => h,
        Ok(Err(e)) => {
            tracing::warn!(target: AUDIT_TARGET, peer_pid = peer.pid, error = %e, "hook 请求头读不了，关闭连接");
            return;
        }
        Err(_) => {
            tracing::warn!(target: AUDIT_TARGET, peer_pid = peer.pid, "hook 请求头没在时限内读完，关闭连接");
            return;
        }
    };
    if let Err(why) = check_header(&ctx, &header, peer) {
        tracing::warn!(target: AUDIT_TARGET, peer_pid = peer.pid, kind = ?header.kind, why, "hook 请求校验没过，答拒绝");
        reply(&mut conn, &Response::deny(REASON_TOKEN_REJECTED)).await;
        return;
    }
    let body = match tokio::time::timeout(HEADER_READ_LIMIT, read_body(&mut conn)).await {
        Ok(Ok(b)) => b,
        Ok(Err(e)) => {
            tracing::warn!(target: AUDIT_TARGET, peer_pid = peer.pid, error = %e, "hook 请求正文读不了，关闭连接");
            return;
        }
        Err(_) => {
            tracing::warn!(target: AUDIT_TARGET, peer_pid = peer.pid, "hook 请求正文没在时限内读完，关闭连接");
            return;
        }
    };
    match header.kind {
        HookKind::SessionStart => {
            reply(&mut conn, &Response::allow()).await;
            let _ = ctx.events.send(HookEvent::SessionStart);
        }
        HookKind::PreToolUse => {
            let (id, answered) = ctx.pending.register();
            let _ = ctx.events.send(HookEvent::Call { id, input: body });
            if let Some(response) = wait(&mut conn, &ctx, id, answered).await {
                let written = reply(&mut conn, &response).await;
                if let Some(event) = undelivered(id, &response, written) {
                    let _ = ctx.events.send(event);
                }
            }
        }
    }
}

/// 答复写不出去时要不要报：只有允许要报（后台记了交付而动作没执行，4.4 第 2 条）；拒绝写不出去，hook 那边
/// 本来也是拒绝（读不到答复按拒绝处理）。这个窗口（答复到了、hook 恰好在写之前走了）在集成测试里造不出确定的
/// 次序，所以抽成这一个函数单测。
fn undelivered(id: CallId, response: &Response, written: bool) -> Option<HookEvent> {
    (!written && response.decision == Decision::Allow).then_some(HookEvent::AllowUndelivered { id })
}

/// 令牌对、进程号与连接的对端一致。返回审计用的原因（不含令牌）。
fn check_header(ctx: &Ctx, header: &RequestHeader, peer: PeerCred) -> Result<(), &'static str> {
    if !ctx.token.matches(&header.token) {
        return Err("令牌不符");
    }
    if header.pid != peer.pid {
        return Err("请求头里的进程号不是连接的对端");
    }
    Ok(())
}

/// 等这一项被回答。返回要写给 hook 的答复；hook 已经断开时返回 `None`（没处可写）。
async fn wait(
    conn: &mut UnixStream,
    ctx: &Ctx,
    id: CallId,
    mut answered: oneshot::Receiver<Response>,
) -> Option<Response> {
    let started = Started::now();
    let mut probe = [0u8; 1];
    tokio::select! {
        // 回答方：后台答复、回合结束、兜底（下面那一支 settle 之后也从这里出来）
        r = &mut answered => Some(r.unwrap_or_else(|_| Response::deny(Failure::Internal.reason()))),
        _ = backstop(started, ctx.backstop) => {
            if ctx.pending.settle(id, Response::deny(Failure::Deadline.reason())) {
                tracing::warn!(target: AUDIT_TARGET, call = id.0, "后台没在兜底期限内答复，桥接答拒绝");
                let _ = ctx.events.send(HookEvent::Backstop { id });
            }
            // 抢到了就是兜底的拒绝；没抢到说明同时有人答了，用那一条
            answered.await.ok()
        }
        // hook 发完请求就只等答复，这条连接上再读到东西（EOF 或多余字节）就是它走了或者不守协议
        _ = conn.read(&mut probe) => {
            ctx.pending.settle(id, Response::deny(Failure::Disconnected.reason()));
            tracing::warn!(target: AUDIT_TARGET, call = id.0, "hook 在等答复时断开了");
            None
        }
    }
}

/// 兜底期限：最多每秒醒一次，重新比两种时钟（睡眠期间单调时钟可能不走）。
async fn backstop(started: Started, deadline: Duration) {
    while let Some(nap) = next_sleep(started.elapsed(), deadline) {
        tokio::time::sleep(nap).await;
    }
}

async fn read_header(conn: &mut UnixStream) -> Result<RequestHeader, wire::WireError> {
    let mut prefix = [0u8; PREFIX_BYTES];
    conn.read_exact(&mut prefix).await?;
    let len = wire::request_header_len(&prefix)?;
    let mut json = vec![0u8; len];
    conn.read_exact(&mut json).await?;
    wire::decode_request_header(&json)
}

async fn read_body(conn: &mut UnixStream) -> Result<Vec<u8>, wire::WireError> {
    let mut len = [0u8; 4];
    conn.read_exact(&mut len).await?;
    let mut body = vec![0u8; wire::request_body_len(len)?];
    conn.read_exact(&mut body).await?;
    Ok(body)
}

/// 写一条答复，返回写没写成。
async fn reply(conn: &mut UnixStream, response: &Response) -> bool {
    let bytes = match wire::encode_response(response) {
        Ok(b) => b,
        Err(e) => {
            tracing::warn!(target: AUDIT_TARGET, error = %e, "答复编码不了，按拒绝写");
            match wire::encode_response(&Response::deny(Failure::BadResponse.reason())) {
                Ok(b) => b,
                Err(_) => return false,
            }
        }
    };
    conn.write_all(&bytes).await.is_ok() && conn.flush().await.is_ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_an_unwritten_allow_is_reported() {
        let id = CallId(7);
        assert!(matches!(
            undelivered(id, &Response::allow(), false),
            Some(HookEvent::AllowUndelivered { id: CallId(7) })
        ));
        assert!(undelivered(id, &Response::allow(), true).is_none());
        assert!(undelivered(id, &Response::deny("x"), false).is_none());
    }

    #[test]
    fn token_debug_is_redacted() {
        let t = Token::from_bytes([0xcd; wire::TOKEN_BYTES]);
        assert!(!format!("{t:?}").contains("cdcd"));
        assert_eq!(t.hex().len(), 64);
        assert!(t.matches(&t.hex()));
    }
}
