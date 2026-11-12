//! 生命周期命令的受理（T04 方案 2.8）：`ShutdownDaemon`。判定是 `iwork_app::lifecycle` 的纯函数，这里只接线。
//! `SetAutostart` 在组 3（方案第 11 节第 9 步）。

use iwork_app::details;
use iwork_app::lifecycle::{ShutdownDecision, decide_shutdown};
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

/// 受理 `ShutdownDaemon`：有成员在跑且没说要取消 → `PRECONDITION_FAILED` 带两个数；否则先回 `CommandAccepted`
/// （不产生事件，两个 seq 都是 0），再触发停机（停机第 1 步会等这条回复送出）。
pub fn shutdown(
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
    match decide_shutdown(work.running(), cmd.cancel_running) {
        ShutdownDecision::NeedsConfirmation {
            running_turns,
            executing_cards,
        } => {
            let mut resp = error_response(
                request_id,
                pb::ErrorCode::PreconditionFailed,
                "还有成员在运行，确认后以 cancel_running 重发",
                false,
            );
            if let Some(pb::response::Body::Error(e)) = &mut resp.body {
                e.details
                    .insert(details::RUNNING_TURNS.into(), running_turns.to_string());
                e.details
                    .insert(details::EXECUTING_CARDS.into(), executing_cards.to_string());
            }
            resp
        }
        ShutdownDecision::Proceed => {
            stop.request(StopReason::UserQuit);
            pb::Response {
                request_id,
                body: Some(pb::response::Body::Accepted(pb::CommandAccepted {
                    first_seq: 0,
                    last_seq: 0,
                    replayed: false,
                })),
            }
        }
    }
}
