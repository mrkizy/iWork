//! 回合编排（技术设计 6.4）：一条会话线上一个接一个地跑回合，回合状态只在这里写。

mod binding;
mod permission;
mod resume_note;
mod resume_summary;
mod runner;
mod scheduler;

pub use permission::{
    Decided, PermissionDecider, PermissionGate, STUB_DENY_MESSAGE, StubDecider, TURN_GONE_MESSAGE,
    VoidPlanError,
};
pub use resume_note::{ResumeReason, resume_note};
pub use resume_summary::{SUMMARY_HEADER, SummaryLimits, resume_summary};
pub use runner::{
    CHANGE_SUMMARY_LIMIT, CHECKPOINT_INTERVAL, FAIR_BURST, LIMIT_CHECK_INTERVAL,
    ORCHESTRATOR_ACTOR, REQUEST_HEADER, ResumeRequest, SUMMARY_RECENT_LIMIT, SessionLine,
    TREE_NOT_CLEARED, TurnCanceller, TurnEntry, TurnPlan, TurnPorts, TurnRunError,
};
