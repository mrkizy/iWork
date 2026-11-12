//! 接缝 `RunningWork`（T04 方案 2.8）：停机流程看得到、管得到的「在跑的东西」。T10 的回合宿主实现它；
//! T04 交付时注入 [`NoWork`]。
//!
//! 给实现方的硬约束（第 11 节交给 T10）：
//! - `abort_all` 一进来就封口：之后任何开会话的请求立即失败、不起进程（停机第 1 步之前进门的命令晚到这里也一样）；
//! - `abort_all` 先撤回合信箱、停编排循环，再关会话：会话关闭不能被当成「运行时进程意外退出」，
//!   否则回合会记成 `Failed`、待审批被作废，违反「后台没了待审批保留」；
//! - `abort_all` 不提交任何事件；返回的 [`AbortReport`] 按 `SupervisedProcess::kill_tree_and_wait` 的结果如实填。

use std::future::Future;

pub use iwork_app::lifecycle::RunningCount;

/// 停机第 3 步的结果。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct AbortReport {
    /// 关掉的会话数。
    pub sessions: u32,
    /// 到点还没确认清空进程组的会话数。只记日志、写进状态文件给诊断页，不拦停机（已知限制 L6）。
    pub not_cleared: u32,
}

pub trait RunningWork: Send + Sync + 'static {
    /// 持有进程的回合数、执行中的卡片数。
    fn running(&self) -> RunningCount;
    /// 全部回合走取消流程、执行中的卡片经句柄停止；等它们结束。
    fn cancel_all(&self) -> impl Future<Output = ()> + Send;
    /// 封口，再关闭全部会话、结束各进程树并逐个核对进程组已空。不提交任何事件。
    fn abort_all(&self) -> impl Future<Output = AbortReport> + Send;
}

/// T04 交付时：没有任何在跑的东西。
#[derive(Debug, Clone, Copy, Default)]
pub struct NoWork;

impl RunningWork for NoWork {
    fn running(&self) -> RunningCount {
        RunningCount::default()
    }
    async fn cancel_all(&self) {}
    async fn abort_all(&self) -> AbortReport {
        AbortReport::default()
    }
}
