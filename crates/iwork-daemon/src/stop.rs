//! 停机流程（T04 方案 2.8；设计 4「退出」「升级」；设计 10.4）。四种原因走同一张表，差别只在勾不勾某一步：
//!
//! | # | 步骤 | 上限 | 用户退出 | 信号 | 升级 | 故障 |
//! | --- | --- | --- | --- | --- | --- | --- |
//! | 1 | 写 `Stopping{reason}`；关门，等在途请求回完 | 1s | ✓ | ✓ | ✓ | ✓ |
//! | 2 | `work.cancel_all()` | 5s | ✓ | — | — | — |
//! | 3 | `work.abort_all()`：封口、关会话、核对进程组已空 | 3s | ✓ | ✓ | ✓ | ✓ |
//! | 4 | 停 `serve` 以外的任务 | 1s | ✓ | ✓ | ✓ | ✓ |
//! | 5 | 取消 `serve`：停止接受连接，每条连接写完队列再关 | 2s | ✓ | ✓ | ✓ | ✓ |
//! | 6 | `maintenance()` → 关存储 | 2s | ✓ | ✓ | ✓ | 存储失效时跳过 |
//! | 7 | 写 `Exited{code, reason}` | — | ✓ | ✓ | ✓ | ✓ |
//!
//! 第 8 步（退出或 `exec`）由调用方做。整个流程另有 15s 硬上限，由一个普通线程看着（[`arm_hard_deadline`]）。

use std::future::Future;
use std::time::Duration;

use iwork_ipc::rendezvous::{Phase, StopReason};
use iwork_ports::store::StorageError;

use crate::dev;
use crate::exit::{process_status, stop_code};
use crate::handler::Gate;
use crate::status::StatusBoard;
use crate::work::RunningWork;

/// 停机请求的开关：任何一方（信号、`ShutdownDaemon`、安装检查、开发构建的触发器）都可以请求，第一次的原因生效。
#[derive(Clone)]
pub struct StopSwitch(tokio::sync::watch::Sender<Option<StopReason>>);

impl StopSwitch {
    pub fn new() -> Self {
        Self(tokio::sync::watch::Sender::new(None))
    }

    /// 请求停机。已经有人请求过时不改原因，返回 `false`。
    pub fn request(&self, reason: StopReason) -> bool {
        self.0.send_if_modified(|r| {
            if r.is_some() {
                return false;
            }
            *r = Some(reason);
            true
        })
    }

    pub fn requested(&self) -> Option<StopReason> {
        *self.0.borrow()
    }

    /// 等到有人请求停机。
    pub async fn wait(&self) -> StopReason {
        let mut rx = self.0.subscribe();
        let r = rx
            .wait_for(Option::is_some)
            .await
            .expect("发送端就在 self 里，不会被丢弃");
        r.expect("wait_for 保证是 Some")
    }
}

impl Default for StopSwitch {
    fn default() -> Self {
        Self::new()
    }
}

/// 停机表的步骤（第 7 步写状态不会跳过，不列）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StopStep {
    CloseGate,
    CancelAll,
    AbortAll,
    StopTasks,
    StopIpc,
    CloseStore,
}

impl StopStep {
    /// 这一步的上限。
    pub fn limit(self) -> Duration {
        Duration::from_secs(match self {
            StopStep::CloseGate | StopStep::StopTasks => 1,
            StopStep::CancelAll => 5,
            StopStep::AbortAll => 3,
            StopStep::StopIpc | StopStep::CloseStore => 2,
        })
    }
}

/// 纯函数：这一步在这个原因下做不做。
pub fn applies(step: StopStep, reason: StopReason, store_failed: bool) -> bool {
    match step {
        StopStep::CancelAll => reason == StopReason::UserQuit,
        StopStep::CloseStore => !store_failed,
        StopStep::CloseGate | StopStep::AbortAll | StopStep::StopTasks | StopStep::StopIpc => true,
    }
}

