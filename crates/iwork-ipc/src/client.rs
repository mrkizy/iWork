//! 客户端（方案 2.11）：一条连接的生命周期——建连、校验服务端、握手、收发；断了就结束，重连交给上层（T09）。
//!
//! # 客户端契约（T09 按它实现）
//!
//! - **只从 [`Incoming`] 这一个出口写状态库**：持久事件、会写视图的查询结果（[`IncomingItem::ViewResult`]）、
//!   订阅快照（[`IncomingItem::Subscribed`]）、服务端主动的错误，按到达顺序逐个应用；到达顺序就是服务端的全序。
//!   [`Client::view_query`] / [`Client::subscribe`] 的 future 只说完成或失败，不带数据。按实体 id 幂等写入，
//!   不需要实体版本，不比 seq。
//! - 快照替换**全部活动范围的视图**（成员 + 内容），**不删除**实体表里没出现在快照中的实体；分页查询结果同样
//!   只更新视图成员。实体的删除只由事件触发。
//! - 客户端记一个数：流游标 = 最近一次**已应用**的持久事件 seq、`snapshot_seq` 或 `ViewResult` 的 seq。
//!   连接因任何原因结束（`Incoming` 给出 [`IncomingItem::Closed`]）就重连，把全部范围**一次写出**
//!   `Subscribe{from_seq: 游标}`；在飞的命令按原 `command_id` 重发（幂等）。
//! - `from_seq = 游标` 的含义是「全部范围的视图都齐到游标」：有范围还没收到过它的 `Subscribed`（订阅还在路上就断了）时，
//!   重连必须用 `from_seq = 0`，否则这个范围永远拿不到快照。改范围的请求用 `subscribe_nowait` / `unsubscribe_nowait`
//!   按调用顺序写出，不要放进各自的任务里乱序发送。
//! - 同一游标第二次被关（续传没送出任何事件又断了）就改用 `from_seq = 0` 拿快照；连续 3 次同游标失败提示
//!   「后台负载过高」，继续退避重连。
//! - 收到的事件若没有任何活动范围对应（退订后排着的），丢掉。
//! - 升级窗口：[`ConnectError::PeerRejected`] 与 [`ConnectError::ClosedBeforeWelcome`] 是同一个状态——显示
//!   [`WaitPrompt::VersionMismatch`]，指数退避重连（[`Backoff`]），每次仍判定对端；不从错误变体推断谁新谁旧，
//!   不接受陌生对端，不结束任何进程。[`ConnectError::VersionMismatch`] 显示 [`WaitPrompt::SwitchingVersion`]。

use std::collections::HashMap;
use std::pin::Pin;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll};
use std::time::Duration;

use bytes::BytesMut;
use iwork_proto::limits::{MAX_CHUNK_DATA, MAX_FRAME_LEN};
use iwork_proto::pb::{self, ErrorCode};
use iwork_proto::routing::{QueryLane, query_lane};
use tokio::io::AsyncWriteExt;
use tokio::sync::{Semaphore, mpsc, oneshot};
use tokio_util::codec::FramedRead;

use crate::audit::{self, Role};
use crate::frame::{CodecError, Direction, Frame, FrameCodec, encode_frame};
use crate::peer::{PeerVerdict, RejectReason, SelfIdentity, TrustPolicy, judge_peer};
use crate::transport::identify::identify_by;
use crate::transport::{Connection, Connector, Endpoint};

/// 客户端配置。
#[derive(Debug, Clone)]
pub struct ClientConfig {
    pub client_kind: pb::ClientKind,
    pub client_version: String,
    pub protocol_major: u32,
    pub protocol_minor: u32,
    pub handshake_timeout: Duration,
    /// 发布构建恒为 `RequireSigner`。
    pub trust: TrustPolicy,
    /// `Incoming` 的容量，按编码字节计。满了读任务停止读 socket，由服务端预算接手。默认 4MiB。
    pub incoming_limit: usize,
}

