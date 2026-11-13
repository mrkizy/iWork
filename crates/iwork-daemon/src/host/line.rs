//! 会话线任务（T10a 方案 2.5「会话线任务」）：一个对话一个，宿主第一次要用这条线时起，拥有 `SessionLine`。
//!
//! 一趟从取到活算起，整趟是一个 future，和封口赛跑（有偏的 `select`，封口在前）：封口一到，这一趟不管停在哪个
//! 等待点都不再被推进、整个丢掉，再 `retire()` 清树（E3）。一趟结束后在宿主的锁里一下做完：线标成空着、
//! 结局写进这一项、趟数加一（D4），然后叫醒宿主。

use std::sync::Arc;
use std::sync::atomic::AtomicBool;

use iwork_app::message_blocks::message_blocks;
use iwork_core::turn::{
    ResumeReason, ResumeRequest, RunTag, SessionLine, TurnEntry, TurnPlan, TurnPorts, resume_note,
};
use iwork_domain::conversation::ConversationState;
use iwork_domain::id::{ConversationId, TurnId};
use iwork_domain::turn::Turn;
use iwork_ports::agent::ContentBlock;
use iwork_ports::read_model::{ReadModelPort, SnapshotInputs};
use tokio::sync::Notify;

use super::changes::ChangeReader;
use super::plan::{Closing, Entry, JobEnd, JobKind, LastRun};
use super::ports::{HostEngine, HostPorts};
use super::prepare::{LineKey, prepare};
use super::{
    Backoff, Bounded, HOST_IO_LIMIT, JobInFlight, LINE_BACKOFF_MAX, LINE_BACKOFF_START, Shared,
    bounded,
};
use crate::logging;

type Line<P> =
    SessionLine<<P as HostPorts>::Agent, <<P as HostPorts>::Engine as HostEngine>::Decider>;

/// 这条线绑着的那份会话：`SessionLine` 与建它时的快照键。
struct Bound<P: HostPorts> {
    line: Line<P>,
    key: LineKey,
}

impl Shared {
    /// 这条线此刻占着它的那一趟（任务取活）。
    fn job_of(&self, c: ConversationId) -> Option<JobInFlight> {
        self.lock().by_conversation.get(&c).and_then(|e| e.job)
    }

    /// 派发边界（D1）：宿主开着、这一趟还是占着这条线的那一趟 → 记成「已过边界」，并把要跑的那条线的取消器
    /// 放进这一项（换过线后旧的作废）。过了边界的这一趟从这一刻起接得住取消。
    fn begin_run<A, D>(&self, c: ConversationId, run: RunTag, line: &SessionLine<A, D>) -> bool
    where
        A: iwork_ports::agent::AgentPort,
        D: iwork_core::turn::PermissionDecider,
    {
        let mut lines = self.lock();
        if lines.closing != Closing::Open {
            return false;
        }
        let Some(entry) = lines.by_conversation.get_mut(&c) else {
            return false;
        };
        match entry.job.as_mut() {
            Some(job) if job.run == run => {
                job.crossed = true;
                // 取消器在锁里取、直接放进这一项，不经过锁外的任何人（不变量 14）
                entry.canceller = Some(line.canceller());
                true
            }
            _ => false,
        }
    }

    /// 一趟结束（D4）：锁里一下做完，再叫醒宿主。收线成功的摘掉这一项。
    fn finish(&self, c: ConversationId, end: JobEnd) {
        {
            let mut lines = self.lock();
            let Some(entry) = lines.by_conversation.get_mut(&c) else {
                return;
            };
            let job = entry.job.take();
            entry.runs += 1;
            let turn = job.and_then(|j| match j.kind {
                JobKind::Run { turn, .. } => Some(turn),
                JobKind::Close => None,
            });
            let sent = job.is_some_and(|j| j.cancel_sent);
            entry.backoff = match &end {
                JobEnd::Broke(_) | JobEnd::NotStarted(_) => {
                    let delay = entry
                        .backoff
                        .map_or(LINE_BACKOFF_START, |b| (b.delay * 2).min(LINE_BACKOFF_MAX));
                    Some(Backoff {
                        delay,
                        ready_at: tokio::time::Instant::now() + delay,
                    })
                }
                JobEnd::Ended(_) | JobEnd::Closed => None,
            };
            if matches!(end, JobEnd::Closed) {
                lines.by_conversation.remove(&c);
            } else {
                entry.last = Some(LastRun {
                    turn,
                    end,
                    cancel_sent: sent,
                });
            }
        }
        #[cfg(any(test, feature = "test-hooks"))]
        self.counters
            .finished
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        self.idle.notify_waiters();
        self.wake.notify_one();
    }
}

