//! 回合用量的口径（审查 2026-11-13-m1-t10a-d7 1/F-3，supervisor 定 b）：`UsageReported` 和 `TurnFinished` 带的
//! 都是「这个回合到现在一共」。续接的那一趟从上次记下的累计接着加；适配层每次报的是本趟的用量，同一趟里后报的
//! 替换先报的，起点不动。

use iwork_domain::common::Usage;

use super::*;

fn tokens(input: u64) -> Usage {
    Usage {
        input_tokens: input,
        output_tokens: input * 2,
        ..Usage::default()
    }
}

/// 这个回合写下的用量：(`UsageReported` 的, `TurnFinished` 的)。
fn usage_events(store: &MemStore, t: TurnId) -> (Vec<Usage>, Vec<Usage>) {
    let all = store.all();
    let reported = all
        .iter()
        .filter_map(|e| match e {
            DomainEvent::UsageReported(u) if u.turn_id == t => Some(u.usage),
            _ => None,
        })
        .collect();
    let finished = all
        .iter()
        .filter_map(|e| match e {
            DomainEvent::TurnFinished(f) if f.turn_id == t => Some(f.usage),
            _ => None,
        })
        .collect();
    (reported, finished)
}

/// 续接的回合：上次记下 10，这一趟先报 5、再报 20（同一趟里替换），结束时两条事件都带 30。
#[tokio::test(flavor = "current_thread")]
async fn resumed_turn_reports_usage_on_top_of_recorded() {
    let turn = queued_turn(1);
    let t = turn.id;
    let w = W::<Reference>::new(&turn).await;
    let turn = interrupted_turn(&w, turn);
    let plan = TurnPlan {
        entry: TurnEntry::Resume {
            request: ResumeRequest::Ready(request_blocks()),
            recorded_usage: tokens(10),
        },
        ..w.resume_plan(turn)
    };
    let task = w.spawn(
        SessionLine::new(w.decider.clone(), Some(OLD_ID.into())),
        plan,
    );
    wait_until("prompt", || w.ctl.prompts.load(Ordering::SeqCst) == 1).await;
    w.ctl.send(AgentUpdate::Usage(tokens(5)));
    w.ctl.send(AgentUpdate::Usage(tokens(20)));
    w.ctl.send(AgentUpdate::Finished(TurnOutcome::Completed));
    assert_eq!(task.await.unwrap().0.unwrap(), TurnState::Completed);
    assert_eq!(
        usage_events(&w.store, t),
        (vec![tokens(30)], vec![tokens(30)])
    );
}

/// 新回合从 0 起：报 7，两条事件都带 7（对照：起点只来自续接入口）。
#[tokio::test(flavor = "current_thread")]
async fn new_turn_reports_its_own_usage() {
    let turn = queued_turn(1);
    let t = turn.id;
    let w = W::<Reference>::new(&turn).await;
    let task = w.spawn(SessionLine::new(w.decider.clone(), None), w.plan(turn));
    wait_until("prompt", || w.ctl.prompts.load(Ordering::SeqCst) == 1).await;
    w.ctl.send(AgentUpdate::Usage(tokens(7)));
    w.ctl.send(AgentUpdate::Finished(TurnOutcome::Completed));
    assert_eq!(task.await.unwrap().0.unwrap(), TurnState::Completed);
    assert_eq!(
        usage_events(&w.store, t),
        (vec![tokens(7)], vec![tokens(7)])
    );
}

/// 续接的那一趟什么都没报：结束事件仍带上次记下的累计，不写多余的 `UsageReported`。
#[tokio::test(flavor = "current_thread")]
async fn resumed_turn_without_new_usage_keeps_recorded() {
    let turn = queued_turn(1);
    let t = turn.id;
    let w = W::<Reference>::new(&turn).await;
    let turn = interrupted_turn(&w, turn);
    let plan = TurnPlan {
        entry: TurnEntry::Resume {
            request: ResumeRequest::Ready(request_blocks()),
            recorded_usage: tokens(10),
        },
        ..w.resume_plan(turn)
    };
    let task = w.spawn(
        SessionLine::new(w.decider.clone(), Some(OLD_ID.into())),
        plan,
    );
    wait_until("prompt", || w.ctl.prompts.load(Ordering::SeqCst) == 1).await;
    w.ctl.send(AgentUpdate::Finished(TurnOutcome::Completed));
    assert_eq!(task.await.unwrap().0.unwrap(), TurnState::Completed);
    assert_eq!(usage_events(&w.store, t), (vec![], vec![tokens(10)]));
}
