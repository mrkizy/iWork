//! 一条连接的生命周期（方案 2.6、2.7、2.8「交接」、2.9）。
//!
//! ```text
//! Accepted ──peer 不过──▶ Closed(audit, 不发任何帧)
//!    │ peer 过
//! AwaitHello ──超时 / 非 Hello / 主版本不符──▶ Closed
//!    │ Welcome 已发
//! Established ──fail_connection / 对端关闭 / 服务停机──▶ Closed
//! ```
//!
//! 已建立后有四条道：命令道（串行）、并行查询道（≤ `query_concurrency`）、交接 FIFO（`Subscribe` /
//! `Unsubscribe` / 会写视图的查询，一次一个）、写任务。流出任何异常都只走 [`ConnShared::fail`]。

use std::collections::VecDeque;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use bytes::BytesMut;
use futures_core::Stream as _;
use iwork_proto::limits::{MAX_FRAME_LEN, MAX_HELLO_LEN};
use iwork_proto::pb::{self, ErrorCode};
use tokio::io::{AsyncWrite, AsyncWriteExt};
use tokio::sync::Notify;
use tokio::time::Instant;
use tokio_util::codec::FramedRead;
use tokio_util::sync::CancellationToken;

use super::Shared;
use super::budget::{ConnectionBudget, Push};
use super::handler::{ConnectionContext, EventSource, PeerSummary, RequestHandler, RoutedEvent};
use super::lanes::{FifoOp, Lanes, call_handler, error, error_response};
use super::stream::{FailCause, ScopeAdd, Stream};
use crate::audit::{self, Role};
use crate::frame::{CodecError, Direction, Frame, FrameCodec, encode_frame};
use crate::peer::{PeerVerdict, judge_peer};
use crate::transport::Connection;

/// 写任务一次合并写出的上限：每秒上千条小事件时减少系统调用。
const WRITE_BATCH: usize = 64 * 1024;
/// 关闭前把最后一帧（`MALFORMED_FRAME` 等）写出去的上限。
const FINAL_FRAME_WRITE: Duration = Duration::from_secs(1);

struct ConnState {
    stream: Stream,
    budget: ConnectionBudget,
    closed: Option<FailCause>,
    /// 关闭前要写出的最后一帧（协议违规的 `MALFORMED_FRAME`、分片被拒的错误）。
    final_frame: Option<pb::Response>,
    /// 正在执行的交接，审计用。
    in_flight: String,
}

pub(crate) struct ConnShared {
    pub(super) ctx: ConnectionContext,
    state: Mutex<ConnState>,
    wake_writer: Notify,
    /// 写任务每写出一批就通知：读循环据此解除暂停，补发据此重试。
    read_resume: Notify,
    replay_space: Notify,
    cancel: CancellationToken,
}