/// 会话线任务在点名册上的那一格。报回封口结果时摘掉；没报回就退出（正常收线、panic 的展开）时丢弃它摘掉。
/// 两种都在点名册的锁里做，报回与摘掉是同一次改动（b45 2/F-2）。
struct LiveGuard {
    shared: Arc<Shared>,
    reported: bool,
}

impl LiveGuard {
    /// 封口那一支清完树：一次加锁里记下结果、从「没报回」里摘掉自己。
    fn report(&mut self, had_session: bool, cleared: bool) {
        let mut t = self.shared.tally();
        if had_session {
            t.report.sessions += 1;
            if !cleared {
                t.report.not_cleared += 1;
            }
        }
        t.unreported -= 1;
        self.reported = true;
    }
}

impl Drop for LiveGuard {
    fn drop(&mut self) {
        if !self.reported {
            self.shared.tally().unreported -= 1;
        }
        self.shared.idle.notify_waiters();
    }
}

/// 会话线任务的主循环。正常结束只有两种：收线做完、封口。
pub(super) async fn run<P: HostPorts>(
    ports: Arc<P>,
    shared: Arc<Shared>,
    conversation: ConversationId,
    wake: Arc<Notify>,
) {
    let mut live = LiveGuard {
        shared: shared.clone(),
        reported: false,
    };
    let mut bound: Option<Bound<P>> = None;
    let mut sealed = shared.sealed.subscribe();
    let read_busy = Arc::new(AtomicBool::new(false));
    loop {
        // 等被叫醒；交活时宿主已经把这一项写好，这条线已经算占着
        let job = loop {
            let notified = wake.notified();
            if let Some(job) = shared.job_of(conversation) {
                break job;
            }
            tokio::select! {
                biased;
                _ = async { sealed.wait_for(|s| *s).await.map(|_| ()) } => {
                    retire(&shared, &mut bound, &mut live).await;
                    return;
                }
                _ = notified => {}
            }
        };
        let end = tokio::select! {
            biased;
            _ = async { sealed.wait_for(|s| *s).await.map(|_| ()) } => {
                // 封口：这一趟的 future 已被丢掉，再清树（次序不能反，T04 第 11 节硬约束一）
                retire(&shared, &mut bound, &mut live).await;
                return;
            }
            end = one_run::<P>(&ports, &shared, conversation, job, &mut bound, &read_busy) => end,
        };
        let closed = matches!(end, JobEnd::Closed);
        shared.finish(conversation, end);
        if closed {
            return;
        }
    }
}

/// 封口那一支：清树并核对，结果报给 `abort_all`。不提交任何东西。
async fn retire<P: HostPorts>(shared: &Shared, bound: &mut Option<Bound<P>>, live: &mut LiveGuard) {
    let Some(b) = bound.as_mut() else {
        live.report(false, true);
        return;
    };
    let had_session = b.line.session().is_some();
    let cleared = match b.line.retire().await {
        Ok(()) => true,
        Err(e) => {
            tracing::warn!(error = %e, "封口时会话线清树没确认清空");
            false
        }
    };
    live.report(had_session, cleared);
    #[cfg(any(test, feature = "test-hooks"))]
    shared.counters.at(|h| h.after_abort_report.clone()).await;
}

type PortsOf<'a, P> = TurnPorts<
    'a,
    <P as HostPorts>::Store,
    <P as HostPorts>::Clock,
    <P as HostPorts>::Ids,
    <P as HostPorts>::Live,
    <P as HostPorts>::Timer,
    <P as HostPorts>::Read,
>;

fn turn_ports<P: HostPorts>(p: &P) -> PortsOf<'_, P> {
    TurnPorts {
        store: p.store(),
        clock: p.clock(),
        ids: p.ids(),
        live: p.live(),
        timer: p.timer(),
        history: p.read(),
    }
}

/// 准备要读的：回合、这一轮的请求、算快照的已提交输入、这个对话最后一条会话绑定。一次在途（每条线 1 个）。
struct Facts {
    turn: Turn,
    request: Option<iwork_domain::message::Message>,
    inputs: SnapshotInputs,
}

