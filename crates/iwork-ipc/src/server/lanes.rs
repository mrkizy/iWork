//! 已建立连接的入站分道（方案 2.7）：命令道（串行）、并行查询道（≤ `query_concurrency`）、交接 FIFO。
//!
//! **准入上限**：每条连接同时存在的请求（排队中 + 执行中，含命令、交接 FIFO 里的操作、等查询道名额的
//! 查询）不超过 `ServerConfig::max_pending_requests`。每个被接受的请求持有一张准入票 [`Admitted`]，
//! 响应发出（或操作结束）时随之释放；票用完时读循环停止读 socket——客户端狂发请求又不等响应，
//! 被堵住的是它自己，后台内存有上界（方案 2.9「读暂停兜底」的落实）。
//!
//! **handler 随连接取消**：handler 在单独的任务里跑（为了接住 panic），这个任务的句柄用
//! [`AbortOnDrop`] 包着：等它的 future 一被丢弃（交接超时、连接关闭时整条道被中止），handler 任务也被中止，
//! 不会在连接关闭后继续读存储。

use std::collections::VecDeque;
use std::future::Future;
use std::sync::Arc;

use iwork_proto::limits::MAX_CHUNK_DATA;
use iwork_proto::pb::{self, ErrorCode};
use iwork_proto::routing::{QueryLane, query_lane};
use tokio::sync::{OwnedSemaphorePermit, Semaphore, mpsc};
use tokio::task::JoinSet;

use super::Shared;
use super::connection::ConnShared;
use super::handler::{ConnectionContext, EventSource, RequestHandler};
use super::stream::{FailCause, ScopeAdd};
use crate::audit;
use crate::frame::{CodecError, Frame};

/// 一个被接受的请求占用的准入票；丢掉即释放。
pub(super) type Admitted = OwnedSemaphorePermit;

/// 交接 FIFO 里的一项。
pub(super) enum FifoOp {
    Subscribe {
        request_id: u64,
        add: ScopeAdd,
        from_seq: u64,
        admitted: Admitted,
    },
    Unsubscribe {
        request_id: u64,
        id: u32,
        admitted: Admitted,
    },
    ViewQuery {
        req: pb::Request,
        admitted: Admitted,
    },
}

pub(super) struct Lanes<H, S> {
    shared: Arc<Shared<H, S>>,
    conn: Arc<ConnShared>,
    /// 准入票：总数 = `max_pending_requests`。
    admission: Arc<Semaphore>,
    cmd_tx: mpsc::UnboundedSender<(pb::Request, Admitted)>,
    /// 并行查询道的名额。先拿名额再 spawn，等名额的查询排在 `side_waiting` 里（受准入上限约束）。
    side: Arc<Semaphore>,
    side_waiting: VecDeque<(pb::Request, Admitted)>,
    /// 命令道与在飞的并行查询。由连接任务及时收割；连接结束时整体中止。
    tasks: JoinSet<()>,
}

impl<H: RequestHandler, S: EventSource> Lanes<H, S> {
    pub(super) fn new(shared: &Arc<Shared<H, S>>, conn: &Arc<ConnShared>) -> Self {
        let mut tasks = JoinSet::new();
        let (cmd_tx, cmd_rx) = mpsc::unbounded_channel();
        tasks.spawn(command_lane(Arc::clone(shared), Arc::clone(conn), cmd_rx));
        Self {
            shared: Arc::clone(shared),
            conn: Arc::clone(conn),
            admission: Arc::new(Semaphore::new(shared.cfg.max_pending_requests)),
            cmd_tx,
            side: Arc::new(Semaphore::new(shared.cfg.query_concurrency)),
            side_waiting: VecDeque::new(),
            tasks,
        }
    }

    /// 还能接受新请求吗。不能时读循环停止读 socket。
    pub(super) fn can_admit(&self) -> bool {
        self.admission.available_permits() > 0
    }