/// 整个停机流程的硬上限。
pub const HARD_LIMIT: Duration = Duration::from_secs(15);

/// 开发构建的暂停点：停在第 3 步（停机卡住的用例）。
pub const PAUSE_IN_ABORT: &str = "stop-abort";

/// 停机第 4–6 步要动的外部东西。真实现在 `daemon.rs`；测试用记录次序的假实现。
pub trait StopHooks: Send {
    /// 第 4 步。
    fn stop_tasks(&mut self) -> impl Future<Output = ()> + Send;
    /// 第 5 步。
    fn stop_ipc(&mut self) -> impl Future<Output = ()> + Send;
    /// 第 6 步：WAL 合回主库（`Busy` 就跳过）再关存储。
    fn close_store(&mut self) -> impl Future<Output = Result<(), StorageError>> + Send;
}

/// 停机一开始就起：一个普通线程（不在 tokio 里），`limit` 后按 `Stopping{reason}` 算退出状态强退。
/// 和走完全程写下 `Exited` 时算出来的是同一个值（判据只看 `reason`）。
pub fn arm_hard_deadline(reason: StopReason, managed: bool, limit: Duration) {
    let status = process_status(&Phase::Stopping { reason }, managed);
    let spawned = std::thread::Builder::new()
        .name("iwork-stop-deadline".into())
        .spawn(move || {
            std::thread::sleep(limit);
            tracing::error!(?reason, status, "停机超过硬上限，强制退出");
            // exit 不跑析构：先把日志排空，这条 error 才留得下来
            crate::logging::flush_global();
            eprintln!("iwork-daemon：停机超过 {limit:?}，强制退出（退出状态 {status}）");
            std::process::exit(i32::from(status));
        });
    if let Err(e) = spawned {
        tracing::error!(error = %e, "起停机硬上限线程失败");
    }
}

