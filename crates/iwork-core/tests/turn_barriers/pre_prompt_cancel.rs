//! 交出 prompt 之前认取消（T10a 方案 D1a；7.2 第 ① 条，7.2a 的 K8、K9）。
//!
//! 五个等待点各一格（`pre_prompt_cancel_at_each_wait`，表驱动），外加裁决点两种先后一格
//! （`cancel_just_before_and_after_pass_prompt`）。每格用闸门把编排停在那个等待点，这时取消，再放开。
//! 「收到 prompt」按会话实例分开数：调用了 prompt、却因进程已退出没收下的，不算收到。

use super::*;

/// 编排停在哪儿时到的取消。
#[derive(Debug, Clone, Copy)]
enum Wait {
    /// 交接表还没建（宿主刚过派发边界）
    BeforeSlot,
    /// 开会话里
    OpeningSession,
    /// 续接失败、读摘要里
    ReadingSummary,
    /// 第一笔提交（会话绑定、进 Running）里
    FirstCommit,
    /// 沿用的进程已退出、重开会话里（第一笔已提交）
    Reopening,
}

const CANCELLED_BEFORE_START: [&str; 2] = ["Queued -> Cancelled", "finished Cancelled"];
const CANCELLED_AFTER_START: [&str; 4] = [
    "Queued -> Running",
    "Running -> Cancelling",
    "Cancelling -> Cancelled",
    "finished Cancelled",
];

fn received(w: &W<Reference>) -> Vec<usize> {
    w.ctl.received.lock().unwrap().clone()
}

fn prompt_calls(w: &W<Reference>) -> Vec<usize> {
    w.ctl.prompt_calls.lock().unwrap().clone()
}

/// 等这一趟结束；期间运行时一收下 prompt 就判红（不然没人答这个 prompt，回合会一直等下去，坏实现表现成超时）。
async fn join_without_prompt(
    w: &W<Reference>,
    mut task: Task<Reference>,
    what: &str,
) -> (Result<TurnState, TurnRunError>, Line<Reference>) {
    let before = received(w).len();
    tokio::select! {
        joined = &mut task => joined.unwrap(),
        () = wait_until("运行时收下 prompt", || received(w).len() > before) => {
            task.abort();
            panic!("{what}：取消之后运行时还是收下了 prompt");
        }
    }
}

#[tokio::test(flavor = "current_thread")]
async fn pre_prompt_cancel_at_each_wait() {
    for wait in [
        Wait::BeforeSlot,
        Wait::OpeningSession,
        Wait::ReadingSummary,
        Wait::FirstCommit,
        Wait::Reopening,
    ] {
        cancel_at(wait).await;
    }
}

async fn cancel_at(wait: Wait) {
    let first = queued_turn(1);
    let w = W::<Reference>::new(&first).await;
    // 「重开」那格要一条已经跑过一个回合、会话开着的线；别的格从新线开始
    let (line, turn) = match wait {
        Wait::Reopening => {
            let task = w.running(first).await;
            w.ctl.send(AgentUpdate::Finished(TurnOutcome::Completed));
            let (result, line) = task.await.unwrap();
            assert_eq!(result.unwrap(), TurnState::Completed);
            (line, queued_turn_seeded(&w, 2))
        }
        Wait::ReadingSummary => (
            SessionLine::new(w.decider.clone(), Some(OLD_ID.into())),
            first,
        ),
        _ => (w.line(), first),
    };
    let t = turn.id;
    let received_before = received(&w);
    let canceller = line.canceller();

    let gate = Arc::new(Notify::new());
    match wait {
        Wait::BeforeSlot => {}
        Wait::OpeningSession => *w.ctl.open_gate.lock().unwrap() = Some(gate.clone()),
        Wait::ReadingSummary => {
            w.history.set(history());
            w.ctl
                .open_errors
                .lock()
                .unwrap()
                .push_back(lost(SessionLostReason::NotFound));
            w.ctl.fresh_ids.lock().unwrap().push_back(NEW_ID.into());
            *w.history.gate.lock().unwrap() = Some(gate.clone());
        }
        Wait::FirstCommit => *w.store.before_commit.lock().unwrap() = Some(gate.clone()),
        Wait::Reopening => {
            w.ctl
                .prompt_errors
                .lock()
                .unwrap()
                .push_back(AgentError::ProcessExited {
                    message: "退出了".into(),
                });
            *w.ctl.open_gate.lock().unwrap() = Some(gate.clone());
        }
    }

    let task = if let Wait::BeforeSlot = wait {
        // 交接表还没建：取消先记在 TurnSlot 上
        canceller.cancel(t, RunTag(1));
        w.spawn(line, w.plan(turn))
    } else {
        let task = w.spawn(line, w.plan(turn));
        let what = format!("停在 {wait:?}");
        wait_until(&what, || match wait {
            Wait::OpeningSession | Wait::Reopening => w.ctl.opening.load(Ordering::SeqCst),
            Wait::ReadingSummary => w.history.reading.load(Ordering::SeqCst),
            Wait::FirstCommit => w.store.commit_entered.load(Ordering::SeqCst),
            Wait::BeforeSlot => unreachable!(),
        })
        .await;
        canceller.cancel(t, RunTag(1));
        gate.notify_one();
        task
    };
    let (result, line) = join_without_prompt(&w, task, &format!("{wait:?}")).await;

    assert_eq!(result.unwrap(), TurnState::Cancelled, "{wait:?}");
    assert_eq!(
        received(&w),
        received_before,
        "{wait:?}：没有哪个会话实例收下这一趟的 prompt"
    );
    let expected: &[&str] = match wait {
        Wait::BeforeSlot | Wait::OpeningSession | Wait::ReadingSummary => &CANCELLED_BEFORE_START,
        Wait::FirstCommit | Wait::Reopening => &CANCELLED_AFTER_START,
    };
    assert_eq!(trail(&w.store, t), strings(expected), "{wait:?}");
    match wait {
        Wait::BeforeSlot => {
            assert_eq!(w.ctl.opens.load(Ordering::SeqCst), 0, "不开会话");
            assert!(prompt_calls(&w).is_empty());
        }
        Wait::OpeningSession => {
            assert!(prompt_calls(&w).is_empty());
            assert!(line.session().is_some(), "开好的会话留给下一回合");
        }
        Wait::ReadingSummary => {
            assert!(prompt_calls(&w).is_empty());
            assert_eq!(
                line.resume_id(),
                Some(OLD_ID),
                "没收下 prompt 的新会话不算数"
            );
            assert!(line.session().is_none());
            assert!(line.is_settled());
        }
        Wait::FirstCommit => {
            assert!(prompt_calls(&w).is_empty());
            assert_eq!(message_endings(&w.store), [MessageStatus::Interrupted]);
        }
        Wait::Reopening => {
            // 旧实例 1 被调用过两次：第一回合收下，这一回合那次因进程已退出没收下；新实例 2 一次都没被调用
            assert_eq!(prompt_calls(&w), [1, 1], "{wait:?}");
            assert_eq!(received(&w), [1], "旧实例这一回合 0 条、新实例 0 条");
            assert_eq!(w.ctl.opens.load(Ordering::SeqCst), 2);
        }
    }
}

