//! 回合状态的两类写者撞车，由存储在提交时裁决（T10a 方案 A4、G1；7.2 的 A4 一行，7.2a 的 K3）。
//!
//! 编排（`run_turn`）和「没有编排在跑时替回合提交的一方」（受理函数的撤回、放弃；宿主的补账）同时出手。
//! 随机挑场景、先后与让出次数，跑完后对库里的事件断言：
//! - 每个回合每条状态变化的 `from` 等于前一条的 `to`（第一条的 `from` 是 `Queued`）；
//! - 终态之后没有状态变化；
//! - 运行时至多收到一次 prompt。
//!
//! 另有固定的一格 `a4_fixed_withdraw_vs_dispatch`：撤回先落盘、编排的第一笔后到。去掉提交时校验，这一格
//! 必红，不靠随机撞上。
//!
//! 提交时校验在假存储的参照投影里（T10a 步 1 加）。步 1 合入之前参照投影还不校验回合状态，这几条标 `ignore`：
//! 那时它们必红，`--ignored` 跑一遍就是 K3 的红灯对照。

use super::*;

use iwork_core::turn::{OwedBy, owed_turn_events};
use iwork_domain::event::{MessageAppended, TurnFinished, TurnStateChanged};
use iwork_domain::message::Message;
use iwork_domain::turn::TurnTrigger;
use iwork_ports::store::{CommitRequest, StorageError, StoragePort};
use proptest::prelude::*;

/// 受理函数或宿主替回合提交时，被拒了重读重算最多这么多次（A4 的「最多 3 次」）。
const DIRECT_ATTEMPTS: usize = 3;

/// 一条状态变化的简写。
fn changed(turn: TurnId, from: TurnState, to: TurnState) -> DomainEvent {
    TurnStateChanged {
        turn_id: turn,
        from,
        to,
        reason: None,
    }
    .into()
}

fn finished(turn: TurnId, state: TurnState) -> DomainEvent {
    TurnFinished {
        turn_id: turn,
        state,
        usage: Default::default(),
        celebrate: false,
        change_summary: None,
    }
    .into()
}

async fn commit(store: &MemStore, events: Vec<DomainEvent>) -> Result<(), StorageError> {
    store
        .commit(CommitRequest {
            command_id: iwork_domain::id::CommandId::from_raw(RawUlid::from_u128(
                0xA4_0000 + store.commits.load(Ordering::SeqCst) as u128,
            )),
            actor: iwork_domain::common::Actor("test".into()),
            events,
            outbox: Vec::new(),
        })
        .await
        .map(|_| ())
}

/// 受理函数「撤回」（`Queued` → `Cancelled`）或「放弃」（`Interrupted` → `Cancelled`）。读到的状态不对就不提交；
/// 被拒了重读再定。返回最后有没有落盘。
async fn withdraw(store: &MemStore, turn: TurnId) -> bool {
    for _ in 0..DIRECT_ATTEMPTS {
        let Some(state) = state_now(store, turn) else {
            return false;
        };
        let trigger = match state {
            TurnState::Queued => TurnTrigger::Cancel,
            TurnState::Interrupted { .. } => TurnTrigger::Abandon,
            _ => return false,
        };
        let to = iwork_domain::turn::next(state, trigger).unwrap();
        match commit(store, vec![changed(turn, state, to), finished(turn, to)]).await {
            Ok(()) => return true,
            Err(StorageError::Projection { .. }) => continue,
            Err(e) => panic!("{e}"),
        }
    }
    false
}

/// 宿主补账（D5）：照库里此刻的状态算欠的事件，被拒了重读重算。
async fn settle_broken(store: &MemStore, turn: &Turn) {
    for _ in 0..DIRECT_ATTEMPTS {
        let mut now = turn.clone();
        now.state = state_now(store, turn.id).unwrap();
        let streaming = streaming_of(store, turn.id);
        let by = OwedBy::BrokenRun {
            cancel_requested: false,
            reason: "编排出错".into(),
        };
        let events = owed_turn_events(&now, &streaming, &by).unwrap();
        if events.is_empty() {
            return;
        }
        match commit(store, events).await {
            Ok(()) => return,
            Err(StorageError::Projection { .. }) => continue,
            Err(e) => panic!("{e}"),
        }
    }
    panic!("补账 {DIRECT_ATTEMPTS} 次都被拒");
}

