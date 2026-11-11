//! 请求入口（T04 方案 2.4 第 9 步、2.8 第 1 步、2.14）：每个请求开 `command` span；停机期间关门；
//! 生命周期命令自己受理，其余交给注入的内层 handler。
//!
//! T04 注入的内层是 [`NotWired`]：一律回 `ERROR_CODE_UNAVAILABLE`「未接入」。真实的命令与查询受理归 T10。

use std::fmt::{self, Write as _};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use iwork_ipc::server::{ChunkRefused, ConnectionContext, RequestHandler};
use iwork_proto::pb;

use crate::logging;

/// 关门与在途计数。停机第 1 步：关门后新请求一律回「不可用、可重试」，再等已经进门的回完。
#[derive(Default)]
pub struct Gate {
    state: Mutex<GateState>,
    drained: tokio::sync::Notify,
}

#[derive(Default)]
struct GateState {
    closed: bool,
    in_flight: usize,
}

/// 进了门的请求；丢弃时出门。
pub struct Ticket<'a>(&'a Gate);

impl Drop for Ticket<'_> {
    fn drop(&mut self) {
        let mut s = self.0.lock();
        s.in_flight -= 1;
        if s.in_flight == 0 {
            self.0.drained.notify_waiters();
        }
    }
}

impl Gate {
    fn lock(&self) -> std::sync::MutexGuard<'_, GateState> {
        self.state.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// 进门。门关了返回 `None`。
    pub fn enter(&self) -> Option<Ticket<'_>> {
        let mut s = self.lock();
        if s.closed {
            return None;
        }
        s.in_flight += 1;
        Some(Ticket(self))
    }

    /// 关门并等在途请求回完，最多等 `limit`。返回到点时还没回完的个数。
    pub async fn close_and_drain(&self, limit: Duration) -> usize {
        self.lock().closed = true;
        let wait = async {
            loop {
                let notified = self.drained.notified();
                if self.lock().in_flight == 0 {
                    return;
                }
                notified.await;
            }
        };
        let _ = tokio::time::timeout(limit, wait).await;
        self.lock().in_flight
    }

    pub fn is_closed(&self) -> bool {
        self.lock().closed
    }
}

/// 只写到第一个 `(` 为止的 `fmt::Write`：从 oneof 的 `Debug` 里取变体名，不格式化载荷（载荷可能是消息正文）。
struct UpToParen(String);

impl fmt::Write for UpToParen {
    fn write_str(&mut self, s: &str) -> fmt::Result {
        match s.find(['(', ' ', '{']) {
            Some(i) => {
                self.0.push_str(&s[..i]);
                Err(fmt::Error)
            }
            None => {
                self.0.push_str(s);
                Ok(())
            }
        }
    }
}

/// oneof 变体名（如 `SendMessage`）。
pub fn variant_name(v: &impl fmt::Debug) -> String {
    let mut w = UpToParen(String::new());
    let _ = write!(w, "{v:?}");
    w.0
}

/// 错误响应。
pub fn error_response(
    request_id: u64,
    code: pb::ErrorCode,
    message: &str,
    retryable: bool,
) -> pb::Response {
    pb::Response {
        request_id,
        body: Some(pb::response::Body::Error(pb::Error {
            code: code as i32,
            message: message.to_owned(),
            retryable,
            details: Default::default(),
        })),
    }
}

/// T04 交付时的内层：真实受理归 T10，这里一律「未接入」。
#[derive(Debug, Clone, Copy, Default)]
pub struct NotWired;

impl RequestHandler for NotWired {
    async fn handle(&self, _ctx: &ConnectionContext, req: pb::Request) -> pb::Response {
        error_response(
            req.request_id,
            pb::ErrorCode::Unavailable,
            "未接入：命令与查询的受理还没有接上",
            false,
        )
    }

    async fn chunk(&self, _ctx: &ConnectionContext, _chunk: pb::Chunk) -> Result<(), ChunkRefused> {
        Err(ChunkRefused(pb::Error {
            code: pb::ErrorCode::Unavailable as i32,
            message: "未接入：分片上传还没有接上".into(),
            retryable: false,
            details: Default::default(),
        }))
    }

    async fn connection_closed(&self, _ctx: &ConnectionContext) {}
}

/// 后台的请求入口。
pub struct DaemonHandler<I> {
    gate: Arc<Gate>,
    inner: I,
}

impl<I: RequestHandler> DaemonHandler<I> {
    pub fn new(gate: Arc<Gate>, inner: I) -> Self {
        Self { gate, inner }
    }
}

const STOPPING: &str = "后台服务正在停止，请稍后重试";

impl<I: RequestHandler> RequestHandler for DaemonHandler<I> {
    async fn handle(&self, ctx: &ConnectionContext, req: pb::Request) -> pb::Response {
        let request_id = req.request_id;
        let span = match &req.body {
            Some(pb::request::Body::Command(c)) => logging::span::command(
                &c.command_id,
                &c.kind.as_ref().map(variant_name).unwrap_or_default(),
            ),
            Some(pb::request::Body::Query(q)) => tracing::info_span!(
                "query",
                kind = q.kind.as_ref().map(variant_name).unwrap_or_default()
            ),
            _ => tracing::Span::none(),
        };
        let Some(_ticket) = self.gate.enter() else {
            return error_response(request_id, pb::ErrorCode::Unavailable, STOPPING, true);
        };
        tracing::Instrument::instrument(self.inner.handle(ctx, req), span).await
    }

    async fn chunk(&self, ctx: &ConnectionContext, chunk: pb::Chunk) -> Result<(), ChunkRefused> {
        let Some(_ticket) = self.gate.enter() else {
            return Err(ChunkRefused(pb::Error {
                code: pb::ErrorCode::Unavailable as i32,
                message: STOPPING.into(),
                retryable: true,
                details: Default::default(),
            }));
        };
        self.inner.chunk(ctx, chunk).await
    }

    async fn connection_closed(&self, ctx: &ConnectionContext) {
        self.inner.connection_closed(ctx).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn variant_name_does_not_format_payload() {
        let kind = pb::command::Kind::SendMessage(pb::SendMessage {
            text: "SECRET".into(),
            ..Default::default()
        });
        assert_eq!(variant_name(&kind), "SendMessage");
        let unit = pb::command::Kind::ShutdownDaemon(pb::ShutdownDaemon {
            cancel_running: true,
        });
        assert_eq!(variant_name(&unit), "ShutdownDaemon");
    }

    #[tokio::test]
    async fn gate_rejects_after_close_and_waits_for_in_flight() {
        let gate = Arc::new(Gate::default());
        let t = gate.enter().unwrap();
        let g2 = gate.clone();
        let drain = tokio::spawn(async move { g2.close_and_drain(Duration::from_secs(5)).await });
        tokio::time::sleep(Duration::from_millis(50)).await;
        assert!(gate.enter().is_none(), "关门后不许进");
        assert!(!drain.is_finished(), "在途的还没回完");
        drop(t);
        assert_eq!(drain.await.unwrap(), 0);
    }

    #[tokio::test]
    async fn drain_gives_up_at_limit() {
        let gate = Gate::default();
        let _t = gate.enter().unwrap();
        assert_eq!(gate.close_and_drain(Duration::from_millis(30)).await, 1);
    }
}