/// 续接的回合在第一笔提交之前看到取消：什么都不提交，原样停在 Interrupted。
#[tokio::test(flavor = "current_thread")]
async fn cancelled_resume_before_start_commits_nothing() {
    let turn = queued_turn(1);
    let w = W::<Reference>::new(&turn).await;
    let turn = interrupted_turn(&w, turn);
    let t = turn.id;
    let before = w.store.all().len();
    let line = w.line();
    line.canceller().cancel(t, RunTag(1));
    let task = w.spawn(line, w.resume_plan(turn));
    let (result, _) = join_without_prompt(&w, task, "续接").await;
    assert_eq!(
        result.unwrap(),
        TurnState::Interrupted {
            reason: InterruptReason::Crash
        }
    );
    assert_eq!(w.store.all().len(), before, "没有提交任何东西");
    assert_eq!(w.ctl.opens.load(Ordering::SeqCst), 0);
}

/// 别的回合的取消、上一回合留下没用上的取消，都不落到这个回合头上。
#[tokio::test(flavor = "current_thread")]
async fn early_cancel_for_another_turn_is_dropped() {
    let turn = queued_turn(1);
    let w = W::<Reference>::new(&turn).await;
    let line = w.line();
    line.canceller().cancel(queued_turn(9).id, RunTag(1));
    let task = w.spawn(line, w.plan(turn.clone()));
    wait_until("prompt", || !received(&w).is_empty()).await;
    w.ctl.send(AgentUpdate::Finished(TurnOutcome::Completed));
    assert_eq!(task.await.unwrap().0.unwrap(), TurnState::Completed);
}

/// 裁决点两种先后：取消在裁决之前（第一笔已落盘、回执还没到）→ 不交出；在裁决之后（prompt 已在路上）→
/// 照在跑的回合取消，运行时收到 prompt 和 `session/cancel`。
#[tokio::test(flavor = "current_thread")]
async fn cancel_just_before_and_after_pass_prompt() {
    // 之前：第一笔处理完、回执停在闸门上
    let turn = queued_turn(1);
    let t = turn.id;
    let w = W::<Reference>::new(&turn).await;
    let gate = Arc::new(Notify::new());
    *w.store.receipt_gate.lock().unwrap() = Some(gate.clone());
    let line = w.line();
    let canceller = line.canceller();
    let task = w.spawn(line, w.plan(turn));
    wait_until("第一笔已落盘", || {
        w.state(t) == Some(TurnState::Running)
    })
    .await;
    canceller.cancel(t, RunTag(1));
    gate.notify_one();
    let (result, _) = join_without_prompt(&w, task, "取消在裁决之前").await;
    assert_eq!(result.unwrap(), TurnState::Cancelled);
    assert!(prompt_calls(&w).is_empty(), "取消先到：没有 prompt");
    assert_eq!(trail(&w.store, t), strings(&CANCELLED_AFTER_START));

    // 之后：裁决已过、prompt 停在运行时收下之前
    let turn = queued_turn(1);
    let t = turn.id;
    let w = W::<Reference>::new(&turn).await;
    let gate = Arc::new(Notify::new());
    *w.ctl.prompt_gate.lock().unwrap() = Some(gate.clone());
    let line = w.line();
    let canceller = line.canceller();
    let task = w.spawn(line, w.plan(turn));
    wait_until("prompt 在路上", || {
        w.ctl.prompting.load(Ordering::SeqCst)
    })
    .await;
    canceller.cancel(t, RunTag(1));
    gate.notify_one();
    let (result, _) = task.await.unwrap();
    assert_eq!(result.unwrap(), TurnState::Cancelled);
    assert_eq!(received(&w), [1], "裁决先过：prompt 已交出");
    assert_eq!(w.ctl.wire(), ["session/cancel"]);
    assert_eq!(trail(&w.store, t), strings(&CANCELLED_AFTER_START));
}

