//! 退出与停机（T10a 方案 E1–E4）：回合宿主是停机流程看得到的「在跑的东西」。
//!
//! - `try_quit`（E1）：在宿主的锁里数出 L（过了派发边界、还没空下来的线），和 N 一起判；要退就置「完全退出」并触发
//!   停机。锁里没有等待点，和 `begin_run` 抢同一把锁（G3）：回合先过了边界，这里数得到它；这里先关了，它再也过不了。
//! - `cancel_all`（E2）：① 锁里给每一趟过了边界的编排发取消（不读库，存储卡住也发得出）；② 之后 `begin_run` 一律被拒；
//!   ③ 读一次库，把每个对话里还排着的回合提交成已取消，一个对话一笔。然后等各条线空下来。
//! - `abort_all`（E3、E4）：锁里置「封口」，再封存储的闸；每条线丢掉手上的一整趟、清树、报告。不写回合状态。

use std::sync::atomic::Ordering;
use std::time::Duration;

use iwork_app::lifecycle::{QuitInputs, decide_shutdown};
use iwork_app::turn_command::withdraw_queued;
use iwork_domain::common::Actor;
use iwork_domain::id::CommandId;
use iwork_domain::turn::TurnState;
use iwork_ports::clock::IdGenerator;
use iwork_ports::read_model::ReadModelPort;
use iwork_ports::store::{CommitRequest, StoragePort};

use super::plan::{Closing, JobKind};
use super::ports::HostPorts;
use super::{Bounded, HOST_ACTOR, HOST_IO_LIMIT, TurnHost, bounded};
use crate::work::{AbortReport, QUIT_COUNT_LIMIT, QuitCounts, RunningWork, ShutdownDecision};

/// 封口后等各条线清完树、报回来，最多这么久（停机第 3 步的上限里）。
pub const ABORT_LIMIT: Duration = Duration::from_secs(3);

impl<P: HostPorts> TurnHost<P> {
    /// 锁里：置「完全退出」（已封口的不往回改），给每一趟过了边界、还没发过取消的编排发取消（D6 ③）。
    /// 发取消在锁里同步调用（不变量 14）。
    fn quit_and_cancel(&self) {
        let mut lines = self.shared.lock();
        if lines.closing == Closing::Open {
            lines.closing = Closing::Quit;
        }
        for entry in lines.by_conversation.values_mut() {
            if let Some(job) = entry.job.as_mut()
                && job.crossed
                && !job.cancel_sent
                && let JobKind::Run { turn, .. } = job.kind
                && let Some(canceller) = entry.canceller.as_ref()
            {
                let tag = job.run;
                canceller.cancel(turn, tag);
                job.cancel_sent = true;
                #[cfg(any(test, feature = "test-hooks"))]
                self.shared.counters.record_cancel(turn, tag);
            }
        }
    }

    /// E2 ③：读一次库，每个对话里还排着、没交给过了边界的一趟的回合提交成已取消，一个对话一笔。
    /// 读或提交没做成：那些回合留在排着，下次启动会被派发（KA1，照实）。
    async fn withdraw_all_queued(&self) {
        let ports = self.ports.clone();
        let read = bounded(&self.shared.withdraw_read_busy, HOST_IO_LIMIT, async move {
            ports.read().turn_work().await
        })
        .await;
        let work = match read {
            Bounded::Done(Ok(w)) => w,
            Bounded::Done(Err(e)) => {
                tracing::warn!(error = %e, "完全退出时读排着的回合失败，它们下次启动会被派发（KA1）");
                return;
            }
            Bounded::Busy | Bounded::TimedOut => {
                tracing::warn!("完全退出时读排着的回合没回来，它们下次启动会被派发（KA1）");
                return;
            }
        };
        for w in work.conversations {
            let crossed_turn = {
                let lines = self.shared.lock();
                lines
                    .by_conversation
                    .get(&w.conversation.id)
                    .and_then(|e| e.job)
                    .filter(|j| j.crossed)
                    .and_then(|j| match j.kind {
                        JobKind::Run { turn, .. } => Some(turn),
                        JobKind::Close => None,
                    })
            };
            let events: Vec<_> = w
                .open_turns
                .iter()
                .filter(|t| t.turn.state == TurnState::Queued && Some(t.turn.id) != crossed_turn)
                .flat_map(|t| withdraw_queued(&t.turn))
                .collect();
            if events.is_empty() {
                continue;
            }
            let req = CommitRequest {
                command_id: CommandId::from_raw(self.ports.ids().next()),
                actor: Actor(HOST_ACTOR.into()),
                events,
                outbox: Vec::new(),
            };
            let ports = self.ports.clone();
            let committed = bounded(
                &self.shared.withdraw_commit_busy,
                HOST_IO_LIMIT,
                async move { ports.store().commit(req).await },
            )
            .await;
            match committed {
                Bounded::Done(Ok(_)) => {}
                Bounded::Done(Err(e)) => {
                    tracing::warn!(error = %e, conversation = %w.conversation.id, "完全退出时取消排着的回合没记下（KA1）")
                }
                Bounded::Busy | Bounded::TimedOut => {
                    tracing::warn!(conversation = %w.conversation.id, "完全退出时取消排着的回合没回来（KA1）");
                    // 上一笔还没回来就不再发后面的
                    return;
                }
            }
        }
    }

