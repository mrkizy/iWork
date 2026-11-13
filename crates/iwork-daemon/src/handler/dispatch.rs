//! 分发入口（T10a 方案 A1、A2，2.6）：命令受理的那一刻就是它那笔提交落盘的那一刻。
//!
//! 每条命令三步：① 同一个 `command_id` 一次只进一份（在途表，后到的等前一份离开）；② 轮到自己先查回执，
//! 这次查**排在存储的写队列里**（看得到前一份被中止后还在路上的那一笔），查到回 `replayed`；③ 没查到才进
//! 受理函数：取数 → `iwork-app` 里的纯函数 → 一笔提交。提交之后要做的事一律不放在返回路径上（ipc 在客户端
//! 断开时会在任何一个等待点中止受理函数）。
//!
//! 被提交时校验（A3、A4）拒掉的重读重算，最多 [`REPLAN_ATTEMPTS`] 次，还不行回 `CONFLICT`（可重试）。

use std::collections::HashMap;
use std::future::Future;
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use iwork_app::refusal::Refusal;
use iwork_core::approval::engine::{DecideError, EngineError, ModeError};
use iwork_domain::common::Actor;
use iwork_domain::id::CommandId;
use iwork_ipc::server::{ChunkRefused, ConnectionContext, RequestHandler};
use iwork_ports::clock::{Clock, IdGenerator};
use iwork_ports::read_model::ReadModelPort;
use iwork_ports::store::{CommitReceipt, StorageError, StoragePort};
use iwork_proto::pb;
use tokio::sync::Notify;

use super::engine::EngineCommands;
use super::error_response;

/// 等同一个 `command_id` 的前一份最多这么久（A2 ①），到时回可重试的 `UNAVAILABLE`。
pub const SAME_ID_WAIT: Duration = Duration::from_secs(10);
/// 被提交时校验拒了重读重算的次数上限（A4）。
pub const REPLAN_ATTEMPTS: usize = 3;

/// 受理路径读内存的唯一一处（A6）：回合宿主关了没有。关了，`SendMessage`、`ResumeTurn` 提前回 `UNAVAILABLE`。
/// 这一处读内存只会多拒，不会多放。
pub trait HostState: Send + Sync + 'static {
    fn is_closed(&self) -> bool;
}

/// 受理函数用到的端口。存储是带封口闸的那一个（E4）。
pub struct CommandPorts<St, Rd, E> {
    pub store: Arc<St>,
    pub read: Arc<Rd>,
    pub engine: Arc<E>,
    pub ids: Arc<dyn IdGenerator>,
    pub clock: Arc<dyn Clock>,
    pub host: Arc<dyn HostState>,
}

