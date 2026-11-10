//! 一条会话上「当前回合」的账：更新往哪个队列送、送了多少条、谁来收尾。
//!
//! 收尾方只有一个，由同一把锁决定：
//! - prompt 先有结果、此时没在取消 → [`TurnBook::complete`] 当场推终态、结束回合；之后的取消请求
//!   看不到回合，什么都不做（不会去结束进程树）；
//! - 取消先登记 → prompt 的结果交给取消方，[`TurnBook::complete`] 只通知「prompt 已结束」；取消方
//!   清完进程树后用 [`TurnBook::finish_cancel`] 推终态，同时把会话标为不可再用。在那之前回合一直
//!   占着，下一回合开不了。

use std::sync::{Mutex, MutexGuard};

use agent_client_protocol::v1::schema::SessionUpdate;
use iwork_ports::agent::{AgentError, AgentUpdate};
use tokio::sync::{mpsc, watch};

use crate::convert::UpdateMapper;

#[derive(Debug, Default)]
pub(crate) struct TurnBook(Mutex<Inner>);

#[derive(Debug, Default)]
struct Inner {
    active: Option<Active>,
    next_id: u64,
    /// 会话已不能再用（取消时结束了进程树），新回合直接报进程已退出
    retired: Option<String>,
}

#[derive(Debug)]
struct Active {
    id: u64,
    tx: mpsc::UnboundedSender<AgentUpdate>,
    mapper: UpdateMapper,
    /// 本回合已交出的更新条数（不含终态）
    emitted: u64,
    /// 取消已登记时有值：prompt 结束后在这里通知取消方
    cancelling: Option<watch::Sender<bool>>,
}

/// [`TurnBook::complete`] 的结果。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Completion {
    /// 终态已推出，回合结束
    Delivered,
    /// 取消在先：终态由取消方推
    LeftToCancel,
    /// 回合已不是这一个（不该发生，记下即可）
    Stale,
}

impl TurnBook {
    fn lock(&self) -> MutexGuard<'_, Inner> {
        self.0.lock().unwrap_or_else(|p| p.into_inner())
    }

    /// 开一个新回合。上一回合没收尾、或会话已不能再用时报错。
    pub fn begin(&self, tx: mpsc::UnboundedSender<AgentUpdate>) -> Result<u64, AgentError> {
        let mut inner = self.lock();
        if let Some(reason) = &inner.retired {
            return Err(AgentError::ProcessExited {
                message: reason.clone(),
            });
        }
        if inner.active.is_some() {
            return Err(AgentError::Protocol {
                message: "上一回合还没结束，不能再发 prompt".into(),
            });
        }
        inner.next_id += 1;
        let id = inner.next_id;
        inner.active = Some(Active {
            id,
            tx,
            mapper: UpdateMapper::default(),
            emitted: 0,
            cancelling: None,
        });
        Ok(id)
    }

    /// 运行时发来一条更新：标准化后送进当前回合的队列。回合之外的更新只留在诊断记录里。
    pub fn on_update(&self, update: SessionUpdate) {
        let mut inner = self.lock();
        let Some(active) = inner.active.as_mut() else {
            return;
        };
        if let Some(update) = active.mapper.map(update)
            && active.tx.send(update).is_ok()
        {
            active.emitted += 1;
        }
    }

    /// 当前回合已交出的更新条数；没有回合时为 0。
    pub fn emitted(&self) -> u64 {
        self.lock().active.as_ref().map_or(0, |a| a.emitted)
    }

    /// prompt 有了结果（响应、失败或进程退出）。
    pub fn complete(&self, id: u64, last: AgentUpdate) -> Completion {
        let mut inner = self.lock();
        match inner.active.as_ref() {
            Some(a) if a.id == id => {}
            _ => return Completion::Stale,
        }
        let active = inner.active.as_ref().expect("刚检查过");
        if let Some(done) = &active.cancelling {
            done.send_replace(true);
            return Completion::LeftToCancel;
        }
        let active = inner.active.take().expect("刚检查过");
        let _ = active.tx.send(last);
        Completion::Delivered
    }

    /// 登记取消。返回回合 id 与「prompt 已结束」的通知；没有进行中的回合、或已在取消时返回 `None`，
    /// 调用方什么都不做。
    pub fn request_cancel(&self) -> Option<(u64, watch::Receiver<bool>)> {
        let mut inner = self.lock();
        let active = inner.active.as_mut()?;
        if active.cancelling.is_some() {
            return None;
        }
        let (tx, rx) = watch::channel(false);
        active.cancelling = Some(tx);
        Some((active.id, rx))
    }

    /// 取消方收尾：推终态、结束回合，并把会话标为不能再用（进程树已被结束）。
    pub fn finish_cancel(&self, id: u64, last: AgentUpdate, retired: String) {
        let mut inner = self.lock();
        inner.retired = Some(retired);
        if inner.active.as_ref().is_some_and(|a| a.id == id) {
            let active = inner.active.take().expect("刚检查过");
            let _ = active.tx.send(last);
        }
    }
}