    /// 等到有请求完成、准入票被释放。返回的 future 不借用 `self`，可与其他分支一起放进 `select!`。
    pub(super) fn admission_freed(&self) -> impl Future<Output = ()> + Send + 'static {
        let admission = Arc::clone(&self.admission);
        async move {
            if let Ok(p) = admission.acquire_owned().await {
                drop(p);
            }
        }
    }

    /// 收割一个已结束的任务；没有任务时永远挂起（给 `select!` 用）。
    pub(super) async fn reap(&mut self) {
        if self.tasks.join_next().await.is_none() {
            std::future::pending::<()>().await;
        }
    }

    /// 有查询在排队时，等查询道空出一个名额；没有排队的查询时永远挂起。不借用 `self`。
    pub(super) fn side_slot(
        &self,
    ) -> impl Future<Output = Option<OwnedSemaphorePermit>> + Send + 'static {
        let waiting = !self.side_waiting.is_empty();
        let side = Arc::clone(&self.side);
        async move {
            if !waiting {
                std::future::pending::<()>().await;
            }
            side.acquire_owned().await.ok()
        }
    }

    /// 拿到查询道名额后派出排在最前的查询。
    pub(super) fn spawn_side_query(&mut self, permit: OwnedSemaphorePermit) {
        let Some((req, admitted)) = self.side_waiting.pop_front() else {
            return;
        };
        let (shared, conn) = (Arc::clone(&self.shared), Arc::clone(&self.conn));
        self.tasks.spawn(async move {
            let resp = call_handler(&shared, &conn.ctx, req).await;
            conn.push_response(resp);
            drop((permit, admitted));
        });
    }

    /// 连接结束：中止命令道与全部在飞查询（连带它们的 handler 任务）。
    pub(super) fn abort_all(&mut self) {
        self.tasks.abort_all();
    }

    /// 处理一条入站帧。返回 false 表示连接要结束。
    pub(super) async fn handle(
        &mut self,
        f: Result<Frame, CodecError>,
        fifo: &mut VecDeque<FifoOp>,
    ) -> bool {
        let frame = match f {
            Ok(frame) => frame,
            Err(CodecError::Io(_)) => return false,
            Err(CodecError::Frame(e)) => {
                self.violation(e.frame_type_name(), &e.to_string());
                return false;
            }
        };
        match frame {
            Frame::Request(req) => self.request(req, fifo),
            Frame::Chunk(chunk) => self.chunk(chunk).await,
            Frame::Hello(_) => {
                self.violation("FRAME_TYPE_HELLO", "握手后又收到 Hello");
                false
            }
            // 解码器按方向已拒绝 Welcome / Response / Event。
            other => {
                self.violation(other.frame_type().as_str_name(), "方向不对");
                false
            }
        }
    }

    pub(super) fn violation(&self, frame_type: &str, reason: &str) {
        audit::protocol_violation(
            self.conn.ctx.connection_id,
            self.conn.ctx.peer.pid,
            frame_type,
            reason,
            "established",
        );
        let err = pb::Error {
            code: ErrorCode::MalformedFrame.into(),
            message: format!("协议违规：{reason}"),
            retryable: false,
            details: [
                ("frame_type".to_owned(), frame_type.to_owned()),
                ("reason".to_owned(), reason.to_owned()),
            ]
            .into(),
        };
        self.conn
            .fail(FailCause::ProtocolViolation, Some(error_response(0, err)));
    }

    fn request(&mut self, req: pb::Request, fifo: &mut VecDeque<FifoOp>) -> bool {
        use pb::request::Body;
        if req.request_id == 0 {
            self.violation("FRAME_TYPE_REQUEST", "request_id 为 0");
            return false;
        }
        // 读循环只在还有票时读帧，所以这里一定拿得到。
        let Ok(admitted) = Arc::clone(&self.admission).try_acquire_owned() else {
            unreachable!("读循环在准入票用完时不读帧");
        };
        let rid = req.request_id;
        match &req.body {
            None => self.conn.push_response(invalid(rid, "Request.body 为空")),
            Some(Body::Command(_)) => {
                let _ = self.cmd_tx.send((req, admitted));
            }
            Some(Body::Query(q)) => match q.kind.as_ref().map(query_lane) {
                None => self.conn.push_response(invalid(rid, "Query.kind 为空")),
                Some(QueryLane::ViewQuery) => fifo.push_back(FifoOp::ViewQuery { req, admitted }),
                Some(QueryLane::SideQuery) => self.side_waiting.push_back((req, admitted)),
            },
            Some(Body::Subscribe(sub)) => {
                let scope = sub.scope.clone().unwrap_or_default();
                let keys = self.shared.source.scope_streams(&scope);
                if keys.is_empty() {
                    // 错误立即发出，不占 FIFO。
                    self.conn.push_response(invalid(rid, "订阅范围为空或无效"));
                } else {
                    fifo.push_back(FifoOp::Subscribe {
                        request_id: rid,
                        add: ScopeAdd {
                            id: sub.subscription_id,
                            scope,
                            keys,
                        },
                        from_seq: sub.from_seq,
                        admitted,
                    });
                }
            }
            Some(Body::Unsubscribe(u)) => fifo.push_back(FifoOp::Unsubscribe {
                request_id: rid,
                id: u.subscription_id,
                admitted,
            }),
        }
        true
    }

    async fn chunk(&mut self, chunk: pb::Chunk) -> bool {
        if chunk.upload_id.is_empty() {
            self.violation("FRAME_TYPE_CHUNK", "upload_id 为空");
            return false;
        }
        if chunk.data.len() > MAX_CHUNK_DATA {
            self.violation("FRAME_TYPE_CHUNK", "Chunk.data 超过 1MiB");
            return false;
        }
        match self.shared.handler.chunk(&self.conn.ctx, chunk).await {
            Ok(()) => true,
            Err(refused) => {
                self.conn
                    .fail(FailCause::ChunkRefused, Some(error_response(0, refused.0)));
                false
            }
        }
    }
}

