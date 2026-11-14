//! 生命周期命令的受理（T04 方案 2.8、2.12）：`ShutdownDaemon`、`SetAutostart`。判定与受理在 `iwork_app::lifecycle`，
//! 这里只接线、把结果变成响应。

use iwork_app::details;
use iwork_app::lifecycle::ShutdownDecision;
use iwork_ipc::rendezvous::StopReason;
use iwork_ipc::server::ConnectionContext;
use iwork_proto::pb;

use super::error_response;
use crate::stop::StopSwitch;
use crate::work::RunningWork;

/// 能让后台完全退出的客户端：桌面客户端；开发构建另加开发工具（测试与 `ipc-tool` 用）。
fn may_shut_down(kind: pb::ClientKind) -> bool {
    match kind {
        pb::ClientKind::Desktop => true,
        #[cfg(feature = "dev")]
        pb::ClientKind::DevTool => true,
        _ => false,
    }
}

/// 受理 `ShutdownDaemon`（E1）：带了确认不读库；没带确认读 N（限时 2 秒，读不出来记成不知道）。然后 `try_quit`
/// 一口气判：要确认 → `PRECONDITION_FAILED` 带各个数与 `counts_unknown`，宿主不关；否则宿主置成完全退出并触发
/// 停机，回 `CommandAccepted`（不产生事件，两个 seq 都是 0；停机第 1 步会等这条回复送出）。
pub async fn shutdown(
    ctx: &ConnectionContext,
    request_id: u64,
    cmd: &pb::ShutdownDaemon,
    work: &impl RunningWork,
    stop: &StopSwitch,
) -> pb::Response {
    if !may_shut_down(ctx.client_kind) {
        return error_response(
            request_id,
            pb::ErrorCode::PermissionDenied,
            "只有桌面客户端能让后台服务完全退出",
            false,
        );
    }
    let confirmed = cmd.cancel_running;
    let counts = if confirmed {
        None
    } else {
        work.quit_counts().await
    };
    match work.try_quit(confirmed, counts, || {
        stop.request(StopReason::UserQuit);
    }) {
        ShutdownDecision::NeedsConfirmation {
            counts,
            counts_unknown,
        } => {
            let mut resp = error_response(
                request_id,
                pb::ErrorCode::PreconditionFailed,
                "还有回合在运行或排着，确认后以 cancel_running 重发",
                false,
            );
            if let Some(pb::response::Body::Error(e)) = &mut resp.body {
                for (k, v) in [
                    (details::RUNNING_TURNS, counts.running_turns.to_string()),
                    (details::QUEUED_TURNS, counts.queued_turns.to_string()),
                    (details::EXECUTING_CARDS, counts.executing_cards.to_string()),
                    (details::COUNTS_UNKNOWN, counts_unknown.to_string()),
                ] {
                    e.details.insert(k.into(), v);
                }
            }
            resp
        }
        ShutdownDecision::Proceed => pb::Response {
            request_id,
            body: Some(pb::response::Body::Accepted(pb::CommandAccepted {
                first_seq: 0,
                last_seq: 0,
                replayed: false,
            })),
        },
    }
}

/// `SetAutostart` 的受理（装配时由端口与存储造出，见 [`autostart_handler`]）。
pub type AutostartHandler = std::sync::Arc<
    dyn Fn(
            u64,
            String,
            bool,
        ) -> std::pin::Pin<Box<dyn std::future::Future<Output = pb::Response> + Send>>
        + Send
        + Sync,
>;

/// 造 `SetAutostart` 的受理：只提交意图（`iwork_app::lifecycle::set_autostart`），不碰系统。
pub fn autostart_handler<A, St>(
    port: std::sync::Arc<A>,
    store: std::sync::Arc<St>,
) -> AutostartHandler
where
    A: iwork_ports::autostart::AutostartPort + 'static,
    St: iwork_ports::store::StoragePort + Send + Sync + 'static,
{
    std::sync::Arc::new(move |request_id, command_id, enabled| {
        let (port, store) = (port.clone(), store.clone());
        Box::pin(async move {
            set_autostart(
                port.as_ref(),
                store.as_ref(),
                request_id,
                &command_id,
                enabled,
            )
            .await
        })
    })
}

/// 受理一次 `SetAutostart`，结果变成响应。
pub async fn set_autostart<A, St>(
    port: &A,
    store: &St,
    request_id: u64,
    command_id: &str,
    enabled: bool,
) -> pb::Response
where
    A: iwork_ports::autostart::AutostartPort,
    St: iwork_ports::store::StoragePort,
{
    use iwork_app::lifecycle::SetAutostartError;
    let Ok(id) = command_id.parse::<iwork_domain::id::CommandId>() else {
        return error_response(
            request_id,
            pb::ErrorCode::InvalidArgument,
            &format!("command_id 不是合法的 ULID：{command_id:?}"),
            false,
        );
    };
    match iwork_app::lifecycle::set_autostart(
        port,
        store,
        id,
        iwork_domain::common::Actor("user".into()),
        enabled,
    )
    .await
    {
        Ok(r) => pb::Response {
            request_id,
            body: Some(pb::response::Body::Accepted(pb::CommandAccepted {
                first_seq: r.first_seq.0,
                last_seq: r.last_seq.0,
                replayed: r.replayed,
            })),
        },
        Err(SetAutostartError::Unsupported(reason)) => error_response(
            request_id,
            pb::ErrorCode::PreconditionFailed,
            &reason,
            false,
        ),
        Err(SetAutostartError::Storage(e)) => {
            let code = if e.retryable() {
                pb::ErrorCode::Unavailable
            } else {
                pb::ErrorCode::Internal
            };
            error_response(request_id, code, &e.to_string(), e.retryable())
        }
    }
}