async fn read_facts<P: HostPorts>(
    ports: &Arc<P>,
    busy: &Arc<AtomicBool>,
    conversation: ConversationId,
    turn: TurnId,
) -> Result<Facts, String> {
    let p = ports.clone();
    let read = async move {
        let r = p.read();
        let (turn, _) = r
            .turn_with_conversation(turn)
            .await
            .map_err(|e| e.to_string())?
            .ok_or_else(|| format!("回合 {turn} 不在"))?;
        let request = r.turn_request(turn.id).await.map_err(|e| e.to_string())?;
        let inputs = r
            .snapshot_inputs(conversation)
            .await
            .map_err(|e| e.to_string())?;
        Ok::<_, String>(Facts {
            turn,
            request,
            inputs,
        })
    };
    match bounded(busy, HOST_IO_LIMIT, read).await {
        Bounded::Done(r) => r,
        Bounded::Busy => Err("这条线上一次准备的读还没回来".into()),
        Bounded::TimedOut => Err("准备的读 30 秒没回来".into()),
    }
}

/// 一趟：跑回合，或收线。
async fn one_run<P: HostPorts>(
    ports: &Arc<P>,
    shared: &Arc<Shared>,
    conversation: ConversationId,
    job: JobInFlight,
    bound: &mut Option<Bound<P>>,
    read_busy: &Arc<AtomicBool>,
) -> JobEnd {
    match job.kind {
        JobKind::Close => close_line::<P>(ports, bound).await,
        JobKind::Run { turn, entry } => {
            let span = logging::span::turn(&turn.to_string());
            tracing::Instrument::instrument(
                run_turn_job::<P>(
                    ports,
                    shared,
                    conversation,
                    job.run,
                    turn,
                    entry,
                    bound,
                    read_busy,
                ),
                span,
            )
            .await
        }
    }
}

/// D8a：丢掉一条线之前把它身上没落盘、没清掉的了结；做不到就留着这条线。
///
/// 这里的补记提交（`SessionLine::settle`）**有意不限时**、不进 [`bounded`]（supervisor 2026-11-14 定）：它在 core 里
/// 借着 `&mut line` 做，每次都取新的 `command_id`；限时丢掉之后再补会换号重发，正好违反不变量 13 的另一半（上一次
/// 没回来不发下一次）。方案 D8a 给它定的是 `Busy` 退避、不设次数上限，没有 30 秒时限。卡住时这条线不前进，封口照样
/// 能把这一趟整个丢掉（E3）。
async fn settle_and_retire<P: HostPorts>(ports: &P, line: &mut Line<P>) -> Result<(), String> {
    line.settle(turn_ports(ports))
        .await
        .map_err(|e| format!("补绑定回退没成：{e}"))?;
    line.retire()
        .await
        .map_err(|e| format!("旧进程树没确认清空：{e}"))?;
    if line.is_settled() {
        Ok(())
    } else {
        Err("会话线还有没了结的东西".into())
    }
}

async fn close_line<P: HostPorts>(ports: &Arc<P>, bound: &mut Option<Bound<P>>) -> JobEnd {
    if let Some(b) = bound.as_mut()
        && let Err(why) = settle_and_retire(ports.as_ref(), &mut b.line).await
    {
        tracing::error!(%why, "删除对话时收线没成，线留着，下一轮再来");
        return JobEnd::NotStarted(why);
    }
    *bound = None;
    JobEnd::Closed
}