impl ClientConfig {
    pub fn new(client_kind: pb::ClientKind, client_version: impl Into<String>) -> Self {
        Self {
            client_kind,
            client_version: client_version.into(),
            protocol_major: iwork_proto::PROTOCOL_MAJOR,
            protocol_minor: 0,
            handshake_timeout: Duration::from_secs(5),
            trust: TrustPolicy::RequireSigner,
            incoming_limit: 4 * 1024 * 1024,
        }
    }
}

/// 建连失败。
#[derive(Debug, thiserror::Error)]
pub enum ConnectError {
    /// 连不上：后台没起（T04 据此拉起）。
    #[error("连不上后台服务：{0}")]
    Io(#[source] std::io::Error),
    /// 我们拒绝了对端（升级窗口里的版本不一致，或陌生进程）。
    #[error("后台服务身份校验未通过：{0}")]
    PeerRejected(RejectReason),
    /// 对端在 Welcome 之前关了连接（对端拒绝了我们）。
    #[error("后台服务在握手完成前关闭了连接")]
    ClosedBeforeWelcome,
    #[error("协议主版本不一致：客户端 v{client_major}，后台服务 v{server_major}")]
    VersionMismatch {
        client_major: u32,
        server_major: u32,
    },
    #[error("握手超时")]
    Timeout,
    /// 对端在握手阶段发了不该发的东西。
    #[error("握手协议错误：{0}")]
    Protocol(String),
}

/// 升级窗口里给用户看的等待提示（方案 2.4c）。文案只在这里写一次。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WaitPrompt {
    /// 中性：分不出谁新谁旧，也分不出是不是连到了别的东西。
    VersionMismatch,
    /// 主版本不同。
    SwitchingVersion,
}

impl WaitPrompt {
    pub fn text(self) -> &'static str {
        match self {
            WaitPrompt::VersionMismatch => "后台服务与客户端版本不一致",
            WaitPrompt::SwitchingVersion => "正在切换版本",
        }
    }

    /// 提示上的出口。
    pub fn exits(self) -> &'static [&'static str] {
        match self {
            WaitPrompt::VersionMismatch => &["重新打开 iWork", "查看诊断"],
            WaitPrompt::SwitchingVersion => &["查看诊断"],
        }
    }
}

impl ConnectError {
    /// 这个错误该显示哪种等待提示；`None` 表示不是升级窗口的问题（如后台没起）。
    pub fn wait_prompt(&self) -> Option<WaitPrompt> {
        match self {
            ConnectError::PeerRejected(_) | ConnectError::ClosedBeforeWelcome => {
                Some(WaitPrompt::VersionMismatch)
            }
            ConnectError::VersionMismatch { .. } => Some(WaitPrompt::SwitchingVersion),
            ConnectError::Io(_) | ConnectError::Timeout | ConnectError::Protocol(_) => None,
        }
    }
}

/// 重连退避：200ms 起、翻倍、上限 5s、不设总时长。
#[derive(Debug, Clone)]
pub struct Backoff {
    next: Duration,
}

impl Backoff {
    pub const INITIAL: Duration = Duration::from_millis(200);
    pub const MAX: Duration = Duration::from_secs(5);

    pub fn new() -> Self {
        Self {
            next: Self::INITIAL,
        }
    }

    /// 下一次等多久。
    pub fn next_delay(&mut self) -> Duration {
        let d = self.next;
        self.next = (self.next * 2).min(Self::MAX);
        d
    }
}

impl Default for Backoff {
    fn default() -> Self {
        Self::new()
    }
}

/// 请求失败。
#[derive(Debug, Clone, PartialEq, thiserror::Error)]
pub enum ClientError {
    #[error("连接已关闭")]
    Closed,
    #[error("后台服务返回错误：{}", .0.message)]
    Server(pb::Error),
    /// 调用方用错了入口：会写视图的查询要用 `view_query`，订阅要用 `subscribe`。
    #[error("这类请求不能走这个入口")]
    WrongLane,
    #[error("分片超过 1MiB")]
    ChunkTooLarge,
    #[error("响应类型与请求不符")]
    UnexpectedResponse,
}

/// `Incoming` 结束的原因。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CloseReason {
    /// 对端关闭。
    PeerClosed,
    /// 读 socket 出错。
    Io(String),
    /// 服务端发来解不出的帧。
    Protocol(String),
}

