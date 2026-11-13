//! 请求入口（T04 方案 2.4 第 9 步、2.8 第 1 步、2.14）：每个请求开 `command` span；停机期间关门；
//! 生命周期命令自己受理，其余交给注入的内层 handler。
//!
//! 内层是 [`dispatch::Dispatcher`]（T10a 方案 2.6：在途表、查回执、分发表；每条命令的外壳在 `turn`、
//! `conversation`、`approval`、`settings`）。没装配时注入 [`NotWired`]：一律回 `ERROR_CODE_UNAVAILABLE`「未接入」。

use std::fmt::{self, Write as _};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use iwork_ipc::server::{ChunkRefused, ConnectionContext, RequestHandler};
use iwork_proto::pb;

use crate::logging;
use crate::stop::StopSwitch;
use crate::work::RunningWork;

mod approval;
mod conversation;
pub mod dispatch;
pub mod engine;
pub mod lifecycle;
mod settings;
mod turn;

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
pub struct DaemonHandler<I, W> {
    gate: Arc<Gate>,
    inner: I,
    work: Arc<W>,
    stop: StopSwitch,
    autostart: Option<lifecycle::AutostartHandler>,
}

impl<I: RequestHandler, W: RunningWork> DaemonHandler<I, W> {
    pub fn new(gate: Arc<Gate>, inner: I, work: Arc<W>, stop: StopSwitch) -> Self {
        Self {
            gate,
            inner,
            work,
            stop,
            autostart: None,
        }
    }

    /// 接上 `SetAutostart` 的受理（没接时它交给内层，T04 的内层回「未接入」）。
    pub fn with_autostart(mut self, h: lifecycle::AutostartHandler) -> Self {
        self.autostart = Some(h);
        self
    }
}

const STOPPING: &str = "后台服务正在停止，请稍后重试";

impl<I: RequestHandler, W: RunningWork> RequestHandler for DaemonHandler<I, W> {
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
        // 只记 span 里的种类与 command_id，不记载荷（载荷可能是消息正文）
        span.in_scope(|| tracing::debug!(request_id, "收到请求"));
        let Some(_ticket) = self.gate.enter() else {
            return error_response(request_id, pb::ErrorCode::Unavailable, STOPPING, true);
        };
        if let Some(pb::request::Body::Command(pb::Command {
            kind: Some(pb::command::Kind::ShutdownDaemon(cmd)),
            ..
        })) = &req.body
        {
            let _e = span.enter();
            return lifecycle::shutdown(ctx, request_id, cmd, self.work.as_ref(), &self.stop);
        }
        if let (
            Some(pb::request::Body::Command(pb::Command {
                command_id,
                kind: Some(pb::command::Kind::SetAutostart(cmd)),
            })),
            Some(h),
        ) = (&req.body, &self.autostart)
        {
            let fut = h(request_id, command_id.clone(), cmd.enabled);
            return tracing::Instrument::instrument(fut, span).await;
        }
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

    use crate::work::{AbortReport, RunningCount};
    use iwork_ipc::peer::UserId;
    use iwork_ipc::rendezvous::StopReason;
    use iwork_ipc::server::PeerSummary;

    struct Busy(RunningCount);
    impl RunningWork for Busy {
        fn running(&self) -> RunningCount {
            self.0
        }
        async fn cancel_all(&self) {}
        async fn abort_all(&self) -> AbortReport {
            AbortReport::default()
        }
    }

    fn ctx(kind: pb::ClientKind) -> ConnectionContext {
        ConnectionContext {
            connection_id: 1,
            session_id: "s".into(),
            peer: PeerSummary {
                pid: 1,
                user: UserId::Uid(1),
            },
            client_kind: kind,
            signature_checked: true,
        }
    }

    fn shutdown_req(cancel_running: bool) -> pb::Request {
        pb::Request {
            request_id: 9,
            body: Some(pb::request::Body::Command(pb::Command {
                command_id: "c".into(),
                kind: Some(pb::command::Kind::ShutdownDaemon(pb::ShutdownDaemon {
                    cancel_running,
                })),
            })),
        }
    }

    fn handler(running: RunningCount) -> (DaemonHandler<NotWired, Busy>, StopSwitch, Arc<Gate>) {
        let stop = StopSwitch::new();
        let gate = Arc::new(Gate::default());
        (
            DaemonHandler::new(
                gate.clone(),
                NotWired,
                Arc::new(Busy(running)),
                stop.clone(),
            ),
            stop,
            gate,
        )
    }

    fn error_of(r: &pb::Response) -> &pb::Error {
        match &r.body {
            Some(pb::response::Body::Error(e)) => e,
            other => panic!("{other:?}"),
        }
    }

    #[tokio::test]
    async fn shutdown_needs_confirmation_when_running() {
        let (h, stop, _) = handler(RunningCount { turns: 2, cards: 1 });
        let r = h
            .handle(&ctx(pb::ClientKind::Desktop), shutdown_req(false))
            .await;
        let e = error_of(&r);
        assert_eq!(e.code, pb::ErrorCode::PreconditionFailed as i32);
        assert_eq!(e.details[iwork_app::details::RUNNING_TURNS], "2");
        assert_eq!(e.details[iwork_app::details::EXECUTING_CARDS], "1");
        assert_eq!(stop.requested(), None, "没确认就不停");

        let r = h
            .handle(&ctx(pb::ClientKind::Desktop), shutdown_req(true))
            .await;
        assert!(matches!(
            r.body,
            Some(pb::response::Body::Accepted(pb::CommandAccepted {
                first_seq: 0,
                last_seq: 0,
                replayed: false
            }))
        ));
        assert_eq!(r.request_id, 9);
        assert_eq!(stop.requested(), Some(StopReason::UserQuit));
    }

    #[tokio::test]
    async fn shutdown_refused_from_non_desktop_client() {
        let (h, stop, _) = handler(RunningCount::default());
        let mut refused = vec![pb::ClientKind::Unspecified];
        if !cfg!(feature = "dev") {
            refused.push(pb::ClientKind::DevTool);
        }
        for kind in refused {
            let r = h.handle(&ctx(kind), shutdown_req(true)).await;
            assert_eq!(
                error_of(&r).code,
                pb::ErrorCode::PermissionDenied as i32,
                "{kind:?}"
            );
        }
        assert_eq!(stop.requested(), None);
        // 对照：桌面客户端、没有在跑的东西 → 直接受理
        let r = h
            .handle(&ctx(pb::ClientKind::Desktop), shutdown_req(false))
            .await;
        assert!(matches!(r.body, Some(pb::response::Body::Accepted(_))));
    }

    #[tokio::test]
    async fn commands_rejected_while_stopping() {
        let (h, _stop, gate) = handler(RunningCount::default());
        gate.close_and_drain(Duration::from_millis(10)).await;
        for req in [
            shutdown_req(true),
            pb::Request {
                request_id: 3,
                body: Some(pb::request::Body::Query(pb::Query { kind: None })),
            },
        ] {
            let r = h.handle(&ctx(pb::ClientKind::Desktop), req).await;
            let e = error_of(&r);
            assert_eq!(e.code, pb::ErrorCode::Unavailable as i32);
            assert!(e.retryable, "停机期间的拒绝可重试");
        }
    }

    #[tokio::test]
    async fn drain_gives_up_at_limit() {
        let gate = Gate::default();
        let _t = gate.enter().unwrap();
        assert_eq!(gate.close_and_drain(Duration::from_millis(30)).await, 1);
    }
}