impl ConnShared {
    fn lock(&self) -> std::sync::MutexGuard<'_, ConnState> {
        self.state.lock().expect("连接状态锁中毒")
    }

    pub(crate) fn on_persisted(&self, e: &Arc<RoutedEvent>) {
        let r = {
            let mut st = self.lock();
            if st.closed.is_some() {
                return;
            }
            let ConnState { stream, budget, .. } = &mut *st;
            stream.on_persisted(e, budget)
        };
        self.after_push(r);
    }

    pub(crate) fn on_transient(
        &self,
        event: &pb::TransientEvent,
        routes: &[iwork_proto::routing::StreamKey],
    ) {
        let r = {
            let mut st = self.lock();
            if st.closed.is_some() {
                return;
            }
            let ConnState { stream, budget, .. } = &mut *st;
            stream.on_transient(event, routes, budget)
        };
        self.after_push(r);
    }

    fn after_push(&self, r: Result<bool, FailCause>) {
        match r {
            Ok(true) => self.wake_writer.notify_one(),
            Ok(false) => {}
            Err(cause) => self.fail(cause, None),
        }
    }

    pub(super) fn push_response(&self, r: pb::Response) {
        {
            let mut st = self.lock();
            if st.closed.is_some() {
                return;
            }
            st.budget.push_response(r);
        }
        self.wake_writer.notify_one();
    }

    /// 在锁内对流与预算做一件事；连接已关就什么都不做。
    fn with_stream<T>(&self, f: impl FnOnce(&mut Stream, &mut ConnectionBudget) -> T) -> Option<T> {
        let out = {
            let mut st = self.lock();
            if st.closed.is_some() {
                return None;
            }
            let ConnState { stream, budget, .. } = &mut *st;
            f(stream, budget)
        };
        self.wake_writer.notify_one();
        Some(out)
    }

    fn set_in_flight(&self, what: String) {
        self.lock().in_flight = what;
    }

    /// `fail_connection`：连接因任何流异常、协议违规、写失败而关闭的唯一入口（方案 2.9 第 3 条）。
    ///
    /// 固定顺序：① 审计；② 取消补发与在飞的交接、丢 FIFO（经取消令牌，由连接任务收尾）；③ 停止读写，
    /// 丢弃队列与缓冲（持久事件在存储里，客户端重连后续传）；④ 关闭 socket（写任务写完 `final_frame` 后退出）；
    /// ⑤ 连接任务收尾时调 `handler.connection_closed`。重复调用只有第一次生效。
    pub(crate) fn fail(&self, cause: FailCause, final_frame: Option<pb::Response>) {
        let (cursor, used, in_flight) = {
            let mut st = self.lock();
            if st.closed.is_some() {
                return;
            }
            let snapshot = (
                st.stream.cursor(),
                st.budget.used(),
                std::mem::take(&mut st.in_flight),
            );
            st.closed = Some(cause);
            st.budget.clear();
            st.stream.clear();
            st.final_frame = final_frame;
            snapshot
        };
        audit::connection_failed(
            self.ctx.connection_id,
            self.ctx.peer.pid,
            cause,
            cursor,
            used,
            &in_flight,
        );
        self.cancel.cancel();
        self.wake_writer.notify_one();
        self.replay_space.notify_one();
        self.read_resume.notify_one();
    }

    fn is_closed(&self) -> bool {
        self.lock().closed.is_some()
    }

    fn should_pause_reading(&self) -> bool {
        let st = self.lock();
        st.budget.used() >= st.budget.limit()
    }

    fn may_resume_reading(&self) -> bool {
        let st = self.lock();
        st.closed.is_some() || st.budget.used() < st.budget.limit() / 2
    }
}

