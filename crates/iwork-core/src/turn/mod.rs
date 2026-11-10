//! 回合编排（技术设计 6.4）：一条会话线上一个接一个地跑回合，回合状态只在这里写。

mod permission;
mod runner;

pub use permission::{
    Decided, PermissionDecider, PermissionGate, STUB_DENY_MESSAGE, StubDecider, TURN_GONE_MESSAGE,
    VoidPlanError,
};
pub use runner::{
    CHECKPOINT_INTERVAL, FAIR_BURST, ORCHESTRATOR_ACTOR, SessionLine, TurnCanceller, TurnPlan,
    TurnPorts, TurnRunError,
};
