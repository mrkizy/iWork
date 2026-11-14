//! 对账的一轮里，一个对话该做哪一件事（T10a 方案 2.5 的对账表）。纯函数：输入已提交的事实（这一轮读到的
//! `ConversationWork`）、这条会话线在内存里的样子、宿主的关闭状态，输出至多一件事。
//!
//! 一个对话里只看最老的那个没到终态的回合（0a.1 第 4 条）：它排着就派发，它中断了就等用户或等续接，它后面的都不动。
//! 归属按**正在处理的那个回合**判（`open_turns[i].owner`），不拿第一个回合的套整个对话。

use std::collections::BTreeMap;

use iwork_core::turn::RunTag;
use iwork_domain::approval::owner_alive;
use iwork_domain::conversation::ConversationState;
use iwork_domain::id::{Seq, TurnId};
use iwork_domain::turn::TurnState;
use iwork_ports::read_model::{ConversationWork, OpenTurn};

/// 宿主的关闭状态（内存）。只会往后走：开着 → 完全退出 → 封口。
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Closing {
    Open,
    /// 完全退出（E2）：在跑的走取消，排着的取消，不再起新活
    Quit,
    /// 封口（E3）：不再开会话、不写回合状态
    Sealed,
}

/// 交给会话线任务的一份活。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum JobKind {
    Run {
        turn: TurnId,
        entry: Entry,
    },
    /// 删除对话时把会话线收干净（X3）
    Close,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Entry {
    New,
    Resume,
}

/// 一趟的结局（D4）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum JobEnd {
    /// 准备没做成、过不了派发边界：什么都没提交
    NotStarted(String),
    /// `run_turn` 正常返回，回合停在这个状态
    Ended(TurnState),
    /// `run_turn` 以错误收场：库里的回合可能还占着进程（D5）
    Broke(String),
    /// 收线的活做完了（X3）：这条线收干净，宿主摘掉这一项
    Closed,
}

/// 这条线上一趟的结局。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LastRun {
    /// 交的是哪个回合的活（收线的活是 `None`）
    pub turn: Option<TurnId>,
    pub end: JobEnd,
    /// 这一趟发过取消信号没有
    pub cancel_sent: bool,
}

/// 这条会话线此刻的样子（宿主锁里的那一项的只读视图）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LineView {
    /// 占着这条线的那一趟
    pub job: Option<JobView>,
    /// 上一趟
    pub last: Option<LastRun>,
    /// 退避到时了没有（上一趟 `Broke` / `NotStarted` 之后才有退避）
    pub backoff_ready: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct JobView {
    pub kind: JobKind,
    pub run: RunTag,
    /// 过了派发边界没有
    pub crossed: bool,
    pub cancel_sent: bool,
}

/// 对账一轮的公共输入。
#[derive(Debug, Clone, Copy)]
pub struct Round<'a> {
    pub closing: Closing,
    /// 宿主这次启动时读到的 head（R7：继续意图要比它大才算数）
    pub boot_head: Seq,
    /// 引擎追平之后的 `turns_to_resume()`：回合 → 批准它的那条事件的 seq；没追平这一轮当它是空的（D7 ①）
    pub turns_to_resume: &'a BTreeMap<TurnId, Seq>,
}

/// 这一轮对一个对话做的事。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Action {
    /// 交给会话线任务（D1、D7、X3）。交活即占用（D3）
    Hand(JobKind),
    /// 给过了边界的那一趟发取消（D6）
    Cancel { turn: TurnId, run: RunTag },
    /// 编排以错误收场，回合还占着进程：补账（D5）。`reason` 是编排报的错；`cancel_requested` 是
    /// [`cancel_counts`] 的结论，决定按取消收还是按失败收
    Settle {
        turn: TurnId,
        reason: String,
        cancel_requested: bool,
    },
    /// 排着的或已中断的回合直接收成已取消（X2、A3 的另一头、E2 ③）
    Withdraw { turn: TurnId, from: TurnState },
    /// 删除的最后一笔（X4）
    FinishDelete,
    /// 不可能的状态：库里占着进程的回合，本进程里却从没有过它的编排（R3 在启动时查过）
    Impossible(String),
}

fn deleting(w: &ConversationWork) -> bool {
    w.conversation.state == ConversationState::Deleting
}

/// 取消意图比这一趟的起点大（A5：意图只管它落盘时的那一段）。
fn cancel_wanted(t: &OpenTurn) -> bool {
    match (t.cancel_intent_seq, t.run_started_seq) {
        (Some(c), Some(r)) => c > r,
        // 没有起点 = 这一趟还没提交第一笔：意图落不到这种回合上（A4 ②），有也是旧的
        _ => false,
    }
}