/// `Incoming` 里的一项。
#[derive(Debug, Clone, PartialEq)]
pub enum IncomingItem {
    Event(Box<pb::Event>),
    ViewResult {
        request_id: u64,
        // 装箱：查询结果比其他项大得多（对话页带工具调用、审批等），不让每一项都按它占位。
        result: Box<pb::QueryResult>,
    },
    Subscribed {
        request_id: u64,
        subscribed: pb::Subscribed,
    },
    /// 服务端主动的错误（`Response{request_id: 0}`）。
    ServerError(pb::Error),
    Closed(CloseReason),
}

/// 客户端唯一的有序出口。按线上字节有界：满了读任务停止读 socket。
///
/// 每一项按产生它的那一帧的完整线上长度计费，至少 1；单项超过 `incoming_limit` 时按整个上限计费——
/// 要等 `Incoming` 清空才放行、独占全部配额。所以占用的上界是 `max(incoming_limit, 单帧上限 4MiB)`。
/// 丢弃 `Incoming` 会关闭配额：读任务随即退出，在飞的请求都以 `ClientError::Closed` 结束。
pub struct Incoming {
    rx: mpsc::UnboundedReceiver<(IncomingItem, u32)>,
    space: Arc<Semaphore>,
}

impl Drop for Incoming {
    fn drop(&mut self) {
        // 读任务可能正停在等配额上：关闭配额让它立刻醒来并退出，不靠对端关连接。
        // 停在等 socket 上的情况由 rx 随之丢弃唤醒（read_loop 里的 incoming.closed()）。
        self.space.close();
    }
}

/// 一项占多少配额（见 [`Incoming`] 的计费规则）。
fn incoming_charge(wire_len: usize, limit: u32) -> u32 {
    u32::try_from(wire_len).unwrap_or(u32::MAX).clamp(1, limit)
}

impl Incoming {
    pub async fn next(&mut self) -> Option<IncomingItem> {
        std::future::poll_fn(|cx| self.poll_item(cx)).await
    }

    fn poll_item(&mut self, cx: &mut Context<'_>) -> Poll<Option<IncomingItem>> {
        match self.rx.poll_recv(cx) {
            Poll::Ready(Some((item, permits))) => {
                self.space.add_permits(permits as usize);
                Poll::Ready(Some(item))
            }
            Poll::Ready(None) => Poll::Ready(None),
            Poll::Pending => Poll::Pending,
        }
    }
}

impl futures_core::Stream for Incoming {
    type Item = IncomingItem;

    fn poll_next(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<IncomingItem>> {
        self.get_mut().poll_item(cx)
    }
}

/// 写进 `Incoming` 的一端：先按字节拿配额再入队，拿不到就等（读任务因此停读）。
struct IncomingTx {
    tx: mpsc::UnboundedSender<(IncomingItem, u32)>,
    space: Arc<Semaphore>,
    limit: u32,
}

impl IncomingTx {
    /// 按 `wire_len` 拿配额后入队。消费端已丢弃时返回 false，读任务应当退出。
    async fn push(&self, item: IncomingItem, wire_len: usize) -> bool {
        let need = incoming_charge(wire_len, self.limit);
        match self.space.acquire_many(need).await {
            Ok(p) => p.forget(),
            Err(_) => return false,
        }
        self.tx.send((item, need)).is_ok()
    }

    /// 消费端还在吗。不写入 `Incoming` 的响应（`Direct`、`Ack`）用它判断读任务该不该继续。
    fn is_open(&self) -> bool {
        !self.tx.is_closed()
    }

    /// 消费端丢掉 `Incoming` 时完成。读任务等 socket 时和它一起等。
    async fn closed(&self) {
        self.tx.closed().await;
    }

