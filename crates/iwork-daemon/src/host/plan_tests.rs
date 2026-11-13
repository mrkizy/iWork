//! 对账表（方案 2.5）逐行一格；期望值按表手写。

use super::*;
use crate::fixtures::*;
use iwork_domain::approval::{ConversationPresence, OwnerFacts, Presence};
use iwork_domain::turn::InterruptReason;

const CONV: u128 = 10;

fn alive() -> OwnerFacts {
    OwnerFacts {
        turn: Presence::Present,
        conversation: ConversationPresence::Present,
        assistant: Presence::Present,
    }
}

fn open(n: u128, state: TurnState) -> OpenTurn {
    let mut t = turn(n, CONV);
    t.state = state;
    OpenTurn {
        turn: t,
        owner: alive(),
        run_started_seq: None,
        interrupted_seq: None,
        cancel_intent_seq: None,
        resume_intent_seq: None,
    }
}

fn work(state: ConversationState, turns: Vec<OpenTurn>) -> ConversationWork {
    let mut c = project_conversation(CONV, 1);
    c.state = state;
    ConversationWork {
        conversation: c,
        open_turns: turns,
        cards_open: false,
    }
}

fn active(turns: Vec<OpenTurn>) -> ConversationWork {
    work(ConversationState::Active, turns)
}

fn round(resume: &BTreeMap<TurnId, Seq>) -> Round<'_> {
    Round {
        closing: Closing::Open,
        boot_head: Seq(100),
        turns_to_resume: resume,
    }
}

fn free() -> LineView {
    LineView {
        job: None,
        last: None,
        backoff_ready: true,
    }
}

fn running_job(turn_n: u128, crossed: bool) -> LineView {
    LineView {
        job: Some(JobView {
            kind: JobKind::Run {
                turn: tid(turn_n),
                entry: Entry::New,
            },
            run: RunTag(3),
            crossed,
            cancel_sent: false,
        }),
        last: None,
        backoff_ready: true,
    }
}

const CRASH: TurnState = TurnState::Interrupted {
    reason: InterruptReason::Crash,
};

#[test]
fn queued_oldest_is_handed_and_later_ones_wait() {
    let none = BTreeMap::new();
    let w = active(vec![open(1, TurnState::Queued), open(2, TurnState::Queued)]);
    assert_eq!(
        decide(&w, None, &round(&none)),
        Some(Action::Hand(JobKind::Run {
            turn: tid(1),
            entry: Entry::New
        }))
    );
    // 线占着（没过边界）：不交第二份活（D3）
    assert_eq!(
        decide(&w, Some(&running_job(1, false)), &round(&none)),
        None
    );
}

/// P1 a：已中断回合后面排着的消息等它。
#[test]
fn queued_behind_interrupted_waits() {
    let none = BTreeMap::new();
    let w = active(vec![open(1, CRASH), open(2, TurnState::Queued)]);
    assert_eq!(decide(&w, Some(&free()), &round(&none)), None);
}

/// D7：两种续接依据各一格；R7：继续意图不大于启动时的 head 不算数；旧意图（不大于中断）不算数。
#[test]
fn resume_sources() {
    let none = BTreeMap::new();
    let mut t = open(1, CRASH);
    t.interrupted_seq = Some(Seq(150));
    t.resume_intent_seq = Some(Seq(160));
    let resume = Some(Action::Hand(JobKind::Run {
        turn: tid(1),
        entry: Entry::Resume,
    }));
    assert_eq!(
        decide(&active(vec![t.clone()]), None, &round(&none)),
        resume
    );
    // 意图在中断之前：上一次中断的意图
    let mut stale = t.clone();
    stale.resume_intent_seq = Some(Seq(140));
    assert_eq!(decide(&active(vec![stale]), None, &round(&none)), None);
    // 意图在启动之前（R7）
    let mut before_boot = t.clone();
    before_boot.interrupted_seq = Some(Seq(50));
    before_boot.resume_intent_seq = Some(Seq(60));
    assert_eq!(
        decide(&active(vec![before_boot.clone()]), None, &round(&none)),
        None
    );
    // 引擎说这次运行里批准已落盘没交付（批准的 seq 比启动时的 head 大）：续接
    let grant = BTreeMap::from([(tid(1), Seq(120))]);
    assert_eq!(
        decide(&active(vec![before_boot.clone()]), None, &round(&grant)),
        resume
    );
    // 上一次运行里的批准（seq 不大于启动时的 head）不让回合自己续接（R6）
    let old_grant = BTreeMap::from([(tid(1), Seq(90))]);
    assert_eq!(
        decide(&active(vec![before_boot]), None, &round(&old_grant)),
        None
    );
}

/// D6：取消意图比起点大、对话在删、宿主完全退出，三种都给过了边界的那一趟发取消；没过边界的不发。
#[test]
fn cancel_only_crossed_runs() {
    let none = BTreeMap::new();
    let mut t = open(1, TurnState::Running);
    t.run_started_seq = Some(Seq(200));
    t.cancel_intent_seq = Some(Seq(210));
    let cancel = Some(Action::Cancel {
        turn: tid(1),
        run: RunTag(3),
    });
    assert_eq!(
        decide(
            &active(vec![t.clone()]),
            Some(&running_job(1, true)),
            &round(&none)
        ),
        cancel
    );
    assert_eq!(
        decide(
            &active(vec![t.clone()]),
            Some(&running_job(1, false)),
            &round(&none)
        ),
        None
    );
    // 旧意图（比这一趟的起点小）不算数（G7）
    let mut old = t.clone();
    old.cancel_intent_seq = Some(Seq(190));
    assert_eq!(
        decide(
            &active(vec![old.clone()]),
            Some(&running_job(1, true)),
            &round(&none)
        ),
        None
    );
    // 发过了不再发
    let mut sent = running_job(1, true);
    sent.job.as_mut().unwrap().cancel_sent = true;
    assert_eq!(decide(&active(vec![t]), Some(&sent), &round(&none)), None);
    // 对话在删 / 完全退出：不看意图
    let w = work(ConversationState::Deleting, vec![old.clone()]);
    assert_eq!(
        decide(&w, Some(&running_job(1, true)), &round(&none)),
        cancel
    );
    let quit = Round {
        closing: Closing::Quit,
        ..round(&none)
    };
    assert_eq!(
        decide(&active(vec![old]), Some(&running_job(1, true)), &quit),
        cancel
    );
}