/// 一趟编排的取消算不算数。发取消（D6）和补账（D5，按取消收还是按失败收）只看这一个判断（b45 1/F-1）。
/// 任一成立就算：本趟已发过取消信号；对话在删；宿主完全退出；回合还占着进程、有比这一趟起点新的取消意图。
/// `turn` 是这一趟的回合（已经到终态、不在没到终态的列表里时为 `None`）。
pub fn cancel_counts(
    w: &ConversationWork,
    turn: Option<&OpenTurn>,
    closing: Closing,
    sent: bool,
) -> bool {
    sent || deleting(w)
        || closing == Closing::Quit
        || turn.is_some_and(|t| t.turn.state.holds_process() && cancel_wanted(t))
}

/// 有续接的依据（D7）：这次运行里落盘的批准还没交付（R6：批准的 seq 比启动时的 head 大，上一次运行里的批准不让回合
/// 自己续接，产品负责人 2026-11-13 定），或这次运行里用户点过「继续」（R7）。两样都按启动时的 head 截。
fn resume_wanted(t: &OpenTurn, r: &Round<'_>) -> bool {
    if r.turns_to_resume
        .get(&t.turn.id)
        .is_some_and(|granted| *granted > r.boot_head)
    {
        return true;
    }
    match (t.resume_intent_seq, t.interrupted_seq) {
        (Some(i), Some(at)) => i > at && i > r.boot_head,
        _ => false,
    }
}

/// 上一趟交的是这个回合、并且没成（退避期间不再交）。
fn waiting_backoff(t: &OpenTurn, line: Option<&LineView>) -> bool {
    line.is_some_and(|l| {
        !l.backoff_ready
            && matches!(&l.last, Some(LastRun { turn: Some(turn), end: JobEnd::Broke(_) | JobEnd::NotStarted(_), .. }) if *turn == t.turn.id)
    })
}

/// 一个对话这一轮做什么。`line` 是这个对话的会话线（没有过就是 `None`）。
pub fn decide(w: &ConversationWork, line: Option<&LineView>, r: &Round<'_>) -> Option<Action> {
    if r.closing == Closing::Sealed {
        return None;
    }
    let job = line.and_then(|l| l.job);
    let Some(t) = w.open_turns.first() else {
        // 回合都到终态了：在删的对话收线、再删记录
        if !deleting(w) {
            return None;
        }
        return match (line, job) {
            (_, Some(_)) => None,
            (Some(_), None) => Some(Action::Hand(JobKind::Close)),
            (None, None) if w.cards_open => None,
            (None, None) => Some(Action::FinishDelete),
        };
    };
    if let Some(j) = job {
        // 线占着：只可能是给过了边界的这一趟发取消（D6），别的都等它结束
        let JobKind::Run { turn, .. } = j.kind else {
            return None;
        };
        let this_turn = w.open_turns.iter().find(|o| o.turn.id == turn);
        return (j.crossed && !j.cancel_sent && cancel_counts(w, this_turn, r.closing, false))
            .then_some(Action::Cancel { turn, run: j.run });
    }
    let owner_gone = owner_alive(&t.owner).is_err();
    match t.turn.state {
        s if s.holds_process() => match line.and_then(|l| l.last.as_ref()) {
            Some(LastRun {
                turn: Some(turn),
                end: JobEnd::Broke(reason),
                cancel_sent,
            }) if *turn == t.turn.id => Some(Action::Settle {
                turn: t.turn.id,
                reason: reason.clone(),
                cancel_requested: cancel_counts(w, Some(t), r.closing, *cancel_sent),
            }),
            _ => Some(Action::Impossible(format!(
                "回合 {} 是 {s:?}，本进程里没有一趟以出错结束的编排管它",
                t.turn.id
            ))),
        },
        TurnState::Queued => {
            if deleting(w) || owner_gone || r.closing == Closing::Quit {
                return Some(Action::Withdraw {
                    turn: t.turn.id,
                    from: TurnState::Queued,
                });
            }
            (!waiting_backoff(t, line)).then_some(Action::Hand(JobKind::Run {
                turn: t.turn.id,
                entry: Entry::New,
            }))
        }
        s @ TurnState::Interrupted { .. } => {
            if deleting(w) || owner_gone {
                return Some(Action::Withdraw {
                    turn: t.turn.id,
                    from: s,
                });
            }
            (r.closing == Closing::Open && resume_wanted(t, r) && !waiting_backoff(t, line))
                .then_some(Action::Hand(JobKind::Run {
                    turn: t.turn.id,
                    entry: Entry::Resume,
                }))
        }
        // 终态不会出现在 open_turns 里
        other => Some(Action::Impossible(format!(
            "回合 {} 是终态 {other:?}，却在没到终态的列表里",
            t.turn.id
        ))),
    }
}

/// `try_quit` 数的那种（E1 的 L）：过了派发边界、还没空下来。
pub fn crossed(line: &LineView) -> bool {
    line.job.is_some_and(|j| j.crossed)
}

#[cfg(test)]
#[path = "plan_tests.rs"]
mod tests;