/// 受理一条命令没成。
#[derive(Debug, thiserror::Error)]
pub enum CommandError {
    #[error(transparent)]
    Refused(#[from] Refusal),
    #[error(transparent)]
    Storage(#[from] StorageError),
    /// 被提交时校验拒了 [`REPLAN_ATTEMPTS`] 次
    #[error("状态刚被改过，请重试")]
    Conflict,
    #[error(transparent)]
    Decide(#[from] DecideError),
    #[error(transparent)]
    Mode(#[from] ModeError),
}

impl From<EngineError> for CommandError {
    fn from(e: EngineError) -> Self {
        match e {
            EngineError::Storage(e) => Self::Storage(e),
            EngineError::Conflict => Self::Conflict,
            EngineError::Fold(e) => Self::Decide(DecideError::Fold(e)),
        }
    }
}

impl From<iwork_domain::DomainError> for CommandError {
    fn from(e: iwork_domain::DomainError) -> Self {
        Self::Refused(e.into())
    }
}

impl From<iwork_app::convert::ConvertError> for CommandError {
    fn from(e: iwork_app::convert::ConvertError) -> Self {
        Self::Refused(e.into())
    }
}

fn storage_code(e: &StorageError) -> pb::ErrorCode {
    match e {
        StorageError::Busy => pb::ErrorCode::Unavailable,
        // 走到这里的提交时拒绝都已经重算过了
        StorageError::Projection { .. } => pb::ErrorCode::Conflict,
        StorageError::NotFound { .. } => pb::ErrorCode::NotFound,
        StorageError::Io { .. }
        | StorageError::Codec(_)
        | StorageError::ProtocolMajorMismatch { .. } => pb::ErrorCode::StorageFailure,
    }
}

fn decide_code(e: &DecideError) -> (pb::ErrorCode, bool) {
    use pb::ErrorCode as C;
    match e {
        DecideError::NotPending(_) => (C::ApprovalNotPending, false),
        DecideError::RememberNotOffered { .. } => (C::InvalidArgument, false),
        DecideError::NoLongerAllowed(_)
        | DecideError::OwnerGone(_)
        | DecideError::SessionUnavailable(_) => (C::PreconditionFailed, false),
        DecideError::Conflict => (C::Conflict, true),
        DecideError::Storage(s) => (storage_code(s), s.retryable()),
        DecideError::Fold(_) | DecideError::Apply(_) | DecideError::Rules(_) => {
            (C::Internal, false)
        }
    }
}

impl CommandError {
    pub fn to_error(&self) -> pb::Error {
        if let Self::Refused(r) = self {
            return r.to_error();
        }
        let (code, retryable) = match self {
            Self::Refused(_) => unreachable!("上面已返回"),
            Self::Storage(e) => (
                storage_code(e),
                e.retryable() || matches!(e, StorageError::Projection { .. }),
            ),
            Self::Conflict => (pb::ErrorCode::Conflict, true),
            Self::Decide(e) => decide_code(e),
            Self::Mode(e) => match e {
                ModeError::FullAccessNotAllowedAsDefault => {
                    (pb::ErrorCode::ApprovalModeNotAllowed, false)
                }
                ModeError::FollowOnlyForMember => (pb::ErrorCode::InvalidArgument, false),
                ModeError::ConversationNotFound(_) => (pb::ErrorCode::NotFound, false),
                ModeError::Storage(s) => (storage_code(s), s.retryable()),
                ModeError::Decision(d) => decide_code(d),
            },
        };
        pb::Error {
            code: code.into(),
            message: self.to_string(),
            retryable,
            details: Default::default(),
        }
    }
}

/// 被提交时校验拒了（`Projection`）就重来，最多 [`REPLAN_ATTEMPTS`] 次。每次重来都重读、重算。
pub async fn with_replan<F, Fut>(mut attempt: F) -> Result<CommitReceipt, CommandError>
where
    F: FnMut() -> Fut,
    Fut: Future<Output = Result<CommitReceipt, CommandError>>,
{
    for _ in 0..REPLAN_ATTEMPTS {
        match attempt().await {
            Err(CommandError::Storage(StorageError::Projection { seq, message })) => {
                tracing::info!(seq = seq.0, %message, "提交被状态前提拒了，重读重算");
            }
            other => return other,
        }
    }
    Err(CommandError::Conflict)
}

// ---------- 在途表（A2 ①）----------

/// 同一个 `command_id` 一次只进一份。只在内存里：进程没了所有受理函数一起没了，不需要恢复。
#[derive(Default)]
pub struct InFlight {
    by_id: Mutex<HashMap<CommandId, Arc<Notify>>>,
}

/// 占着一个 `command_id`。丢弃时出表并叫醒在等的（受理函数被中止也一样）。
pub struct Slot<'a> {
    table: &'a InFlight,
    id: CommandId,
}

impl Drop for Slot<'_> {
    fn drop(&mut self) {
        let left = self
            .table
            .by_id
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .remove(&self.id);
        if let Some(n) = left {
            n.notify_waiters();
        }
    }
}

impl InFlight {
    /// 进表。已经有人占着就等它离开再重查；`on_wait` 在「确认有人占着、自己准备开始等」的那一下调用
    /// （测试的可观测点，7.2a 表下第一条）。
    pub async fn enter(&self, id: CommandId, on_wait: impl Fn()) -> Slot<'_> {
        loop {
            let left = {
                let mut by_id = self.by_id.lock().unwrap_or_else(PoisonError::into_inner);
                match by_id.get(&id) {
                    None => {
                        by_id.insert(id, Arc::new(Notify::new()));
                        return Slot { table: self, id };
                    }
                    Some(n) => n.clone(),
                }
            };
            // `Notified` 从建好的那一刻起就收得到 `notify_waiters`。建好之后再看一眼：前一份要是在出锁到这里
            // 之间已经离开（通知已经发过），重查一遍，不白等
            let notified = left.notified();
            if !self.is_held_by(id, &left) {
                continue;
            }
            on_wait();
            notified.await;
        }
    }

    /// 表里这个 id 还是同一份占着（建 `Notified` 之前它可能已经离开）。
    fn is_held_by(&self, id: CommandId, n: &Arc<Notify>) -> bool {
        self.by_id
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .get(&id)
            .is_some_and(|cur| Arc::ptr_eq(cur, n))
    }

    #[cfg(any(test, feature = "test-hooks"))]
    pub fn waiting_len(&self) -> usize {
        self.by_id
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .values()
            .map(|n| Arc::strong_count(n) - 1)
            .sum()
    }
}

// ---------- 测试的可观测点（只在测试构建里有，做法同 `dev::pause_at`）----------

#[cfg(any(test, feature = "test-hooks"))]
pub type HookFuture = std::pin::Pin<Box<dyn Future<Output = ()> + Send>>;

/// 交错测试用屏障和通知定先后，不靠 `sleep`（T10a 方案 7.2a 表下）。
#[cfg(any(test, feature = "test-hooks"))]
#[derive(Default)]
pub struct DispatchHooks {
    /// K1：确认同一个 `command_id` 已经有人占着、自己准备开始等
    pub waiting_for_same_id: Option<Box<dyn Fn(CommandId) + Send + Sync>>,
    /// K1、K2 的屏障二：查完回执、没查到、进受理函数读业务状态之前
    pub before_handler: Option<Box<dyn Fn(CommandId) -> HookFuture + Send + Sync>>,
    /// 受理函数读完、算好、直接提交之前（A3 的窗口：读到的状态和提交时的不一样）
    pub before_commit: Option<Box<dyn Fn(CommandId) -> HookFuture + Send + Sync>>,
}

// ---------- 分发 ----------

/// 命令与查询的受理（`DaemonHandler` 的内层）。`ShutdownDaemon`、`SetAutostart` 在外层受理，不到这里。
pub struct Dispatcher<St, Rd, E> {
    pub(super) ports: CommandPorts<St, Rd, E>,
    in_flight: InFlight,
    #[cfg(any(test, feature = "test-hooks"))]
    hooks: DispatchHooks,
}

pub(super) const USER: &str = "user";

pub(super) fn user() -> Actor {
    Actor(USER.into())
}

fn accepted(request_id: u64, r: CommitReceipt) -> pb::Response {
    pb::Response {
        request_id,
        body: Some(pb::response::Body::Accepted(pb::CommandAccepted {
            first_seq: r.first_seq.0,
            last_seq: r.last_seq.0,
            replayed: r.replayed,
        })),
    }
}

fn failed(request_id: u64, e: pb::Error) -> pb::Response {
    pb::Response {
        request_id,
        body: Some(pb::response::Body::Error(e)),
    }
}

impl<St, Rd, E> Dispatcher<St, Rd, E>
where
    St: StoragePort + 'static,
    Rd: ReadModelPort + 'static,
    E: EngineCommands,
{
    pub fn new(ports: CommandPorts<St, Rd, E>) -> Self {
        Self {
            ports,
            in_flight: InFlight::default(),
            #[cfg(any(test, feature = "test-hooks"))]
            hooks: DispatchHooks::default(),
        }
    }

    #[cfg(any(test, feature = "test-hooks"))]
    pub fn with_hooks(mut self, hooks: DispatchHooks) -> Self {
        self.hooks = hooks;
        self
    }

    #[cfg(any(test, feature = "test-hooks"))]
    pub fn in_flight(&self) -> &InFlight {
        &self.in_flight
    }

    #[cfg(any(test, feature = "test-hooks"))]
    pub(super) async fn before_commit(&self, id: CommandId) {
        if let Some(h) = &self.hooks.before_commit {
            h(id).await;
        }
    }

    async fn command(&self, request_id: u64, cmd: pb::Command) -> pb::Response {
        let Ok(id) = cmd.command_id.parse::<CommandId>() else {
            return error_response(
                request_id,
                pb::ErrorCode::InvalidArgument,
                &format!("command_id 不是合法的 ULID：{:?}", cmd.command_id),
                false,
            );
        };
        let Some(kind) = cmd.kind else {
            return error_response(
                request_id,
                pb::ErrorCode::InvalidArgument,
                "Command.kind 缺失",
                false,
            );
        };
        if let Some(what) = unwired(&kind) {
            return failed(request_id, Refusal::Unwired { what }.to_error());
        }
        // A2 ①：同一个 id 一次只进一份
        let entered = tokio::time::timeout(
            SAME_ID_WAIT,
            self.in_flight.enter(id, || {
                #[cfg(any(test, feature = "test-hooks"))]
                if let Some(h) = &self.hooks.waiting_for_same_id {
                    h(id);
                }
            }),
        )
        .await;
        let Ok(_slot) = entered else {
            return error_response(
                request_id,
                pb::ErrorCode::Unavailable,
                "同一条命令的上一份还没处理完，请稍后重试",
                true,
            );
        };
        // A2 ②：排在写队列里查回执
        match self.ports.store.command_receipt(id).await {
            Ok(Some(r)) => return accepted(request_id, r),
            Ok(None) => {}
            Err(e) => return failed(request_id, CommandError::Storage(e).to_error()),
        }
        #[cfg(any(test, feature = "test-hooks"))]
        if let Some(h) = &self.hooks.before_handler {
            h(id).await;
        }
        match self.accept(id, kind).await {
            Ok(r) => accepted(request_id, r),
            Err(e) => {
                tracing::info!(error = %e, "命令没受理");
                failed(request_id, e.to_error())
            }
        }
    }

    /// 分发表：每条命令一个外壳。加一条命令 = 这里一行加一个外壳函数（`unwired` 里删掉它）。
    async fn accept(
        &self,
        id: CommandId,
        kind: pb::command::Kind,
    ) -> Result<CommitReceipt, CommandError> {
        use pb::command::Kind as K;
        match kind {
            K::SendMessage(c) => self.send_message(id, c).await,
            K::CancelTurn(c) => self.cancel_turn(id, c).await,
            K::CancelAll(c) => self.cancel_all(id, c).await,
            K::ResumeTurn(c) => self.resume_turn(id, c).await,
            K::AbandonTurn(c) => self.abandon_turn(id, c).await,
            K::DeleteConversation(c) => self.delete_conversation(id, c).await,
            K::CreateConversation(c) => self.create_conversation(id, c).await,
            K::MarkRead(c) => self.mark_read(id, c).await,
            K::SetConversationModel(c) => self.set_conversation_model(id, c).await,
            K::DecideApproval(c) => self.decide_approval(id, c).await,
            K::SetApprovalMode(c) => self.set_approval_mode(id, c).await,
            K::RevokeRule(c) => self.revoke_rule(id, c).await,
            K::SetPlayfulUi(c) => self.set_playful_ui(id, c).await,
            K::SetProjectsRoot(c) => self.set_projects_root(id, c).await,
            K::UpdateProjectSettings(c) => self.update_project_settings(id, c).await,
            other => unreachable!(
                "{} 在 unwired 里拦下了",
                iwork_app::admission::command_name(&other)
            ),
        }
    }
}

/// 还没接上的命令（归别的卡）。返回 `Some(是什么)` = 回「未接入」。
fn unwired(kind: &pb::command::Kind) -> Option<&'static str> {
    use pb::command::Kind as K;
    match kind {
        K::SendMessage(c) if !c.attachment_ids.is_empty() => Some("附件（T10d）"),
        K::SendMessage(_)
        | K::CancelTurn(_)
        | K::CancelAll(_)
        | K::ResumeTurn(_)
        | K::AbandonTurn(_)
        | K::DeleteConversation(_)
        | K::CreateConversation(_)
        | K::MarkRead(_)
        | K::SetConversationModel(_)
        | K::DecideApproval(_)
        | K::SetApprovalMode(_)
        | K::RevokeRule(_)
        | K::SetPlayfulUi(_)
        | K::SetProjectsRoot(_)
        | K::UpdateProjectSettings(_) => None,
        K::RenameConversation(_) | K::SwitchDmProject(_) => Some("对话管理（T10b）"),
        K::CreateAssistant(_)
        | K::UpdateAssistant(_)
        | K::DeleteAssistant(_)
        | K::ResetAssistant(_) => Some("助手管理（T10b）"),
        K::AddProject(_)
        | K::CreateProject(_)
        | K::InitRepository(_)
        | K::CloneRepository(_)
        | K::RelocateProject(_)
        | K::PinProject(_)
        | K::RemoveProject(_)
        | K::ConfirmAgentsMdDraft(_) => Some("项目管理（T10c）"),
        K::UploadAttachment(_) | K::ConfirmCredentialAttachment(_) => Some("附件（T10d）"),
        K::ExecuteUserAction(_)
        | K::DismissUserAction(_)
        | K::StopUserAction(_)
        | K::ConfirmUserActionEnded(_)
        | K::AddSecurityListEntry(_)
        | K::RemoveSecurityListEntry(_) => Some("亲手执行与安全清单（T10e）"),
        K::ImportLocalConfig(_) | K::SetMcpSecret(_) | K::DismissLocalImportPrompt(_) => {
            Some("本机导入（T10f）")
        }
        K::RedetectRuntime(_) => Some("运行时（T10g）"),
        K::ReportPresence(_) => Some("在看哪（T10a 步 8）"),
        // 在外层受理；外层没接时落到这里
        K::SetAutostart(_) | K::ShutdownDaemon(_) => Some("生命周期命令"),
    }
}

impl<St, Rd, E> RequestHandler for Dispatcher<St, Rd, E>
where
    St: StoragePort + 'static,
    Rd: ReadModelPort + 'static,
    E: EngineCommands,
{
    async fn handle(&self, _ctx: &ConnectionContext, req: pb::Request) -> pb::Response {
        match req.body {
            Some(pb::request::Body::Command(c)) => self.command(req.request_id, c).await,
            // 查询与快照归步 7
            _ => error_response(
                req.request_id,
                pb::ErrorCode::Unavailable,
                "未接入：查询还没有接上（T10a 步 7）",
                false,
            ),
        }
    }

    async fn chunk(&self, _ctx: &ConnectionContext, _chunk: pb::Chunk) -> Result<(), ChunkRefused> {
        Err(ChunkRefused(
            Refusal::Unwired {
                what: "分片上传（T10d）",
            }
            .to_error(),
        ))
    }

    async fn connection_closed(&self, _ctx: &ConnectionContext) {}
}