/// 连接任务入口：校验对端 → 握手 → 已建立。
pub(crate) async fn run<H, S, C>(
    shared: Arc<Shared<H, S>>,
    conn: C,
    id: u64,
    shutdown: CancellationToken,
) where
    H: RequestHandler,
    S: EventSource,
    C: Connection,
{
    // 1. 校验对端：不过就关闭，不发任何帧。
    let peer = match conn.peer().await {
        Ok(p) => p,
        Err(e) => {
            audit::peer_identity_failed(Role::Server, &e);
            return;
        }
    };
    let signature_checked = match judge_peer(&shared.own, &peer, shared.cfg.trust) {
        PeerVerdict::Rejected(reason) => {
            audit::peer_rejected(Role::Server, &peer, &reason);
            return;
        }
        PeerVerdict::Accepted { signature_checked } => signature_checked,
    };
    #[cfg(feature = "dev")]
    if !signature_checked {
        audit::peer_accepted_without_signature_check(Role::Server, &peer);
    }

    // 2. 等 Hello：上限 64KiB、5s。
    let (rd, mut wr) = conn.into_split();
    let mut reader = FramedRead::new(rd, FrameCodec::new(Direction::ToServer, MAX_HELLO_LEN));
    let first = tokio::time::timeout(shared.cfg.handshake_timeout, next_frame(&mut reader)).await;
    let hello = match first {
        Ok(Some(Ok(Frame::Hello(h)))) => h,
        Ok(None) => return, // 对端关闭
        Ok(Some(Ok(other))) => {
            audit::protocol_violation(
                id,
                peer.pid,
                other.frame_type().as_str_name(),
                "握手前收到非 Hello 帧",
                "handshake",
            );
            return;
        }
        Ok(Some(Err(CodecError::Frame(e)))) => {
            audit::protocol_violation(
                id,
                peer.pid,
                e.frame_type_name(),
                &e.to_string(),
                "handshake",
            );
            return;
        }
        Ok(Some(Err(CodecError::Io(_)))) => return,
        Err(_) => {
            audit::protocol_violation(id, peer.pid, "NONE", "握手超时", "handshake");
            return;
        }
    };

    // 3. 主版本门：不符就回错误并断开。此时没有登记任何订阅、没有把任何帧交给 handler。
    if let Err(e) = iwork_proto::handshake::check_hello(&hello, shared.cfg.protocol_major) {
        let resp = pb::Response {
            request_id: 0,
            body: Some(pb::response::Body::Error(e)),
        };
        // 握手阶段的写也有期限：对端不读时不能把连接任务挂住。
        let _ = tokio::time::timeout(shared.cfg.handshake_timeout, async {
            write_one(&mut wr, &Frame::Response(resp)).await;
            wr.shutdown().await
        })
        .await;
        return;
    }
    if hello.resume_token.is_some() {
        tracing::debug!(target: "iwork_ipc", connection_id = id, "Hello.resume_token M1 不用，忽略");
    }

    // 4. Welcome。
    let head_seq = match shared.source.head_seq().await {
        Ok(h) => h,
        Err(e) => {
            tracing::error!(target: "iwork_ipc", connection_id = id, error = %e, "读 head 失败，关闭连接");
            return;
        }
    };
    let session_id = ulid::Ulid::generate().to_string();
    let welcome = pb::Welcome {
        session_id: session_id.clone(),
        server_version: shared.cfg.server_version.clone(),
        capabilities: Vec::new(),
        head_seq,
        protocol_major: shared.cfg.protocol_major,
        protocol_minor: shared.cfg.protocol_minor,
    };
    let sent = tokio::time::timeout(
        shared.cfg.handshake_timeout,
        write_one(&mut wr, &Frame::Welcome(welcome)),
    )
    .await;
    if sent != Ok(true) {
        return;
    }
    reader.decoder_mut().set_max_len(MAX_FRAME_LEN);

    let ctx = ConnectionContext {
        connection_id: id,
        session_id,
        peer: PeerSummary {
            pid: peer.pid,
            user: peer.user.clone(),
        },
        client_kind: hello.client_kind(),
        signature_checked,
    };
    let conn = Arc::new(ConnShared {
        ctx: ctx.clone(),
        state: Mutex::new(ConnState {
            stream: Stream::new(),
            budget: ConnectionBudget::new(shared.cfg.budget_limit, shared.cfg.replay_reserve),
            closed: None,
            final_frame: None,
            in_flight: String::new(),
        }),
        wake_writer: Notify::new(),
        read_resume: Notify::new(),
        replay_space: Notify::new(),
        cancel: CancellationToken::new(),
    });
    shared
        .hub
        .conns
        .lock()
        .expect("连接表锁中毒")
        .insert(id, Arc::clone(&conn));
    tracing::debug!(target: "iwork_ipc", connection_id = id, pid = peer.pid, "连接已建立");

    let writer = tokio::spawn(write_loop(Arc::clone(&conn), wr));
    established(&shared, &conn, reader, &shutdown).await;

    conn.cancel.cancel();
    conn.wake_writer.notify_one();
    let _ = writer.await;
    shared.hub.conns.lock().expect("连接表锁中毒").remove(&id);
    shared.handler.connection_closed(&ctx).await;
    tracing::debug!(target: "iwork_ipc", connection_id = id, "连接已关闭");
}

async fn next_frame<R: tokio::io::AsyncRead + Unpin>(
    reader: &mut FramedRead<R, FrameCodec>,
) -> Option<Result<Frame, CodecError>> {
    std::future::poll_fn(|cx| std::pin::Pin::new(&mut *reader).poll_next(cx)).await
}

/// 不阻塞地看一眼有没有已经能读出的帧（已在缓冲里或 socket 立即可读）。
async fn try_next_frame<R: tokio::io::AsyncRead + Unpin>(
    reader: &mut FramedRead<R, FrameCodec>,
) -> Option<Option<Result<Frame, CodecError>>> {
    tokio::select! {
        biased;
        f = next_frame(reader) => Some(f),
        () = std::future::ready(()) => None,
    }
}