    /// 流结束的最后一项：不占配额（每条连接只有一项），消费端不在了就算了。
    fn push_closed(&self, reason: CloseReason) {
        let _ = self.tx.send((IncomingItem::Closed(reason), 0));
    }
}

enum Pending {
    /// 命令、并行查询：Response 交给 future。
    Direct(oneshot::Sender<Result<pb::Response, ClientError>>),
    /// 会写视图的查询、订阅：载荷进 Incoming，future 只说成败。
    Ordered(oneshot::Sender<Result<(), ClientError>>),
    /// 退订：只说成败。
    Ack(oneshot::Sender<Result<(), ClientError>>),
}

/// 读任务与 `Client` 共享的部分。写端不在这里：`Client` 被丢掉时写端随之关闭，连接才会结束。
struct Inner {
    pending: Mutex<Option<HashMap<u64, Pending>>>,
    next_id: AtomicU64,
}

impl Inner {
    fn register(&self, p: Pending) -> Option<u64> {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let mut guard = self.pending.lock().expect("pending 锁中毒");
        guard.as_mut()?.insert(id, p);
        Some(id)
    }

    fn take(&self, id: u64) -> Option<Pending> {
        self.pending
            .lock()
            .expect("pending 锁中毒")
            .as_mut()?
            .remove(&id)
    }

    /// 连接结束：之后的请求立即失败，在飞的全部以 `Closed` 结束。
    fn close_all(&self) {
        let pending = self.pending.lock().expect("pending 锁中毒").take();
        for p in pending.into_iter().flat_map(HashMap::into_values) {
            match p {
                Pending::Direct(tx) => {
                    let _ = tx.send(Err(ClientError::Closed));
                }
                Pending::Ordered(tx) | Pending::Ack(tx) => {
                    let _ = tx.send(Err(ClientError::Closed));
                }
            }
        }
    }
}

/// 一条已握手的连接。
pub struct Client {
    inner: Arc<Inner>,
    out: mpsc::UnboundedSender<BytesMut>,
    welcome: pb::Welcome,
}

impl Client {
    /// 连接 → 校验服务端（同一个 `judge_peer`，两步都查）→ 握手。校验不过不发 `Hello`。
    pub async fn connect<C: Connector>(
        connector: &C,
        endpoint: &Endpoint,
        own: &SelfIdentity,
        cfg: ClientConfig,
    ) -> Result<(Client, Incoming), ConnectError> {
        let conn = connector
            .connect(endpoint)
            .await
            .map_err(ConnectError::Io)?;
        // 识别对端和读 Welcome 共用一个截止时间：识别（macOS 上是全量校验对端映像）本身没有上限，慢机器上卡住时
        // 会话会一直停在建连里。超时算失败、不放行，通信已被关掉（`Timeout` → 诊断页、退避重拨）。
        let deadline = tokio::time::Instant::now() + cfg.handshake_timeout;
        let peer = match identify_by(&conn, deadline).await {
            Err(_) => {
                audit::peer_identity_failed(
                    Role::Client,
                    &format_args!("识别对端超时（{:?}）", cfg.handshake_timeout),
                );
                return Err(ConnectError::Timeout);
            }
            Ok(Ok(p)) => p,
            Ok(Err(e)) => {
                audit::peer_identity_failed(Role::Client, &e);
                return Err(ConnectError::PeerRejected(RejectReason::Unavailable {
                    reason: e.to_string(),
                }));
            }
        };
        match judge_peer(own, &peer, cfg.trust) {
            PeerVerdict::Rejected(reason) => {
                audit::peer_rejected(Role::Client, &peer, &reason);
                return Err(ConnectError::PeerRejected(reason));
            }
            PeerVerdict::Accepted {
                signature_checked: _signature_checked,
            } =>
            {
                #[cfg(feature = "dev")]
                if !_signature_checked {
                    audit::peer_accepted_without_signature_check(Role::Client, &peer);
                }
            }
        }

        let (rd, mut wr) = conn.into_split();
        let hello = Frame::Hello(pb::Hello {
            protocol_major: cfg.protocol_major,
            protocol_minor: cfg.protocol_minor,
            client_kind: cfg.client_kind.into(),
            client_version: cfg.client_version.clone(),
            resume_token: None,
        });
        let mut buf = BytesMut::new();
        encode_frame(&hello, &mut buf);
        if wr.write_all(&buf).await.is_err() || wr.flush().await.is_err() {
            return Err(ConnectError::ClosedBeforeWelcome);
        }
        let mut reader = FramedRead::new(rd, FrameCodec::new(Direction::ToClient, MAX_FRAME_LEN));
        let first = tokio::time::timeout_at(deadline, next_frame(&mut reader))
            .await
            .map_err(|_| ConnectError::Timeout)?;
        let welcome = match first {
            None | Some(Err(CodecError::Io(_))) => return Err(ConnectError::ClosedBeforeWelcome),
            Some(Err(CodecError::Frame(e))) => return Err(ConnectError::Protocol(e.to_string())),
            Some(Ok(Frame::Welcome(w))) => w,
            Some(Ok(Frame::Response(r))) => return Err(handshake_refusal(*r, cfg.protocol_major)),
            Some(Ok(other)) => {
                return Err(ConnectError::Protocol(format!(
                    "握手时收到 {}",
                    other.frame_type().as_str_name()
                )));
            }
        };
        // 两道门各守各的：服务端的握手门之外，客户端再比一次主版本。
        if welcome.protocol_major != cfg.protocol_major {
            return Err(ConnectError::VersionMismatch {
                client_major: cfg.protocol_major,
                server_major: welcome.protocol_major,
            });
        }

        let (out_tx, mut out_rx) = mpsc::unbounded_channel::<BytesMut>();
        tokio::spawn(async move {
            while let Some(buf) = out_rx.recv().await {
                if wr.write_all(&buf).await.is_err() || wr.flush().await.is_err() {
                    break;
                }
            }
            let _ = wr.shutdown().await;
        });
        let inner = Arc::new(Inner {
            pending: Mutex::new(Some(HashMap::new())),
            next_id: AtomicU64::new(1),
        });
        let limit = u32::try_from(cfg.incoming_limit).unwrap_or(u32::MAX).max(1);
        let space = Arc::new(Semaphore::new(limit as usize));
        let (in_tx, in_rx) = mpsc::unbounded_channel();
        let incoming_tx = IncomingTx {
            tx: in_tx,
            space: Arc::clone(&space),
            limit,
        };
        tokio::spawn(read_loop(reader, Arc::clone(&inner), incoming_tx));
        let client = Client {
            inner,
            out: out_tx,
            welcome,
        };
        Ok((client, Incoming { rx: in_rx, space }))
    }