/// 回合在库里此刻的状态：最后一条状态变化的终点；没有就是布景时的 `Queued`。
fn state_now(store: &MemStore, turn: TurnId) -> Option<TurnState> {
    Some(state_of(store, turn).unwrap_or(TurnState::Queued))
}

/// 回合名下还在流式的助手消息。
fn streaming_of(store: &MemStore, turn: TurnId) -> Vec<Message> {
    let all = store.all();
    all.iter()
        .filter_map(|e| {
            match e {
            DomainEvent::MessageAppended(MessageAppended { message })
                if message.turn_id == Some(turn)
                    && !all.iter().any(|c| {
                        matches!(c, DomainEvent::MessageCompleted(m) if m.message_id == message.id)
                    }) =>
            {
                Some(message.clone())
            }
            _ => None,
        }
        })
        .collect()
}

/// 三条不变量。从回合排队时的 `Queued` 起，走一遍库里它的全部状态变化（含布景写的）。
fn check_invariants(w: &W<Reference>, turn: TurnId) -> Result<(), String> {
    let mut at = TurnState::Queued;
    for e in w.store.all() {
        if let DomainEvent::TurnStateChanged(c) = e
            && c.turn_id == turn
        {
            if at.is_terminal() {
                return Err(format!("终态 {at:?} 之后还有 {:?} -> {:?}", c.from, c.to));
            }
            if c.from != at {
                return Err(format!("from {:?} 不等于前一条的 to {at:?}", c.from));
            }
            at = c.to;
        }
    }
    let prompts = w.ctl.prompts.load(Ordering::SeqCst);
    if prompts > 1 {
        return Err(format!("prompt 发了 {prompts} 次"));
    }
    Ok(())
}

/// 场景。
#[derive(Debug, Clone, Copy)]
enum Scenario {
    /// 撤回排着的回合 × 派发它
    WithdrawVsDispatch,
    /// 放弃已中断的回合 × 续接它
    AbandonVsResume,
    /// 同一个已中断的回合被两条线同时续接（宿主出错时会有的样子；只该有一趟进 Running）
    TwoResumes,
    /// 编排的一笔已入队、编排随后被丢掉，宿主照读到的状态补账
    SettleVsLateCommit,
}

/// 直接提交的一方在编排的哪一步之前出手。
#[derive(Debug, Clone, Copy)]
enum Order {
    /// 编排还没开始
    DirectFirst,
    /// 编排的第一笔停在入队门口时
    AtFirstCommit,
    /// 编排的第一笔已落盘
    RunnerFirst,
}

/// 编排跑完：已 prompt 就让运行时做完。编排出错（第一笔被拒）也是一种结局，不在这里判。
async fn drive_to_end(w: &Arc<W<Reference>>, mut task: Task<Reference>) {
    tokio::select! {
        _ = &mut task => {}
        () = wait_until("prompt 或编排结束", || w.ctl.prompts.load(Ordering::SeqCst) > 0) => {
            w.ctl.send(AgentUpdate::Finished(TurnOutcome::Completed));
            let _ = task.await;
        }
    }
}

async fn yields(n: u8) {
    for _ in 0..n {
        tokio::task::yield_now().await;
    }
}

/// 新回合用 `W::plan`，续接用 `W::resume_plan`。
type PlanOf = fn(&W<Reference>, Turn) -> TurnPlan;