async fn write_one<W: AsyncWrite + Unpin>(wr: &mut W, frame: &Frame) -> bool {
    let mut buf = BytesMut::new();
    encode_frame(frame, &mut buf);
    wr.write_all(&buf).await.is_ok() && wr.flush().await.is_ok()
}

/// 写任务。每次写都和连接的取消令牌一起等：客户端停读、socket 写不动时，`fail_connection` 或停机
/// 一取消就放弃这次写、丢掉写端，连接的收尾不依赖对端恢复读取。
async fn write_loop<W: AsyncWrite + Unpin>(conn: Arc<ConnShared>, mut wr: W) {
    let mut buf = BytesMut::with_capacity(WRITE_BATCH);
    // 一次写到一半被取消后，流里留着半帧：之后不能再写任何东西（包括最后一帧），只能关掉。
    let mut torn = false;
    loop {
        enum Next {
            Write,
            Idle,
            Close(Option<Box<pb::Response>>),
        }
        let next = {
            let mut st = conn.lock();
            if st.closed.is_some() {
                Next::Close(st.final_frame.take().map(Box::new))
            } else if conn.cancel.is_cancelled() {
                Next::Close(None)
            } else if st.budget.pop_batch(WRITE_BATCH, &mut buf) > 0 {
                Next::Write
            } else {
                Next::Idle
            }
        };
        match next {
            Next::Close(final_frame) => {
                if let Some(f) = final_frame.filter(|_| !torn) {
                    let _ = tokio::time::timeout(
                        FINAL_FRAME_WRITE,
                        write_one(&mut wr, &Frame::Response(*f)),
                    )
                    .await;
                }
                let _ = tokio::time::timeout(FINAL_FRAME_WRITE, wr.shutdown()).await;
                return;
            }
            Next::Idle => {
                tokio::select! {
                    () = conn.wake_writer.notified() => {}
                    () = conn.cancel.cancelled() => {}
                }
            }
            Next::Write => {
                let written = tokio::select! {
                    r = async { wr.write_all(&buf).await?; wr.flush().await } => Some(r.is_ok()),
                    () = conn.cancel.cancelled() => None,
                };
                buf.clear();
                match written {
                    Some(true) => {
                        conn.read_resume.notify_one();
                        conn.replay_space.notify_one();
                    }
                    Some(false) => conn.fail(FailCause::PeerClosed, None),
                    None => torn = true,
                }
            }
        }
    }
}

async fn established<H, S, R>(
    shared: &Arc<Shared<H, S>>,
    conn: &Arc<ConnShared>,
    mut reader: FramedRead<R, FrameCodec>,
    shutdown: &CancellationToken,
) where
    H: RequestHandler,
    S: EventSource,
    R: tokio::io::AsyncRead + Unpin,
{
    let mut lanes = Lanes::new(shared, conn);
    let mut fifo: VecDeque<FifoOp> = VecDeque::new();
    let mut op: Option<AbortOnDropHandle> = None;

    loop {
        // 取 FIFO 头之前先把已经能读到的入站帧都处理完（都入队），这样重连时一次写出的多条 Subscribe
        // 能合并成一次续传（方案 2.8 第 0 步）。准入票用完时不读，直接开始执行。
        if op.is_none() && !fifo.is_empty() {
            let ready = if lanes.can_admit() {
                try_next_frame(&mut reader).await
            } else {
                None
            };
            match ready {
                Some(Some(f)) => {
                    if !lanes.handle(f, &mut fifo).await {
                        break;
                    }
                    continue;
                }
                Some(None) => break,
                None => op = Some(start_op(shared, conn, &mut fifo)),
            }
        }
        // 两种情况停止读 socket：发送预算用满（等写任务腾地方），或准入票用完（等请求完成）。
        let budget_paused = conn.should_pause_reading();
        let can_read = !budget_paused && lanes.can_admit();
        let side_slot = lanes.side_slot();
        let admission_freed = lanes.admission_freed();
        tokio::select! {
            biased;
            () = conn.cancel.cancelled() => break,
            () = shutdown.cancelled() => {
                flush_before_shutdown(conn, shared.cfg.shutdown_flush).await;
                break;
            }
            _ = async { (&mut op.as_mut().expect("guarded").0).await }, if op.is_some() => op = None,
            () = lanes.reap() => {}
            Some(permit) = side_slot => lanes.spawn_side_query(permit),
            () = conn.read_resume.notified(), if budget_paused => {
                // 写任务写出了一批；下一轮重新判断是否解除暂停。
                if !conn.may_resume_reading() { continue; }
            }
            () = admission_freed, if !budget_paused && !can_read => {}
            f = next_frame(&mut reader), if can_read => match f {
                Some(f) => if !lanes.handle(f, &mut fifo).await { break; },
                None => break,
            },
        }
    }
    // 中止在飞的交接与全部请求任务；它们持有的 handler 任务随之中止。
    drop(op);
    lanes.abort_all();
}