    pub fn welcome(&self) -> &pb::Welcome {
        &self.welcome
    }

    /// 命令与不写视图的查询（`ReadArtifact`、`ListModels`）：future 带回 `Response`。
    pub async fn request(&self, body: pb::request::Body) -> Result<pb::Response, ClientError> {
        match &body {
            pb::request::Body::Command(_) => {}
            pb::request::Body::Query(q)
                if q.kind.as_ref().map(query_lane) == Some(QueryLane::SideQuery) => {}
            _ => return Err(ClientError::WrongLane),
        }
        let (tx, rx) = oneshot::channel();
        self.send_request(Pending::Direct(tx), body)?;
        rx.await.unwrap_or(Err(ClientError::Closed))
    }

    /// 会写视图的查询：future 只说完成 / 失败；结果以 [`IncomingItem::ViewResult`] 从 `Incoming` 按到达顺序出来。
    /// 结果先排进 `Incoming` 再唤醒 future，所以等这个 future 时必须有人在读 `Incoming`（`Incoming` 满了读任务会停）。
    pub async fn view_query(&self, query: pb::Query) -> Result<(), ClientError> {
        if query.kind.as_ref().map(query_lane) != Some(QueryLane::ViewQuery) {
            return Err(ClientError::WrongLane);
        }
        self.ordered(pb::request::Body::Query(query)).await
    }

    /// 立即写出一个会写视图的查询，返回它的 `request_id`（结果在 `Incoming` 里以同一个 id 出现）与等成败的接收端。
    /// 与 `subscribe_nowait` / `unsubscribe_nowait` 一起用时，帧按调用顺序发出。
    pub fn view_query_nowait(
        &self,
        query: pb::Query,
    ) -> Result<(u64, oneshot::Receiver<Result<(), ClientError>>), ClientError> {
        if query.kind.as_ref().map(query_lane) != Some(QueryLane::ViewQuery) {
            return Err(ClientError::WrongLane);
        }
        let (tx, rx) = oneshot::channel();
        let id = self.send_request(Pending::Ordered(tx), pb::request::Body::Query(query))?;
        Ok((id, rx))
    }