// ---------- 早到的取消认趟（T10a 方案 2.4「早到的取消认趟」；7.2a 的 K16） ----------

/// 已中断的回合第 `run` 趟续接的 plan。
fn resume_run(w: &W<Reference>, turn: &Turn, run: u64) -> TurnPlan {
    TurnPlan {
        run: RunTag(run),
        ..w.resume_plan(turn.clone())
    }
}

/// 第 1 趟在交出 prompt 之前被取消、原样停在 Interrupted，交接表已撤。
async fn first_run_cancelled(w: &Arc<W<Reference>>, turn: &Turn) -> Line<Reference> {
    let line = w.line();
    line.canceller().cancel(turn.id, RunTag(1));
    let task = w.spawn(line, resume_run(w, turn, 1));
    let (result, line) = join_without_prompt(w, task, "第 1 趟").await;
    assert_eq!(
        result.unwrap(),
        TurnState::Interrupted {
            reason: InterruptReason::Crash
        }
    );
    line
}

/// 第 1 趟结束、表已撤之后才到一条给（回合，第 1 趟）的取消；同一个回合第 2 趟续接照常交出 prompt。
#[tokio::test(flavor = "current_thread")]
async fn late_cancel_for_previous_run_does_not_hit_next_run() {
    let turn = queued_turn(1);
    let w = W::<Reference>::new(&turn).await;
    let turn = interrupted_turn(&w, turn);
    let line = first_run_cancelled(&w, &turn).await;
    // 宿主还没记下第 1 趟结束，这时发的取消晚到了
    line.canceller().cancel(turn.id, RunTag(1));

    let task = w.spawn(line, resume_run(&w, &turn, 2));
    wait_until("第 2 趟交出 prompt", || !received(&w).is_empty()).await;
    w.ctl.send(AgentUpdate::Finished(TurnOutcome::Completed));
    assert_eq!(task.await.unwrap().0.unwrap(), TurnState::Completed);
    assert_eq!(received(&w), [1], "第 2 趟收下了 prompt");
}

/// 正向：给（回合，第 2 趟）的取消在第 2 趟开表之前到 → 第 2 趟不交出 prompt。
#[tokio::test(flavor = "current_thread")]
async fn early_cancel_for_next_run_is_kept() {
    let turn = queued_turn(1);
    let w = W::<Reference>::new(&turn).await;
    let turn = interrupted_turn(&w, turn);
    let line = first_run_cancelled(&w, &turn).await;
    line.canceller().cancel(turn.id, RunTag(2));

    let task = w.spawn(line, resume_run(&w, &turn, 2));
    let (result, _) = join_without_prompt(&w, task, "第 2 趟").await;
    assert_eq!(
        result.unwrap(),
        TurnState::Interrupted {
            reason: InterruptReason::Crash
        }
    );
    assert_eq!(w.ctl.opens.load(Ordering::SeqCst), 0, "两趟都没开会话");
}

/// 正向：第 2 趟的表已经开着（停在开会话里）时来一条给第 1 趟的取消 → 不起作用，照常交出 prompt。
#[tokio::test(flavor = "current_thread")]
async fn cancel_for_previous_run_misses_open_table() {
    let turn = queued_turn(1);
    let w = W::<Reference>::new(&turn).await;
    let turn = interrupted_turn(&w, turn);
    let line = first_run_cancelled(&w, &turn).await;
    let canceller = line.canceller();

    let gate = Arc::new(Notify::new());
    *w.ctl.open_gate.lock().unwrap() = Some(gate.clone());
    let task = w.spawn(line, resume_run(&w, &turn, 2));
    wait_until("第 2 趟停在开会话里", || {
        w.ctl.opening.load(Ordering::SeqCst)
    })
    .await;
    canceller.cancel(turn.id, RunTag(1));
    gate.notify_one();
    wait_until("第 2 趟交出 prompt", || !received(&w).is_empty()).await;
    w.ctl.send(AgentUpdate::Finished(TurnOutcome::Completed));
    assert_eq!(task.await.unwrap().0.unwrap(), TurnState::Completed);
}