/// 交接任务的句柄：被丢弃时中止任务（连带它等着的 handler）。
struct AbortOnDropHandle(tokio::task::JoinHandle<()>);

impl Drop for AbortOnDropHandle {
    fn drop(&mut self) {
        self.0.abort();
    }
}

async fn flush_before_shutdown(conn: &ConnShared, limit: Duration) {
    let deadline = Instant::now() + limit;
    loop {
        if conn.is_closed() || conn.lock().budget.queue_is_empty() {
            return;
        }
        conn.wake_writer.notify_one();
        if tokio::time::timeout_at(deadline, conn.read_resume.notified())
            .await
            .is_err()
        {
            return;
        }
    }
}

/// 从 FIFO 取一个操作开始执行；头部连续的、`from_seq` 相同的 `Subscribe` 合并成一次。
/// 操作持有的准入票在任务结束（或被中止）时释放。
fn start_op<H: RequestHandler, S: EventSource>(
    shared: &Arc<Shared<H, S>>,
    conn: &Arc<ConnShared>,
    fifo: &mut VecDeque<FifoOp>,
) -> AbortOnDropHandle {
    let (shared, conn) = (Arc::clone(shared), Arc::clone(conn));
    let task = match fifo.pop_front().expect("调用方已确认非空") {
        FifoOp::Subscribe {
            request_id,
            add,
            from_seq,
            admitted,
        } => {
            let mut batch = vec![(request_id, add)];
            let mut tickets = vec![admitted];
            while let Some(FifoOp::Subscribe { from_seq: f, .. }) = fifo.front()
                && *f == from_seq
            {
                let Some(FifoOp::Subscribe {
                    request_id,
                    add,
                    admitted,
                    ..
                }) = fifo.pop_front()
                else {
                    unreachable!()
                };
                batch.push((request_id, add));
                tickets.push(admitted);
            }
            tokio::spawn(async move {
                subscribe(&shared, &conn, batch, from_seq).await;
                drop(tickets);
            })
        }
        FifoOp::Unsubscribe {
            request_id,
            id,
            admitted,
        } => tokio::spawn(async move {
            let resp = conn.with_stream(|stream, _| {
                if stream.remove_scope(id) {
                    pb::Response {
                        request_id,
                        body: Some(pb::response::Body::Subscribed(pb::Subscribed {
                            subscription_id: id,
                            ..Default::default()
                        })),
                    }
                } else {
                    error(request_id, ErrorCode::NotFound, "没有这个订阅")
                }
            });
            if let Some(resp) = resp {
                conn.push_response(resp);
            }
            drop(admitted);
        }),
        FifoOp::ViewQuery { req, admitted } => tokio::spawn(async move {
            view_query(&shared, &conn, req).await;
            drop(admitted);
        }),
    };
    AbortOnDropHandle(task)
}

/// 等发布水位追上 `s`；超过交接期限按 `PublishStalled` 关连接。
async fn wait_published<H, S>(
    shared: &Shared<H, S>,
    conn: &ConnShared,
    s: u64,
    deadline: Instant,
) -> bool {
    let mut rx = shared.hub.published_head.subscribe();
    match tokio::time::timeout_at(deadline, rx.wait_for(|h| *h >= s)).await {
        Ok(Ok(_)) => true,
        _ => {
            conn.fail(FailCause::PublishStalled, None);
            false
        }
    }
}