pub(super) fn error_response(request_id: u64, error: pb::Error) -> pb::Response {
    pb::Response {
        request_id,
        body: Some(pb::response::Body::Error(error)),
    }
}

pub(super) fn error(request_id: u64, code: ErrorCode, message: &str) -> pb::Response {
    error_response(
        request_id,
        pb::Error {
            code: code.into(),
            message: message.to_owned(),
            retryable: false,
            details: Default::default(),
        },
    )
}

fn invalid(request_id: u64, message: &str) -> pb::Response {
    error(request_id, ErrorCode::InvalidArgument, message)
}

/// 任务句柄：被丢弃时中止任务。让 handler 任务的生命周期从属于等它的那个 future。
struct AbortOnDrop<T>(tokio::task::JoinHandle<T>);

impl<T> Drop for AbortOnDrop<T> {
    fn drop(&mut self) {
        self.0.abort();
    }
}

/// 调 handler，接住它的 panic（连接回 `INTERNAL`，panic 本身由 daemon 的监督器处理）。
/// 这个 future 被丢弃时 handler 任务随之中止（见模块文档）。
pub(super) async fn call_handler<H: RequestHandler, S>(
    shared: &Arc<Shared<H, S>>,
    ctx: &ConnectionContext,
    req: pb::Request,
) -> pb::Response {
    let rid = req.request_id;
    let (handler, ctx) = (Arc::clone(&shared.handler), ctx.clone());
    let mut task = AbortOnDrop(tokio::spawn(async move { handler.handle(&ctx, req).await }));
    match (&mut task.0).await {
        Ok(mut resp) => {
            resp.request_id = rid;
            resp
        }
        Err(e) => {
            tracing::error!(target: "iwork_ipc", request_id = rid, error = %e, "handler 异常");
            error(rid, ErrorCode::Internal, "后台服务内部错误")
        }
    }
}

async fn command_lane<H: RequestHandler, S: EventSource>(
    shared: Arc<Shared<H, S>>,
    conn: Arc<ConnShared>,
    mut rx: mpsc::UnboundedReceiver<(pb::Request, Admitted)>,
) {
    // 一个接一个：用户连发两条消息，第二条先落盘会颠倒对话顺序。
    while let Some((req, admitted)) = rx.recv().await {
        let resp = call_handler(&shared, &conn.ctx, req).await;
        conn.push_response(resp);
        drop(admitted);
    }
}