/// 第 1–7 步。返回写下的最后一个 `Phase`。
pub async fn stop<W: RunningWork>(
    reason: StopReason,
    store_failed: bool,
    board: &StatusBoard,
    gate: &Gate,
    work: &W,
    hooks: &mut impl StopHooks,
) -> Phase {
    tracing::info!(?reason, "开始停机");
    // 1：意图先落盘。之后哪一步卡住、被硬上限强退，`relaunchable` 的答案都不变
    board.set_phase(Phase::Stopping { reason });
    let left = gate.close_and_drain(StopStep::CloseGate.limit()).await;
    if left > 0 {
        tracing::warn!(
            in_flight = left,
            "关门后到点还有请求没回完，靠第 3 步封口兜住"
        );
    }
    // 2：只有用户明确退出才走取消流程
    if applies(StopStep::CancelAll, reason, store_failed)
        && tokio::time::timeout(StopStep::CancelAll.limit(), work.cancel_all())
            .await
            .is_err()
    {
        tracing::warn!("取消流程到点没走完，剩下的停在「取消中」，下次启动记成已取消");
    }
    // 3：封口、关会话、核对进程组
    dev::pause_at(PAUSE_IN_ABORT);
    match tokio::time::timeout(StopStep::AbortAll.limit(), work.abort_all()).await {
        Ok(report) => {
            if report.not_cleared > 0 {
                tracing::warn!(
                    sessions = report.sessions,
                    not_cleared = report.not_cleared,
                    "有会话的进程组到点没清空（已知限制 L6）"
                );
            } else {
                tracing::info!(sessions = report.sessions, "会话已全部关闭");
            }
            board.set_not_cleared(report.not_cleared);
        }
        Err(_) => {
            // 不靠宿主自己的期限先到：外层到点就读此刻的报告，没报回来的算没清空（b45 1/F-4）
            let report = work.abort_snapshot();
            tracing::warn!(
                sessions = report.sessions,
                not_cleared = report.not_cleared,
                "关会话到点没走完，按此刻的报告记"
            );
            board.set_not_cleared(report.not_cleared);
        }
    }
    // 4、5
    hooks.stop_tasks().await;
    hooks.stop_ipc().await;
    // 6
    if applies(StopStep::CloseStore, reason, store_failed) {
        match tokio::time::timeout(StopStep::CloseStore.limit(), hooks.close_store()).await {
            Ok(Ok(())) => {}
            Ok(Err(e)) => tracing::warn!(error = %e, "关存储出错"),
            Err(_) => tracing::warn!("关存储到点没完成"),
        }
    } else {
        tracing::warn!("存储已失效，跳过关存储");
    }
    // 7
    let phase = Phase::Exited {
        code: stop_code(reason),
        reason: Some(reason),
        message: None,
    };
    board.set_phase(phase.clone());
    phase
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::status::StatusIdentity;
    use crate::work::{AbortReport, QuitCounts, ShutdownDecision};
    use iwork_ipc::rendezvous::{ExeStamp, ExitCode};
    use std::sync::{Arc, Mutex};

    type Log = Arc<Mutex<Vec<&'static str>>>;

    struct Work {
        log: Log,
        not_cleared: u32,
    }

    impl RunningWork for Work {
        async fn quit_counts(&self) -> Option<QuitCounts> {
            Some(QuitCounts {
                running_turns: 1,
                ..QuitCounts::default()
            })
        }
        fn try_quit(
            &self,
            confirmed: bool,
            counts: Option<QuitCounts>,
            trigger: impl FnOnce(),
        ) -> ShutdownDecision {
            crate::work::NoWork.try_quit(confirmed, counts, trigger)
        }
        async fn cancel_all(&self) {
            self.log.lock().unwrap().push("cancel_all");
        }
        async fn abort_all(&self) -> AbortReport {
            self.log.lock().unwrap().push("abort_all");
            AbortReport {
                sessions: 1,
                not_cleared: self.not_cleared,
            }
        }
        fn abort_snapshot(&self) -> AbortReport {
            AbortReport {
                sessions: 1,
                not_cleared: self.not_cleared,
            }
        }
    }

    struct Hooks(Log);

    impl StopHooks for Hooks {
        async fn stop_tasks(&mut self) {
            self.0.lock().unwrap().push("stop_tasks");
        }
        async fn stop_ipc(&mut self) {
            self.0.lock().unwrap().push("stop_ipc");
        }
        async fn close_store(&mut self) -> Result<(), StorageError> {
            self.0.lock().unwrap().push("close_store");
            Ok(())
        }
    }

    fn board(dir: &std::path::Path) -> StatusBoard {
        StatusBoard::create(
            dir,
            StatusIdentity {
                version: "t".into(),
                protocol_major: 1,
                exe_stamp: ExeStamp {
                    len: 0,
                    modified_unix_ns: 0,
                },
                log_dir: dir.into(),
            },
            Phase::Ready,
        )
    }

    async fn run(
        reason: StopReason,
        store_failed: bool,
        not_cleared: u32,
    ) -> (Vec<&'static str>, Phase, StatusBoard, tempfile::TempDir) {
        let dir = tempfile::tempdir().unwrap();
        let b = board(dir.path());
        let log: Log = Default::default();
        let work = Work {
            log: log.clone(),
            not_cleared,
        };
        let gate = Gate::default();
        let phase = stop(
            reason,
            store_failed,
            &b,
            &gate,
            &work,
            &mut Hooks(log.clone()),
        )
        .await;
        assert!(gate.enter().is_none(), "停机后门是关的");
        let l = log.lock().unwrap().clone();
        (l, phase, b, dir)
    }

    #[tokio::test]
    async fn user_quit_cancels_then_aborts() {
        let (log, phase, _b, _d) = run(StopReason::UserQuit, false, 0).await;
        assert_eq!(
            log,
            [
                "cancel_all",
                "abort_all",
                "stop_tasks",
                "stop_ipc",
                "close_store"
            ]
        );
        assert_eq!(
            phase,
            Phase::Exited {
                code: ExitCode::Stopped,
                reason: Some(StopReason::UserQuit),
                message: None
            }
        );
    }

    #[tokio::test]
    async fn signal_aborts_without_cancel() {
        for reason in [StopReason::Signal, StopReason::Upgrade, StopReason::Fatal] {
            let (log, phase, _b, _d) = run(reason, false, 0).await;
            assert_eq!(
                log,
                ["abort_all", "stop_tasks", "stop_ipc", "close_store"],
                "{reason:?}"
            );
            assert!(matches!(phase, Phase::Exited { reason: Some(r), .. } if r == reason));
        }
    }

    #[tokio::test]
    async fn fatal_skips_store_close() {
        let (log, phase, _b, _d) = run(StopReason::Fatal, true, 0).await;
        assert_eq!(log, ["abort_all", "stop_tasks", "stop_ipc"]);
        assert!(matches!(
            phase,
            Phase::Exited {
                code: ExitCode::Fatal,
                ..
            }
        ));
    }

    /// S20：第 3 步报没清空 → 停机照常走完，状态文件记下个数。
    #[tokio::test]
    async fn uncleared_sessions_are_recorded_and_stop_proceeds() {
        let (log, _phase, b, _d) = run(StopReason::Signal, false, 2).await;
        assert_eq!(log.last(), Some(&"close_store"));
        assert_eq!(b.snapshot().not_cleared, 2);
    }

    /// 停机表的勾选（方案 2.8）。
    #[tokio::test]
    async fn first_stop_request_wins() {
        let s = StopSwitch::new();
        let w = tokio::spawn({
            let s = s.clone();
            async move { s.wait().await }
        });
        assert!(s.request(StopReason::Signal));
        assert!(!s.request(StopReason::Fatal));
        assert_eq!(w.await.unwrap(), StopReason::Signal);
        assert_eq!(s.requested(), Some(StopReason::Signal));
    }

    /// b45 1/F-4：第 3 步到点、`abort_all` 还没回来时，状态文件记的是此刻的报告（不是初值 0）。
    #[tokio::test(start_paused = true)]
    async fn abort_deadline_records_snapshot() {
        struct Stuck;
        impl RunningWork for Stuck {
            async fn quit_counts(&self) -> Option<QuitCounts> {
                None
            }
            fn try_quit(
                &self,
                confirmed: bool,
                counts: Option<QuitCounts>,
                trigger: impl FnOnce(),
            ) -> ShutdownDecision {
                crate::work::NoWork.try_quit(confirmed, counts, trigger)
            }
            async fn cancel_all(&self) {}
            async fn abort_all(&self) -> AbortReport {
                std::future::pending().await
            }
            fn abort_snapshot(&self) -> AbortReport {
                AbortReport {
                    sessions: 3,
                    not_cleared: 2,
                }
            }
        }
        let dir = tempfile::tempdir().unwrap();
        let b = board(dir.path());
        let log: Log = Default::default();
        stop(
            StopReason::Signal,
            false,
            &b,
            &Gate::default(),
            &Stuck,
            &mut Hooks(log),
        )
        .await;
        assert_eq!(b.snapshot().not_cleared, 2);
    }

    #[test]
    fn stop_table() {
        use StopReason::*;
        use StopStep::*;
        for r in [UserQuit, Signal, Upgrade, Fatal] {
            for step in [CloseGate, AbortAll, StopTasks, StopIpc] {
                assert!(applies(step, r, false) && applies(step, r, true));
            }
            assert_eq!(applies(CancelAll, r, false), r == UserQuit);
            assert!(applies(CloseStore, r, false));
            assert!(!applies(CloseStore, r, true));
        }
        let total: u64 = [
            CloseGate, CancelAll, AbortAll, StopTasks, StopIpc, CloseStore,
        ]
        .iter()
        .map(|s| s.limit().as_secs())
        .sum();
        assert!(
            total < HARD_LIMIT.as_secs(),
            "各步上限之和 {total}s 在硬上限之内"
        );
    }
}