    /// 还占着的线（有一趟没结束）。
    fn busy_lines(&self) -> usize {
        self.shared
            .lock()
            .by_conversation
            .values()
            .filter(|e| e.job.is_some())
            .count()
    }
}

impl<P: HostPorts> RunningWork for TurnHost<P> {
    async fn quit_counts(&self) -> Option<QuitCounts> {
        let ports = self.ports.clone();
        let read = bounded(&self.shared.quit_read_busy, QUIT_COUNT_LIMIT, async move {
            ports.read().quit_counts().await
        })
        .await;
        match read {
            Bounded::Done(Ok(c)) => Some(QuitCounts {
                running_turns: c.running_turns,
                queued_turns: c.queued_turns,
                // 卡片那一半由 T10e 接上
                executing_cards: 0,
            }),
            Bounded::Done(Err(e)) => {
                tracing::warn!(error = %e, "退出前读数目失败，按「不知道」问");
                None
            }
            Bounded::Busy | Bounded::TimedOut => {
                tracing::warn!("退出前读数目 2 秒没回来（或上一次还没回来），按「不知道」问");
                None
            }
        }
    }

    fn try_quit(
        &self,
        confirmed: bool,
        counts: Option<QuitCounts>,
        trigger: impl FnOnce(),
    ) -> ShutdownDecision {
        let mut lines = self.shared.lock();
        let live = lines
            .by_conversation
            .values()
            .filter(|e| e.job.is_some_and(|j| j.crossed))
            .count();
        let decision = decide_shutdown(QuitInputs {
            confirmed,
            counts,
            live: u32::try_from(live).unwrap_or(u32::MAX),
        });
        if decision == ShutdownDecision::Proceed {
            if lines.closing == Closing::Open {
                lines.closing = Closing::Quit;
            }
            trigger();
        }
        decision
    }

    async fn cancel_all(&self) {
        // ① ②
        self.quit_and_cancel();
        self.wake();
        // ③
        self.withdraw_all_queued().await;
        // 等各条线空下来（上限由停机第 2 步管）
        loop {
            let idle = self.shared.idle.notified();
            if self.busy_lines() == 0 {
                return;
            }
            idle.await;
        }
    }

    async fn abort_all(&self) -> AbortReport {
        {
            // 先在宿主的锁里置封口，再封存储的闸（固定的先后；宿主的锁里不等闸以外的东西）
            let mut lines = self.shared.lock();
            lines.closing = Closing::Sealed;
        }
        self.ports.seal_store();
        self.shared.sealed.send_replace(true);
        let all_out = async {
            loop {
                let out = self.shared.idle.notified();
                if self.shared.live_lines.load(Ordering::SeqCst) == 0 {
                    return;
                }
                out.await;
            }
        };
        if tokio::time::timeout(ABORT_LIMIT, all_out).await.is_err() {
            tracing::warn!(
                stuck = self.shared.live_lines.load(Ordering::SeqCst),
                "封口后到点还有会话线没清完树"
            );
        }
        self.abort_snapshot()
    }

    /// 已报回来的照实记；还活着的会话线（还没清完树、没报回来）都算没清空。
    fn abort_snapshot(&self) -> AbortReport {
        let mut report = *self.shared.abort.lock().unwrap_or_else(|e| e.into_inner());
        let stuck = self.shared.live_lines.load(Ordering::SeqCst);
        report.not_cleared += u32::try_from(stuck).unwrap_or(u32::MAX);
        report
    }
}

/// 一条线的任务在封口时报回来的（`line::run` 里用）。
pub(super) fn record_abort(shared: &super::Shared, had_session: bool, cleared: bool) {
    let mut r = shared.abort.lock().unwrap_or_else(|e| e.into_inner());
    if had_session {
        r.sessions += 1;
        if !cleared {
            r.not_cleared += 1;
        }
    }
}
