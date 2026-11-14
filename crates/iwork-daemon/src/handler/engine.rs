//! 受理函数要用的引擎三件事（T10a 方案 2.6）：会牵动审批或卡片的提交、用户的审批决定、切档。
//!
//! 分发入口只依赖这个接缝，不带着引擎的六个类型参数；真实现是 [`ApprovalEngine`]，测试也用它。

use std::future::Future;

use iwork_core::approval::engine::{
    ApprovalEngine, Committed, DecideError, DecideOutcome, EngineError, ModeError, ModeSwitched,
    Scope, UserDecision,
};
use iwork_domain::approval_mode::ApprovalMode;
use iwork_domain::common::Actor;
use iwork_domain::event::ApprovalModeTarget;
use iwork_domain::id::{ApprovalId, CommandId};
use iwork_ports::agent::AgentPort;
use iwork_ports::clock::{Clock, IdGenerator, Timer};
use iwork_ports::fs::PathResolver;
use iwork_ports::store::{CommitRequest, StoragePort};

pub trait EngineCommands: Send + Sync + 'static {
    fn commit_with(
        &self,
        scope: Scope,
        req: CommitRequest,
    ) -> impl Future<Output = Result<Committed, EngineError>> + Send;
    fn decide(
        &self,
        id: ApprovalId,
        decision: UserDecision,
        by: Actor,
        command_id: CommandId,
    ) -> impl Future<Output = Result<DecideOutcome, DecideError>> + Send;
    fn switch_mode(
        &self,
        target: ApprovalModeTarget,
        mode: Option<ApprovalMode>,
        by: Actor,
        command_id: CommandId,
    ) -> impl Future<Output = Result<ModeSwitched, ModeError>> + Send;
}

impl<C, G, S, A, T, R> EngineCommands for ApprovalEngine<C, G, S, A, T, R>
where
    C: Clock + 'static,
    G: IdGenerator + 'static,
    S: StoragePort + 'static,
    A: AgentPort + 'static,
    T: Timer + 'static,
    R: PathResolver + 'static,
{
    fn commit_with(
        &self,
        scope: Scope,
        req: CommitRequest,
    ) -> impl Future<Output = Result<Committed, EngineError>> + Send {
        ApprovalEngine::commit_with(self, scope, req)
    }

    fn decide(
        &self,
        id: ApprovalId,
        decision: UserDecision,
        by: Actor,
        command_id: CommandId,
    ) -> impl Future<Output = Result<DecideOutcome, DecideError>> + Send {
        ApprovalEngine::decide(self, id, decision, by, command_id)
    }

    fn switch_mode(
        &self,
        target: ApprovalModeTarget,
        mode: Option<ApprovalMode>,
        by: Actor,
        command_id: CommandId,
    ) -> impl Future<Output = Result<ModeSwitched, ModeError>> + Send {
        ApprovalEngine::switch_mode(self, target, mode, by, command_id)
    }
}
