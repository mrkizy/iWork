//! 回合编排的多路输入（优先次序见 `runner.rs` 的模块文档）。

use std::future::Future;
use std::pin::Pin;
use std::task::{Context, Poll};

use futures_channel::mpsc;
use futures_core::Stream;
use iwork_ports::agent::AgentUpdate;

use super::permission::{Ticket, TurnSlot, Work};
use super::runner::FAIR_BURST;

pub(crate) enum Input {
    Update(Option<AgentUpdate>),
    Cancel,
    FlushDue,
    /// 该看一眼等待上限了
    LimitCheck,
    /// 受理交接表里这张票
    Admit(Ticket),
    /// 采纳交接表里这张票的结论
    Adopt(Ticket),
}

type Sleep<'t> = Pin<Box<dyn Future<Output = ()> + Send + 't>>;

/// 多路输入的调度状态。
#[derive(Default)]
pub(crate) struct Scheduler<'t> {
    pub flush_timer: Option<Sleep<'t>>,
    /// 有「等判定方」的票、会话声明了等待上限时才有：到时去查一次期限
    pub limit_timer: Option<Sleep<'t>>,
    /// 连续处理的更新条数
    burst: u32,
    /// 这一轮连续更新之后是否已经让出过
    yielded: bool,
    /// 本回合已处理的更新条数，与审批的前序边界比较
    consumed: u64,
}

impl Scheduler<'_> {
    pub fn poll<F: Future<Output = Option<AgentUpdate>>>(
        &mut self,
        mut next: Pin<&mut F>,
        slot: &TurnSlot,
        cancels: &mut mpsc::UnboundedReceiver<()>,
        cx: &mut Context<'_>,
    ) -> Poll<Input> {
        // 取消的发送端在交接表里，回合结束前不会关闭；关闭了也只是不再有输入
        if let Poll::Ready(Some(())) = Pin::new(cancels).poll_next(cx) {
            return Poll::Ready(Input::Cancel);
        }
        if let Some(timer) = self.flush_timer.as_mut()
            && timer.as_mut().poll(cx).is_ready()
        {
            self.flush_timer = None;
            return Poll::Ready(Input::FlushDue);
        }
        if let Some(timer) = self.limit_timer.as_mut()
            && timer.as_mut().poll(cx).is_ready()
        {
            self.limit_timer = None;
            return Poll::Ready(Input::LimitCheck);
        }
        let work_first = self.burst >= FAIR_BURST;
        if !work_first && let Poll::Ready(update) = next.as_mut().poll(cx) {
            return Poll::Ready(self.update(update));
        }
        if work_first && !self.yielded {
            // 更新一直就绪时主动让出一次，别的任务（含运行时的其他会话）才有机会跑
            self.yielded = true;
            cx.waker().wake_by_ref();
            return Poll::Pending;
        }
        self.yielded = false;
        self.burst = 0;
        match slot.poll_work(self.consumed, cx) {
            Work::Adopt(ticket) => Poll::Ready(Input::Adopt(ticket)),
            Work::Admit(ticket) => Poll::Ready(Input::Admit(ticket)),
            // 排在最前的请求还没到前序边界：只处理更新，别的请求排在它后面
            Work::UpdatesFirst => match next.as_mut().poll(cx) {
                Poll::Ready(update) => Poll::Ready(self.update(update)),
                Poll::Pending => Poll::Pending,
            },
            Work::Idle => {
                if work_first && let Poll::Ready(update) = next.as_mut().poll(cx) {
                    return Poll::Ready(self.update(update));
                }
                Poll::Pending
            }
        }
    }

    fn update(&mut self, update: Option<AgentUpdate>) -> Input {
        self.burst += 1;
        if update.is_some() {
            self.consumed += 1;
        }
        Input::Update(update)
    }
}