    /// 订阅：`Subscribed`（可能带快照）以 [`IncomingItem::Subscribed`] 从 `Incoming` 出来，future 只说完成 / 失败。
    pub async fn subscribe(
        &self,
        id: u32,
        scope: pb::SubscriptionScope,
        from_seq: u64,
    ) -> Result<(), ClientError> {
        self.ordered(pb::request::Body::Subscribe(pb::Subscribe {
            subscription_id: id,
            scope: Some(scope),
            from_seq,
        }))
        .await
    }

    /// 只把订阅请求写出去，不等结果（重连时一次写出全部 `Subscribe` 用）。
    pub fn subscribe_nowait(
        &self,
        id: u32,
        scope: pb::SubscriptionScope,
        from_seq: u64,
    ) -> Result<oneshot::Receiver<Result<(), ClientError>>, ClientError> {
        let (tx, rx) = oneshot::channel();
        self.send_request(
            Pending::Ordered(tx),
            pb::request::Body::Subscribe(pb::Subscribe {
                subscription_id: id,
                scope: Some(scope),
                from_seq,
            }),
        )?;
        Ok(rx)
    }

    pub async fn unsubscribe(&self, id: u32) -> Result<(), ClientError> {
        self.unsubscribe_nowait(id)?
            .await
            .unwrap_or(Err(ClientError::Closed))
    }

    /// 立即写出退订请求，返回等结果的接收端。与 [`Client::subscribe_nowait`] 一起用时，帧按调用顺序发出。
    pub fn unsubscribe_nowait(
        &self,
        id: u32,
    ) -> Result<oneshot::Receiver<Result<(), ClientError>>, ClientError> {
        let (tx, rx) = oneshot::channel();
        self.send_request(
            Pending::Ack(tx),
            pb::request::Body::Unsubscribe(pb::Unsubscribe {
                subscription_id: id,
            }),
        )?;
        Ok(rx)
    }

    /// 分片上传。本地先查 1MiB 上限。
    pub async fn send_chunk(&self, chunk: pb::Chunk) -> Result<(), ClientError> {
        if chunk.data.len() > MAX_CHUNK_DATA {
            return Err(ClientError::ChunkTooLarge);
        }
        if self.send(&Frame::Chunk(chunk)) {
            Ok(())
        } else {
            Err(ClientError::Closed)
        }
    }

    async fn ordered(&self, body: pb::request::Body) -> Result<(), ClientError> {
        let (tx, rx) = oneshot::channel();
        self.send_request(Pending::Ordered(tx), body)?;
        rx.await.unwrap_or(Err(ClientError::Closed))
    }

    fn send(&self, frame: &Frame) -> bool {
        let mut buf = BytesMut::new();
        encode_frame(frame, &mut buf);
        self.out.send(buf).is_ok()
    }