/// D5：占着进程的回合，本进程里它的那一趟以出错结束 → 补账；从没有过 → 不可能（R3）。
#[test]
fn settle_after_broke_only() {
    let none = BTreeMap::new();
    let w = active(vec![open(1, TurnState::Running)]);
    let broke = LineView {
        last: Some((Some(tid(1)), JobEnd::Broke("写不进".into()))),
        ..free()
    };
    assert_eq!(
        decide(&w, Some(&broke), &round(&none)),
        Some(Action::Settle { turn: tid(1) })
    );
    assert!(matches!(
        decide(&w, Some(&free()), &round(&none)),
        Some(Action::Impossible(_))
    ));
}

/// D5a：上一趟没成，退避到时之前不再交。
#[test]
fn backoff_after_failed_attempt() {
    let none = BTreeMap::new();
    let w = active(vec![open(1, TurnState::Queued)]);
    let waiting = LineView {
        last: Some((Some(tid(1)), JobEnd::Broke("x".into()))),
        backoff_ready: false,
        ..free()
    };
    assert_eq!(decide(&w, Some(&waiting), &round(&none)), None);
    let ready = LineView {
        backoff_ready: true,
        ..waiting
    };
    assert!(matches!(
        decide(&w, Some(&ready), &round(&none)),
        Some(Action::Hand(_))
    ));
}

/// X2：对话在删，排着的、已中断的收成已取消；X3、X4：回合都到终态后收线、再删记录；卡片没完先等。
#[test]
fn deleting_conversation() {
    let none = BTreeMap::new();
    let w = work(
        ConversationState::Deleting,
        vec![open(1, TurnState::Queued)],
    );
    assert_eq!(
        decide(&w, None, &round(&none)),
        Some(Action::Withdraw {
            turn: tid(1),
            from: TurnState::Queued
        })
    );
    let w = work(ConversationState::Deleting, vec![open(1, CRASH)]);
    assert_eq!(
        decide(&w, None, &round(&none)),
        Some(Action::Withdraw {
            turn: tid(1),
            from: CRASH
        })
    );
    let empty = work(ConversationState::Deleting, vec![]);
    assert_eq!(
        decide(&empty, Some(&free()), &round(&none)),
        Some(Action::Hand(JobKind::Close))
    );
    assert_eq!(
        decide(&empty, None, &round(&none)),
        Some(Action::FinishDelete)
    );
    let mut cards = work(ConversationState::Deleting, vec![]);
    cards.cards_open = true;
    assert_eq!(decide(&cards, None, &round(&none)), None);
    // 对照：没在删的对话没有活
    assert_eq!(decide(&active(vec![]), Some(&free()), &round(&none)), None);
}

/// 归属按回合判：同一个对话里排着两个回合、助手不同，处理的是哪个就读哪个的归属。
#[test]
fn owner_is_judged_per_turn_pure() {
    let none = BTreeMap::new();
    let mut gone = open(2, TurnState::Queued);
    gone.owner.assistant = Presence::Gone;
    // 前一个的助手在：照常派发，不因后一个的助手没了而收掉
    let w = active(vec![open(1, TurnState::Queued), gone.clone()]);
    assert!(matches!(
        decide(&w, None, &round(&none)),
        Some(Action::Hand(_))
    ));
    // 反过来：前一个的助手没了 → 收成已取消
    let w = active(vec![gone, open(1, TurnState::Queued)]);
    assert_eq!(
        decide(&w, None, &round(&none)),
        Some(Action::Withdraw {
            turn: tid(2),
            from: TurnState::Queued
        })
    );
}

/// 封口之后什么都不做；完全退出时不再交活，排着的收成已取消，已中断的不动。
#[test]
fn closing_states() {
    let none = BTreeMap::new();
    let sealed = Round {
        closing: Closing::Sealed,
        ..round(&none)
    };
    let quit = Round {
        closing: Closing::Quit,
        ..round(&none)
    };
    let w = active(vec![open(1, TurnState::Queued)]);
    assert_eq!(decide(&w, None, &sealed), None);
    assert!(matches!(
        decide(&w, None, &quit),
        Some(Action::Withdraw { .. })
    ));
    let mut t = open(1, CRASH);
    t.interrupted_seq = Some(Seq(150));
    t.resume_intent_seq = Some(Seq(160));
    assert_eq!(decide(&active(vec![t]), None, &quit), None);
}

/// 叫醒宿主的登记表（0a.6）里每一项都是真实的事件种类：写错一个字就等于漏了这一种（后果是晚到保底的 30 秒）。
#[test]
fn wake_kinds_are_real_event_kinds() {
    for k in crate::host::TURN_HOST_WAKE_KINDS {
        assert!(
            iwork_domain::event::DomainEvent::KINDS.contains(k),
            "{k} 不是事件种类"
        );
    }
}