#[cfg(test)]
mod tests {
    use iwork_ports::agent::TurnOutcome;

    use super::*;

    fn done() -> AgentUpdate {
        AgentUpdate::Finished(TurnOutcome::Completed)
    }

    fn cancelled() -> AgentUpdate {
        AgentUpdate::Finished(TurnOutcome::Cancelled)
    }

    /// 取消插在「prompt 有结果」与「收尾」之间：收尾让给取消方，旧回合不报 Completed；
    /// 取消方清完之前下一回合开不了，之后会话不能再用。
    #[test]
    fn cancel_between_result_and_completion_owns_the_ending() {
        let book = TurnBook::default();
        let (tx, mut rx) = mpsc::unbounded_channel();
        let id = book.begin(tx).unwrap();
        // prompt 的响应已到、还没收尾时，取消登记
        let (cancel_id, prompt_done) = book.request_cancel().expect("回合在进行");
        assert_eq!(cancel_id, id);
        assert_eq!(book.complete(id, done()), Completion::LeftToCancel);
        assert!(*prompt_done.borrow(), "取消方被告知 prompt 已结束");
        assert!(rx.try_recv().is_err(), "旧回合没有报 Completed");
        // 取消方还没清完：下一回合开不了
        let (tx2, _rx2) = mpsc::unbounded_channel();
        assert!(matches!(book.begin(tx2), Err(AgentError::Protocol { .. })));
        book.finish_cancel(id, cancelled(), "已取消".into());
        assert_eq!(rx.try_recv().unwrap(), cancelled());
        let (tx3, _rx3) = mpsc::unbounded_channel();
        assert!(
            matches!(book.begin(tx3), Err(AgentError::ProcessExited { .. })),
            "进程树已结束，会话不能再用"
        );
    }

    /// prompt 先收尾：之后的取消看不到回合，不会去结束进程树；下一回合照常开。
    #[test]
    fn completion_before_cancel_leaves_nothing_to_cancel() {
        let book = TurnBook::default();
        let (tx, mut rx) = mpsc::unbounded_channel();
        let id = book.begin(tx).unwrap();
        assert_eq!(book.complete(id, done()), Completion::Delivered);
        assert_eq!(rx.try_recv().unwrap(), done());
        assert!(
            book.request_cancel().is_none(),
            "没有回合可取消，也就不结束进程树"
        );
        let (tx2, _rx2) = mpsc::unbounded_channel();
        assert_eq!(book.begin(tx2).unwrap(), id + 1);
    }

    #[test]
    fn second_cancel_is_ignored_and_stale_completion_is_harmless() {
        let book = TurnBook::default();
        let (tx, _rx) = mpsc::unbounded_channel();
        let id = book.begin(tx).unwrap();
        assert!(book.request_cancel().is_some());
        assert!(book.request_cancel().is_none());
        assert_eq!(book.complete(id + 7, done()), Completion::Stale);
    }

    #[test]
    fn emitted_counts_only_forwarded_updates() {
        use agent_client_protocol::v1::schema::{
            ContentBlock, ContentChunk, CurrentModeUpdate, TextContent,
        };
        let book = TurnBook::default();
        let (tx, _rx) = mpsc::unbounded_channel();
        book.begin(tx).unwrap();
        let text = || {
            SessionUpdate::AgentMessageChunk(ContentChunk::new(ContentBlock::Text(
                TextContent::new("x"),
            )))
        };
        book.on_update(text());
        book.on_update(SessionUpdate::CurrentModeUpdate(CurrentModeUpdate::new(
            "m",
        )));
        book.on_update(text());
        assert_eq!(book.emitted(), 2, "不转成更新的通知不计数");
    }
}