/// 订阅（合并后的一批）：登记范围、暂停 → 读 head → 续传或快照 → 交接回 Live（方案 2.8 第 1–8 步）。
async fn subscribe<H: RequestHandler, S: EventSource>(
    shared: &Shared<H, S>,
    conn: &ConnShared,
    batch: Vec<(u64, ScopeAdd)>,
    from_seq: u64,
) {
    let ids: Vec<(u64, u32)> = batch.iter().map(|(rid, a)| (*rid, a.id)).collect();
    let adds = batch.into_iter().map(|(_, a)| a).collect();
    let max_scopes = shared.cfg.max_scopes;
    // 先登记后读 head：从这一刻起匹配新过滤的实时事件都进缓冲。
    let Some((resume_base, too_many)) = conn.with_stream(|stream, _| {
        let base = stream.add_scopes(adds, from_seq);
        (base, stream.scope_count() > max_scopes)
    }) else {
        return;
    };
    if too_many {
        audit::protocol_violation(
            conn.ctx.connection_id,
            conn.ctx.peer.pid,
            "FRAME_TYPE_REQUEST",
            "同时存在的订阅范围超过上限",
            "established",
        );
        let err = pb::Error {
            code: ErrorCode::MalformedFrame.into(),
            message: format!("协议违规：同时存在的订阅范围超过 {max_scopes}"),
            retryable: false,
            details: Default::default(),
        };
        conn.fail(FailCause::ProtocolViolation, Some(error_response(0, err)));
        return;
    }
    let deadline = Instant::now() + shared.cfg.handover_timeout;
    let head = match tokio::time::timeout_at(deadline, shared.source.head_seq()).await {
        Ok(Ok(h)) => h,
        Ok(Err(e)) => return source_failed(conn, &e),
        Err(_) => return conn.fail(FailCause::HandoverTimeout, None),
    };
    let resume = from_seq > 0
        && from_seq == resume_base
        && from_seq <= head
        && head - from_seq <= shared.cfg.replay_limit;
    if resume {
        conn.with_stream(|stream, _| stream.adopt_cursor(from_seq));
        conn.set_in_flight(format!("replay({from_seq},{head}]"));
        for (rid, id) in &ids {
            conn.push_response(subscribed(*rid, *id, None, 0));
        }
        if replay(shared, conn, from_seq, head).await {
            conn.with_stream(|stream, budget| stream.finish_replay(head, budget));
            conn.set_in_flight(String::new());
        }
    } else {
        conn.set_in_flight("snapshot".into());
        let Some((snapshot, s)) = take_snapshot(shared, conn, deadline).await else {
            return;
        };
        if !wait_published(shared, conn, s, deadline).await {
            return;
        }
        // 快照覆盖全部活动范围，随这批的第一个响应发出；其余只确认。
        let mut snapshot = Some(snapshot);
        let responses = ids
            .iter()
            .map(|(rid, id)| subscribed(*rid, *id, snapshot.take(), s))
            .collect();
        conn.with_stream(|stream, budget| stream.insert_at(s, responses, budget));
        conn.set_in_flight(String::new());
    }
}

fn subscribed(request_id: u64, id: u32, snapshot: Option<pb::Snapshot>, seq: u64) -> pb::Response {
    let has_snapshot = snapshot.is_some();
    pb::Response {
        request_id,
        body: Some(pb::response::Body::Subscribed(pb::Subscribed {
            subscription_id: id,
            snapshot,
            snapshot_seq: if has_snapshot { seq } else { 0 },
        })),
    }
}

fn source_failed(conn: &ConnShared, e: &super::SourceError) {
    tracing::error!(target: "iwork_ipc", connection_id = conn.ctx.connection_id, error = %e, "事件源出错");
    conn.fail(FailCause::SourceFailed, None);
}