#[allow(clippy::too_many_arguments)]
async fn run_turn_job<P: HostPorts>(
    ports: &Arc<P>,
    shared: &Arc<Shared>,
    conversation: ConversationId,
    run: RunTag,
    turn_id: TurnId,
    entry: Entry,
    bound: &mut Option<Bound<P>>,
    read_busy: &Arc<AtomicBool>,
) -> JobEnd {
    let started = std::time::Instant::now();
    // ① 读已提交的输入、解析路径、重算快照（D8）
    #[cfg(any(test, feature = "test-hooks"))]
    {
        shared
            .counters
            .prepare_reads
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        shared.counters.at(|h| h.before_prepare_read.clone()).await;
    }
    let facts = match read_facts(ports, read_busy, conversation, turn_id).await {
        Ok(f) => f,
        Err(why) => return JobEnd::NotStarted(why),
    };
    if facts.inputs.conversation.state != ConversationState::Active {
        return JobEnd::NotStarted("对话在删".into());
    }
    let prepared = match prepare(ports.env(), ports.fs(), &facts.inputs, &facts.turn) {
        Ok(p) => p,
        Err(why) => return JobEnd::NotStarted(why),
    };
    // ② 和线上绑的那份比；不一样先把旧线收干净（D8a），再按这个对话最后一条绑定建新线（D3a）
    let reuse = bound.as_ref().is_some_and(|b| b.key == prepared.line_key);
    if !reuse {
        if let Some(b) = bound.as_mut()
            && let Err(why) = settle_and_retire(ports.as_ref(), &mut b.line).await
        {
            tracing::warn!(%why, "换线前旧线没了结，旧线留着，这个回合不开始");
            return JobEnd::NotStarted(why);
        }
        // 在 D8a 补完旧绑定之后再取（换线要用补完之后的那一条）；和准备的读共用这条线的在途标记，先后做
        let p = ports.clone();
        let read = bounded(read_busy, HOST_IO_LIMIT, async move {
            p.read().last_session_binding(conversation).await
        })
        .await;
        let resume = match read {
            Bounded::Done(Ok(r)) => r,
            Bounded::Done(Err(e)) => {
                return JobEnd::NotStarted(format!("取最后一条会话绑定失败：{e}"));
            }
            Bounded::Busy => return JobEnd::NotStarted("这条线上一次取绑定的读还没回来".into()),
            Bounded::TimedOut => {
                return JobEnd::NotStarted("取最后一条会话绑定 30 秒没回来".into());
            }
        };
        let decider = Arc::new(ports.engine().decider_for(prepared.keys.clone()));
        *bound = Some(Bound {
            line: SessionLine::new(decider, resume),
            key: prepared.line_key.clone(),
        });
    }
    let Some(b) = bound.as_mut() else {
        unreachable!("上面刚建好");
    };
    // ③ 拼回合计划：请求经同一个「用户消息 → 输入块」的函数转（T06 5.2a）
    let request = facts
        .request
        .as_ref()
        .ok_or_else(|| format!("回合 {turn_id} 没有触发它的用户消息"))
        .and_then(message_blocks);
    let (entry, blocks) = match entry {
        Entry::New => match request {
            Ok(blocks) => (TurnEntry::New, blocks),
            Err(why) => return JobEnd::NotStarted(why),
        },
        Entry::Resume => {
            let granted = ports.engine().granted_call(turn_id);
            let note = match &granted {
                Some(g) => resume_note(&g.reason()),
                None => resume_note(&ResumeReason::Interrupted),
            };
            let request = match request {
                Ok(blocks) => ResumeRequest::Ready(blocks),
                Err(why) => ResumeRequest::Unavailable(why),
            };
            (
                TurnEntry::Resume {
                    request,
                    recorded_usage: Default::default(),
                },
                vec![ContentBlock::Text(note)],
            )
        }
    };
    let prepare_ms = started.elapsed().as_millis() as u64;
    let capture_started = std::time::Instant::now();
    let changes = ports.changes().capture(&prepared.spec.cwd).await;
    // 第 6 节：回合开始前的读与路径解析、拍改动的开始那一份，两个耗时记进 `turn` span 下的这一条日志
    tracing::info!(
        prepare_ms,
        capture_ms = capture_started.elapsed().as_millis() as u64,
        change_probe = changes.is_some(),
        "回合开始前的准备"
    );
    let plan = TurnPlan {
        turn: facts.turn,
        entry,
        assistant_name: prepared.assistant_name,
        context_project_id: prepared.context_project_id,
        spec: prepared.spec,
        blocks,
        changes,
        run,
    };
    // ④ 派发边界
    if !shared.begin_run(conversation, run, &b.line) {
        return JobEnd::NotStarted("宿主已关，过不了派发边界".into());
    }
    // ⑤ 跑
    match b
        .line
        .run_turn(ports.agent(), turn_ports(ports.as_ref()), plan)
        .await
    {
        Ok(state) => JobEnd::Ended(state),
        Err(e) => {
            tracing::error!(error = %e, turn = %turn_id, "编排以错误收场，宿主补账");
            JobEnd::Broke(e.to_string())
        }
    }
}