    fn send_request(&self, p: Pending, body: pb::request::Body) -> Result<u64, ClientError> {
        let id = self.inner.register(p).ok_or(ClientError::Closed)?;
        let frame = Frame::Request(pb::Request {
            request_id: id,
            body: Some(body),
        });
        if self.send(&frame) {
            Ok(id)
        } else {
            self.inner.take(id);
            Err(ClientError::Closed)
        }
    }
}

fn handshake_refusal(r: pb::Response, client_major: u32) -> ConnectError {
    match r.body {
        Some(pb::response::Body::Error(e)) if e.code() == ErrorCode::ProtocolVersionMismatch => {
            let server_major = e
                .details
                .get(iwork_proto::handshake::DETAIL_SERVER_MAJOR)
                .and_then(|v| v.parse().ok())
                .unwrap_or(0);
            ConnectError::VersionMismatch {
                client_major,
                server_major,
            }
        }
        Some(pb::response::Body::Error(e)) => ConnectError::Protocol(e.message),
        _ => ConnectError::Protocol("握手时收到非错误的 Response".into()),
    }
}

async fn next_frame<R: tokio::io::AsyncRead + Unpin>(
    reader: &mut FramedRead<R, FrameCodec>,
) -> Option<Result<Frame, CodecError>> {
    std::future::poll_fn(|cx| futures_core::Stream::poll_next(Pin::new(&mut *reader), cx)).await
}

/// 唯一的读任务：按帧到达顺序分发，`Incoming` 满了就停在这里不读 socket。
/// 对端关闭、读出错、协议错误或 `Incoming` 被丢弃都会结束它，并让在飞的请求以 `Closed` 结束。
/// `Incoming` 被丢弃时读任务可能停在两处：等配额（`Incoming::drop` 关闭配额唤醒它），
/// 或等 socket（这里和 `incoming.closed()` 一起等）。两处都退到同一个收尾 `inner.close_all()`。
async fn read_loop<R: tokio::io::AsyncRead + Unpin>(
    mut reader: FramedRead<R, FrameCodec>,
    inner: Arc<Inner>,
    incoming: IncomingTx,
) {
    let reason = loop {
        let next = tokio::select! {
            biased;
            () = incoming.closed() => break None,
            next = next_frame(&mut reader) => next,
        };
        let frame = match next {
            None => break Some(CloseReason::PeerClosed),
            Some(Err(CodecError::Io(e))) => break Some(CloseReason::Io(e.to_string())),
            Some(Err(CodecError::Frame(e))) => break Some(CloseReason::Protocol(e.to_string())),
            Some(Ok(f)) => f,
        };
        let wire_len = frame.encoded_len();
        let consumer_alive = match frame {
            Frame::Event(e) => incoming.push(IncomingItem::Event(e), wire_len).await,
            Frame::Response(r) => dispatch_response(&inner, &incoming, *r, wire_len).await,
            other => {
                break Some(CloseReason::Protocol(format!(
                    "已建立后收到 {}",
                    other.frame_type().as_str_name()
                )));
            }
        };
        if !consumer_alive {
            break None;
        }
    };
    inner.close_all();
    if let Some(reason) = reason {
        incoming.push_closed(reason);
    }
}

/// 分发一个响应。返回 false 表示 `Incoming` 已被丢弃（读任务随即收尾，不管这一项走不走 `Incoming`）。
async fn dispatch_response(
    inner: &Inner,
    incoming: &IncomingTx,
    r: pb::Response,
    wire_len: usize,
) -> bool {
    use pb::response::Body;
    if r.request_id == 0 {
        return match r.body {
            Some(Body::Error(e)) => incoming.push(IncomingItem::ServerError(e), wire_len).await,
            _ => incoming.is_open(),
        };
    }
    let Some(p) = inner.take(r.request_id) else {
        tracing::warn!(target: "iwork_ipc", request_id = r.request_id, "收到没人等的响应");
        return incoming.is_open();
    };
    let rid = r.request_id;
    match p {
        Pending::Direct(tx) => {
            let _ = tx.send(match r.body {
                Some(Body::Error(e)) => Err(ClientError::Server(e)),
                _ => Ok(r),
            });
            incoming.is_open()
        }
        Pending::Ordered(tx) => {
            // 先把载荷排进 Incoming，再唤醒 future：future 完成时对应项已经排好。
            let item = match r.body {
                Some(Body::Error(e)) => {
                    let _ = tx.send(Err(ClientError::Server(e)));
                    return incoming.is_open();
                }
                Some(Body::Result(result)) => IncomingItem::ViewResult {
                    request_id: rid,
                    result: Box::new(result),
                },
                Some(Body::Subscribed(subscribed)) => IncomingItem::Subscribed {
                    request_id: rid,
                    subscribed,
                },
                _ => {
                    let _ = tx.send(Err(ClientError::UnexpectedResponse));
                    return incoming.is_open();
                }
            };
            let alive = incoming.push(item, wire_len).await;
            let _ = tx.send(if alive {
                Ok(())
            } else {
                Err(ClientError::Closed)
            });
            alive
        }
        Pending::Ack(tx) => {
            let _ = tx.send(match r.body {
                Some(Body::Error(e)) => Err(ClientError::Server(e)),
                _ => Ok(()),
            });
            incoming.is_open()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn incoming(limit: u32) -> (IncomingTx, Incoming) {
        let space = Arc::new(Semaphore::new(limit as usize));
        let (tx, rx) = mpsc::unbounded_channel();
        (
            IncomingTx {
                tx,
                space: Arc::clone(&space),
                limit,
            },
            Incoming { rx, space },
        )
    }

    fn ack() -> IncomingItem {
        IncomingItem::Subscribed {
            request_id: 1,
            subscribed: pb::Subscribed::default(),
        }
    }

    #[tokio::test]
    async fn zero_payload_items_still_take_quota() {
        // 默认 Subscribed 的载荷是 0 字节，但它的线上帧不是：配额满了就得等。
        let (tx, mut rx) = incoming(10);
        for _ in 0..10 {
            assert!(tx.push(ack(), 1).await);
        }
        let blocked =
            tokio::time::timeout(std::time::Duration::from_millis(50), tx.push(ack(), 1)).await;
        assert!(blocked.is_err(), "配额用完后不能再入队");
        rx.next().await.unwrap();
        assert!(tx.push(ack(), 1).await, "消费一项后释放配额");
    }

    #[test]
    fn charge_is_full_wire_length_at_least_one_capped_at_limit() {
        assert_eq!(incoming_charge(0, 100), 1);
        assert_eq!(incoming_charge(37, 100), 37);
        assert_eq!(incoming_charge(500, 100), 100);
    }

    #[tokio::test]
    async fn oversize_item_waits_for_empty_incoming_then_takes_whole_quota() {
        let (tx, mut rx) = incoming(100);
        assert!(tx.push(ack(), 10).await);
        // 超过上限的单项：要等 Incoming 清空。
        let big =
            tokio::time::timeout(std::time::Duration::from_millis(50), tx.push(ack(), 500)).await;
        assert!(big.is_err(), "Incoming 不空时超大单项不能放行");
        rx.next().await.unwrap();
        assert!(tx.push(ack(), 500).await, "清空后放行");
        let more =
            tokio::time::timeout(std::time::Duration::from_millis(50), tx.push(ack(), 1)).await;
        assert!(more.is_err(), "超大单项独占全部配额");
    }

    #[tokio::test]
    async fn dropping_incoming_releases_a_waiting_producer() {
        let (tx, rx) = incoming(1);
        assert!(tx.push(ack(), 1).await);
        let waiting = tokio::spawn(async move { tx.push(ack(), 1).await });
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        drop(rx);
        let pushed = tokio::time::timeout(std::time::Duration::from_secs(1), waiting)
            .await
            .expect("Incoming 丢弃后等配额的生产者应立刻醒来")
            .unwrap();
        assert!(!pushed, "消费端不在了，push 报告失败");
    }

    #[test]
    fn backoff_doubles_from_200ms_and_caps_at_5s() {
        let mut b = Backoff::new();
        let delays: Vec<u64> = (0..8).map(|_| b.next_delay().as_millis() as u64).collect();
        assert_eq!(delays, [200, 400, 800, 1600, 3200, 5000, 5000, 5000]);
    }

    #[test]
    fn both_rejections_map_to_the_same_neutral_prompt() {
        let a = ConnectError::PeerRejected(RejectReason::Unavailable { reason: "x".into() });
        let b = ConnectError::ClosedBeforeWelcome;
        assert_eq!(a.wait_prompt(), Some(WaitPrompt::VersionMismatch));
        assert_eq!(a.wait_prompt(), b.wait_prompt());
        assert_eq!(
            WaitPrompt::VersionMismatch.text(),
            "后台服务与客户端版本不一致"
        );
        assert_eq!(
            WaitPrompt::VersionMismatch.exits(),
            ["重新打开 iWork", "查看诊断"]
        );
        let v = ConnectError::VersionMismatch {
            client_major: 1,
            server_major: 2,
        };
        assert_eq!(v.wait_prompt(), Some(WaitPrompt::SwitchingVersion));
        assert_eq!(
            ConnectError::Io(std::io::ErrorKind::NotFound.into()).wait_prompt(),
            None
        );
    }
}