/// 取一份快照，要求各部分 seq 一致（一个读事务）；不一致就丢掉重取，直到交接期限。
async fn take_snapshot<H, S: EventSource>(
    shared: &Shared<H, S>,
    conn: &ConnShared,
    deadline: Instant,
) -> Option<(pb::Snapshot, u64)> {
    let scopes = conn.with_stream(|stream, _| stream.active_scopes())?;
    loop {
        match tokio::time::timeout_at(deadline, shared.source.snapshot(&scopes)).await {
            Ok(Ok((snap, s))) => {
                if snap.results.iter().all(|r| r.seq == s) {
                    return Some((snap, s));
                }
                tracing::warn!(target: "iwork_ipc", connection_id = conn.ctx.connection_id, snapshot_seq = s, "快照各部分 seq 不一致（不在一个读事务里），丢弃重取");
            }
            Ok(Err(e)) => {
                source_failed(conn, &e);
                return None;
            }
            Err(_) => {
                conn.fail(FailCause::HandoverTimeout, None);
                return None;
            }
        }
    }
}

/// 从存储补发 `(from, to]`。每拿到一页重新计时；往队列放不下时等写任务腾地方，最多 `replay_reserve_timeout`。
async fn replay<H, S: EventSource>(
    shared: &Shared<H, S>,
    conn: &ConnShared,
    from: u64,
    to: u64,
) -> bool {
    let Some(filter) = conn.with_stream(|stream, _| stream.filter().to_vec()) else {
        return false;
    };
    let mut scan = from;
    while scan < to {
        let page = match tokio::time::timeout(
            shared.cfg.handover_timeout,
            shared
                .source
                .replay(&filter, scan, to, shared.cfg.replay_page),
        )
        .await
        {
            Ok(Ok(p)) => p,
            Ok(Err(e)) => {
                source_failed(conn, &e);
                return false;
            }
            Err(_) => {
                conn.fail(FailCause::HandoverTimeout, None);
                return false;
            }
        };
        if page.next_from <= scan {
            source_failed(
                conn,
                &super::SourceError(format!(
                    "补发没有前进：next_from {} ≤ {scan}",
                    page.next_from
                )),
            );
            return false;
        }
        for e in &page.events {
            if !deliver_replayed(shared, conn, e).await {
                return false;
            }
        }
        scan = page.next_from;
    }
    true
}

async fn deliver_replayed<H, S>(
    shared: &Shared<H, S>,
    conn: &ConnShared,
    e: &Arc<RoutedEvent>,
) -> bool {
    let mut deadline = None;
    loop {
        let Some(pushed) = conn.with_stream(|stream, budget| stream.deliver_replay(e, budget))
        else {
            return false;
        };
        if pushed == Push::Queued {
            return true;
        }
        let d = *deadline.get_or_insert_with(|| Instant::now() + shared.cfg.replay_reserve_timeout);
        if tokio::time::timeout_at(d, conn.replay_space.notified())
            .await
            .is_err()
        {
            conn.fail(FailCause::ReplayReserveTimeout, None);
            return false;
        }
    }
}

/// 会写视图的查询：暂停流 → 读事务里取结果与 S → 等发布水位 ≥ S → 插到流里 S 的位置 → 恢复（方案 2.8「交接」）。
async fn view_query<H: RequestHandler, S: EventSource>(
    shared: &Arc<Shared<H, S>>,
    conn: &Arc<ConnShared>,
    req: pb::Request,
) {
    let rid = req.request_id;
    if conn.with_stream(|stream, _| stream.pause()).is_none() {
        return;
    }
    conn.set_in_flight(format!("view_query({rid})"));
    let deadline = Instant::now() + shared.cfg.handover_timeout;
    let resp = match tokio::time::timeout_at(deadline, call_handler(shared, &conn.ctx, req)).await {
        Ok(r) => r,
        Err(_) => return conn.fail(FailCause::HandoverTimeout, None),
    };
    let s = match &resp.body {
        Some(pb::response::Body::Result(r)) => r.seq,
        // 业务失败：错误立即发出（不参与排序），缓冲按序送出，流照常。
        _ => {
            conn.with_stream(|stream, budget| {
                budget.push_response(resp);
                stream.drain_and_resume(budget);
            });
            conn.set_in_flight(String::new());
            return;
        }
    };
    if !wait_published(shared, conn, s, deadline).await {
        return;
    }
    conn.with_stream(|stream, budget| stream.insert_at(s, vec![resp], budget));
    conn.set_in_flight(String::new());
}