async fn run_scenario(scenario: Scenario, order: Order, spin: u8) -> Result<(), String> {
    let turn = queued_turn(1);
    let t = turn.id;
    let w = W::<Reference>::new(&turn).await;
    let (turn, plan_of): (Turn, PlanOf) = match scenario {
        Scenario::WithdrawVsDispatch | Scenario::SettleVsLateCommit => (turn, W::<Reference>::plan),
        Scenario::AbandonVsResume | Scenario::TwoResumes => {
            (interrupted_turn(&w, turn), W::<Reference>::resume_plan)
        }
    };

    match scenario {
        Scenario::WithdrawVsDispatch | Scenario::AbandonVsResume => {
            let gate = Arc::new(Notify::new());
            let task = match order {
                Order::DirectFirst => {
                    withdraw(&w.store, t).await;
                    yields(spin).await;
                    w.spawn(w.line(), plan_of(&w, turn))
                }
                Order::AtFirstCommit => {
                    *w.store.enqueue_gate.lock().unwrap() = Some(gate.clone());
                    let task = w.spawn(w.line(), plan_of(&w, turn));
                    wait_until("第一笔停在门口", || {
                        w.store.at_enqueue_gate.load(Ordering::SeqCst)
                    })
                    .await;
                    withdraw(&w.store, t).await;
                    yields(spin).await;
                    gate.notify_one();
                    task
                }
                Order::RunnerFirst => {
                    let line = w.line();
                    let canceller = line.canceller();
                    let task = w.spawn(line, plan_of(&w, turn));
                    wait_until("第一笔落盘", || w.state(t) == Some(TurnState::Running)).await;
                    yields(spin).await;
                    // 被拒了：回合在跑，照 A5 改成取消意图，宿主随后发取消信号
                    if !withdraw(&w.store, t).await {
                        canceller.cancel(t, RunTag(1));
                    }
                    task
                }
            };
            drive_to_end(&w, task).await;
        }
        Scenario::TwoResumes => {
            let gate = Arc::new(Notify::new());
            *w.store.enqueue_gate.lock().unwrap() = Some(gate.clone());
            let a = w.spawn(w.line(), plan_of(&w, turn.clone()));
            wait_until("甲停在门口", || {
                w.store.at_enqueue_gate.load(Ordering::SeqCst)
            })
            .await;
            // 甲先放（甲的第一笔先入队），或乙先跑（乙的第一笔先落盘、甲在门口等）
            if let Order::DirectFirst = order {
                gate.notify_one();
            }
            let b = w.spawn(w.line(), plan_of(&w, turn));
            yields(spin).await;
            if !matches!(order, Order::DirectFirst) {
                gate.notify_one();
            }
            drive_to_end(&w, a).await;
            drive_to_end(&w, b).await;
        }
        Scenario::SettleVsLateCommit => {
            let task = w.running(turn.clone()).await;
            let gate = Arc::new(Notify::new());
            *w.store.before_commit.lock().unwrap() = Some(gate.clone());
            // 编排的「等待审批」那一笔入队后停在处理之前
            let _ask = w.ctl.ask("A", "npm test");
            wait_until("编排的一笔已入队", || {
                w.store.commit_entered.load(Ordering::SeqCst)
            })
            .await;
            task.abort();
            let _ = task.await;
            yields(spin).await;
            let host = {
                let w = w.clone();
                let turn = turn.clone();
                tokio::spawn(async move { settle_broken(&w.store, &turn).await })
            };
            if let Order::DirectFirst = order {
                yields(spin).await;
            }
            gate.notify_one();
            host.await.unwrap();
        }
    }
    check_invariants(&w, t)
}

fn rt() -> tokio::runtime::Runtime {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap()
}

fn scenario() -> impl Strategy<Value = Scenario> {
    prop_oneof![
        Just(Scenario::WithdrawVsDispatch),
        Just(Scenario::AbandonVsResume),
        Just(Scenario::TwoResumes),
        Just(Scenario::SettleVsLateCommit),
    ]
}

fn order() -> impl Strategy<Value = Order> {
    prop_oneof![
        Just(Order::DirectFirst),
        Just(Order::AtFirstCommit),
        Just(Order::RunnerFirst),
    ]
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(
        std::env::var("PROPTEST_CASES").ok().and_then(|n| n.parse().ok()).unwrap_or(64)
    ))]
    #[test]
    #[ignore = "等 T10a 步 1：假存储的参照投影还没有回合状态的提交时校验（A4）"]
    fn a4_interleavings(scenario in scenario(), order in order(), spin in 0u8..8) {
        let result = rt().block_on(run_scenario(scenario, order, spin));
        prop_assert!(result.is_ok(), "{:?}", result);
    }
}

/// 7.2a 的 K3：撤回先落盘、编排的第一笔（`Queued` → `Running`）后到。没有提交时校验时两笔都落盘，第二条的
/// `from` 不等于前一条的 `to`。
#[tokio::test(flavor = "current_thread")]
#[ignore = "等 T10a 步 1：假存储的参照投影还没有回合状态的提交时校验（A4）"]
async fn a4_fixed_withdraw_vs_dispatch() {
    let result = run_scenario(Scenario::WithdrawVsDispatch, Order::DirectFirst, 0).await;
    assert_eq!(result, Ok(()));
}
