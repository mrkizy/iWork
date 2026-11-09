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
use prost::Message;
use tokio::io::AsyncWriteExt;
use tokio::sync::{Semaphore, mpsc, oneshot};
use tokio_util::codec::FramedRead;

use crate::audit::{self, Role};
use crate::frame::{CodecError, Direction, Frame, FrameCodec, encode_frame};
use crate::peer::{PeerVerdict, RejectReason, SelfIdentity, TrustPolicy, judge_peer};
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
#[expect(
    clippy::large_enum_variant,
    reason = "每条事件都经过它；装箱等于推送热路径上每条多一次分配"
)]
pub enum IncomingItem {
    Event(pb::Event),
    ViewResult {
        request_id: u64,
        result: pb::QueryResult,
    },
    Subscribed {
        request_id: u64,
        subscribed: pb::Subscribed,
    },
    /// 服务端主动的错误（`Response{request_id: 0}`）。
    ServerError(pb::Error),
    Closed(CloseReason),
}

impl IncomingItem {
    fn byte_len(&self) -> usize {
        match self {
            IncomingItem::Event(e) => e.encoded_len(),
            IncomingItem::ViewResult { result, .. } => result.encoded_len(),
            IncomingItem::Subscribed { subscribed, .. } => subscribed.encoded_len(),
            IncomingItem::ServerError(e) => e.encoded_len(),
            IncomingItem::Closed(_) => 0,
        }
    }
}

/// 客户端唯一的有序出口。按编码字节有界：满了读任务停止读 socket。
pub struct Incoming {
    rx: mpsc::UnboundedReceiver<(IncomingItem, u32)>,
    space: Arc<Semaphore>,
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
    async fn push(&self, item: IncomingItem) {
        let need = u32::try_from(item.byte_len())
            .unwrap_or(u32::MAX)
            .min(self.limit);
        if need > 0 {
            match self.space.acquire_many(need).await {
                Ok(p) => p.forget(),
                Err(_) => return,
            }
        }
        let _ = self.tx.send((item, need));
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
        let peer = match conn.peer().await {
            Ok(p) => p,
            Err(e) => {
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
        let first = tokio::time::timeout(cfg.handshake_timeout, next_frame(&mut reader))
            .await
            .map_err(|_| ConnectError::Timeout)?;
        let welcome = match first {
            None | Some(Err(CodecError::Io(_))) => return Err(ConnectError::ClosedBeforeWelcome),
            Some(Err(CodecError::Frame(e))) => return Err(ConnectError::Protocol(e.to_string())),
            Some(Ok(Frame::Welcome(w))) => w,
            Some(Ok(Frame::Response(r))) => return Err(handshake_refusal(r, cfg.protocol_major)),
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
        let client = Client { inner, out: out_tx, welcome };
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
    pub async fn view_query(&self, query: pb::Query) -> Result<(), ClientError> {
        if query.kind.as_ref().map(query_lane) != Some(QueryLane::ViewQuery) {
            return Err(ClientError::WrongLane);
        }
        self.ordered(pb::request::Body::Query(query)).await
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
async fn read_loop<R: tokio::io::AsyncRead + Unpin>(
    mut reader: FramedRead<R, FrameCodec>,
    inner: Arc<Inner>,
    incoming: IncomingTx,
) {
    let reason = loop {
        let frame = match next_frame(&mut reader).await {
            None => break CloseReason::PeerClosed,
            Some(Err(CodecError::Io(e))) => break CloseReason::Io(e.to_string()),
            Some(Err(CodecError::Frame(e))) => break CloseReason::Protocol(e.to_string()),
            Some(Ok(f)) => f,
        };
        match frame {
            Frame::Event(e) => incoming.push(IncomingItem::Event(e)).await,
            Frame::Response(r) => dispatch_response(&inner, &incoming, r).await,
            other => {
                break CloseReason::Protocol(format!(
                    "已建立后收到 {}",
                    other.frame_type().as_str_name()
                ));
            }
        }
    };
    inner.close_all();
    incoming.push(IncomingItem::Closed(reason)).await;
}

async fn dispatch_response(inner: &Inner, incoming: &IncomingTx, r: pb::Response) {
    use pb::response::Body;
    if r.request_id == 0 {
        if let Some(Body::Error(e)) = r.body {
            incoming.push(IncomingItem::ServerError(e)).await;
        }
        return;
    }
    let Some(p) = inner.take(r.request_id) else {
        tracing::warn!(target: "iwork_ipc", request_id = r.request_id, "收到没人等的响应");
        return;
    };
    let rid = r.request_id;
    match p {
        Pending::Direct(tx) => {
            let _ = tx.send(match r.body {
                Some(Body::Error(e)) => Err(ClientError::Server(e)),
                _ => Ok(r),
            });
        }
        Pending::Ordered(tx) => {
            // 先把载荷排进 Incoming，再唤醒 future：future 完成时对应项已经排好。
            let outcome = match r.body {
                Some(Body::Error(e)) => Err(ClientError::Server(e)),
                Some(Body::Result(result)) => {
                    incoming
                        .push(IncomingItem::ViewResult {
                            request_id: rid,
                            result,
                        })
                        .await;
                    Ok(())
                }
                Some(Body::Subscribed(subscribed)) => {
                    incoming
                        .push(IncomingItem::Subscribed {
                            request_id: rid,
                            subscribed,
                        })
                        .await;
                    Ok(())
                }
                _ => Err(ClientError::UnexpectedResponse),
            };
            let _ = tx.send(outcome);
        }
        Pending::Ack(tx) => {
            let _ = tx.send(match r.body {
                Some(Body::Error(e)) => Err(ClientError::Server(e)),
                _ => Ok(()),
            });
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
