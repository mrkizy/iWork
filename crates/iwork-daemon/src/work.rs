//! 接缝 `RunningWork`（T04 方案 2.8；T10a 方案 E1–E3）：停机流程看得到、管得到的「在跑的东西」。回合宿主实现它。
//!
//! 给实现方的硬约束（第 11 节交给 T10）：
//! - `abort_all` 一进来就封口：之后任何开会话的请求立即失败、不起进程（停机第 1 步之前进门的命令晚到这里也一样）；
//! - `abort_all` 先撤回合信箱、停编排循环，再关会话：会话关闭不能被当成「运行时进程意外退出」，
//!   否则回合会记成 `Failed`、待审批被作废，违反「后台没了待审批保留」；
//! - `abort_all` 不提交任何事件；返回的 [`AbortReport`] 按 `SupervisedProcess::kill_tree_and_wait` 的结果如实填。

use std::future::Future;

pub use iwork_app::lifecycle::{QuitCounts, ShutdownDecision};

/// 停机第 3 步的结果。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct AbortReport {
    /// 关掉的会话数。
    pub sessions: u32,
    /// 到点还没确认清空进程组的会话数。只记日志、写进状态文件给诊断页，不拦停机（已知限制 L6）。
    pub not_cleared: u32,
}

pub trait RunningWork: Send + Sync + 'static {
    /// 退出前要问的数 N（E1）：读库，自己限时（[`QUIT_COUNT_LIMIT`]）。读不出来返回 `None`（「不知道」，不当成 0）。
    fn quit_counts(&self) -> impl Future<Output = Option<QuitCounts>> + Send;
    /// E1 的裁决，**一口气做完、中间没有等待点**：在宿主的锁里数出 L，和 N 一起判；要退就把宿主置成「完全退出」
    /// 并调 `trigger`（触发停机）。回合宿主、卡片宿主两把锁先后拿（卡片那一半由 T10e 接上）。
    fn try_quit(
        &self,
        confirmed: bool,
        counts: Option<QuitCounts>,
        trigger: impl FnOnce(),
    ) -> ShutdownDecision;
    /// 停机第 2 步（E2）：在跑的走取消，排着的取消；等它们结束。
    fn cancel_all(&self) -> impl Future<Output = ()> + Send;
    /// 停机第 3 步（E3）：封口，再关闭全部会话、结束各进程树并逐个核对进程组已空。不提交任何事件。
    fn abort_all(&self) -> impl Future<Output = AbortReport> + Send;
}

/// 读 N 最多等这么久（E1）。
pub const QUIT_COUNT_LIMIT: std::time::Duration = std::time::Duration::from_secs(2);

/// 没有任何在跑的东西（单测与没装宿主的组装用）。
#[derive(Debug, Clone, Copy, Default)]
pub struct NoWork;

impl RunningWork for NoWork {
    async fn quit_counts(&self) -> Option<QuitCounts> {
        Some(QuitCounts::default())
    }
    fn try_quit(
        &self,
        confirmed: bool,
        counts: Option<QuitCounts>,
        trigger: impl FnOnce(),
    ) -> ShutdownDecision {
        let d = iwork_app::lifecycle::decide_shutdown(iwork_app::lifecycle::QuitInputs {
            confirmed,
            counts,
            live: 0,
        });
        if d == ShutdownDecision::Proceed {
            trigger();
        }
        d
    }
    async fn cancel_all(&self) {}
    async fn abort_all(&self) -> AbortReport {
        AbortReport::default()
    }
}

/// 在 `bind` 之前才填好的「在跑的东西」：请求入口要先建好才有发布方，而回合宿主又要用发布方（同 `handler::Late`）。
/// 没填时等于 [`NoWork`]。停机第 6 步关存储之前要 [`LateWork::release`]：宿主经带闸的存储握着存储，不放手存储关不了。
pub struct LateWork<W>(std::sync::Mutex<Option<std::sync::Arc<W>>>);

impl<W> Default for LateWork<W> {
    fn default() -> Self {
        Self(std::sync::Mutex::new(None))
    }
}

impl<W> LateWork<W> {
    fn lock(&self) -> std::sync::MutexGuard<'_, Option<std::sync::Arc<W>>> {
        self.0
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    pub fn fill(&self, w: W) {
        *self.lock() = Some(std::sync::Arc::new(w));
    }

    /// 放手：之后等于 [`NoWork`]。
    pub fn release(&self) {
        self.lock().take();
    }

    fn current(&self) -> Option<std::sync::Arc<W>> {
        self.lock().clone()
    }
}

impl<W: RunningWork> RunningWork for LateWork<W> {
    async fn quit_counts(&self) -> Option<QuitCounts> {
        match self.current() {
            Some(w) => w.quit_counts().await,
            None => NoWork.quit_counts().await,
        }
    }
    fn try_quit(
        &self,
        confirmed: bool,
        counts: Option<QuitCounts>,
        trigger: impl FnOnce(),
    ) -> ShutdownDecision {
        match self.current() {
            Some(w) => w.try_quit(confirmed, counts, trigger),
            None => NoWork.try_quit(confirmed, counts, trigger),
        }
    }
    async fn cancel_all(&self) {
        if let Some(w) = self.current() {
            w.cancel_all().await;
        }
    }
    async fn abort_all(&self) -> AbortReport {
        match self.current() {
            Some(w) => w.abort_all().await,
            None => AbortReport::default(),
        }
    }
}
