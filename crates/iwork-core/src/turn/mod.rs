//! 回合编排（技术设计 6.4）：一条会话线上一个接一个地跑回合，回合状态只在这里写。

mod binding;
mod permission;
mod resume_note;
mod runner;
mod scheduler;

pub use permission::{
    Decided, PermissionDecider, PermissionGate, STUB_DENY_MESSAGE, StubDecider, TURN_GONE_MESSAGE,
    VoidPlanError,
};
pub use resume_note::{ResumeReason, resume_note};
pub use runner::{
    CHECKPOINT_INTERVAL, FAIR_BURST, LIMIT_CHECK_INTERVAL, ORCHESTRATOR_ACTOR, SessionLine,
    TREE_NOT_CLEARED, TurnCanceller, TurnPlan, TurnPorts, TurnRunError,
};
