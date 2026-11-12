//! 交接表的屏障测试（T06 方案 15.3 的五条 core 用例）。
//!
//! 处理方的 future 用手动轮询（`Waker::noop`）推进：每一步停在哪由测试决定，不靠调度。抢锁的两条用真线程。
//! 产品代码里没有测试停靠点：「发送方在临界区里」由测试线程自己拿着 `TurnSlot` 的锁来扮演。

use std::collections::BTreeSet;
use std::future::{Future, poll_fn};
use std::pin::Pin;
use std::sync::{Arc, Barrier, Condvar, Mutex};
use std::task::{Context, Poll, Waker};

use iwork_domain::approval::VoidReason;
use iwork_domain::event::DomainEvent;
use iwork_domain::id::{ApprovalId, RawUlid, TurnId};
use iwork_ports::agent::{PermissionDecision, PermissionHandler, PermissionRequest, RawToolInput};
use iwork_ports::clock::MonotonicInstant;

use super::*;

/// 判定方：工具输入在「已答」集合里才给出放行，否则一直等。
#[derive(Default)]
struct Scripted {
    ready: Mutex<BTreeSet<String>>,
}

impl Scripted {
    fn answer(&self, input: &str) {
        self.ready.lock().unwrap().insert(input.to_owned());
    }
}

impl PermissionDecider for Scripted {
    type VoidHold = ();

    async fn decide(&self, _turn_id: TurnId, req: &PermissionRequest) -> Decided {
        poll_fn(|_| {
            if self.ready.lock().unwrap().contains(&req.raw_input.0) {
                Poll::Ready(Decided {
                    decision: PermissionDecision::Allow,
                    approval: Some(ApprovalId::from_raw(RawUlid::from_u128(9))),
                })
            } else {
                Poll::Pending
            }
        })
        .await
    }

    async fn void_pending(
        &self,
        _turn_id: TurnId,
        _reason: VoidReason,
    ) -> Result<(Vec<DomainEvent>, ()), VoidPlanError> {
        Ok((Vec::new(), ()))
    }
}

fn turn() -> TurnId {
    TurnId::from_raw(RawUlid::from_u128(1))
}

fn req(input: &str) -> PermissionRequest {
    PermissionRequest {
        tool: iwork_domain::action::ToolRef("Bash".into()),
        raw_input: RawToolInput(input.into()),
        bypass_sandbox: false,
        updates_before: 0,
        cwd: None,
    }
}

fn at() -> AcceptedAt {
    AcceptedAt {
        mono: MonotonicInstant(0),
        wall: iwork_domain::common::Timestamp(0),
    }
}

fn gone() -> PermissionDecision {
    turn_gone()
}

struct Bench {
    slot: Arc<TurnSlot>,
    decider: Arc<Scripted>,
    gate: Arc<PermissionGate<Scripted>>,
}

impl Bench {
    fn new() -> Self {
        let slot = Arc::new(TurnSlot::default());
        let _cancels = slot.open(turn());
        let decider = Arc::new(Scripted::default());
        let gate = Arc::new(PermissionGate {
            slot: slot.clone(),
            decider: decider.clone(),
        });
        Self {
            slot,
            decider,
            gate,
        }
    }

    fn ask(&self, input: &str) -> Pending {
        let gate = self.gate.clone();
        let req = req(input);
        Pending(Box::pin(async move { gate.on_permission(req).await }))
    }

    fn table_is_empty(&self) -> bool {
        self.slot.inspect(Handoff::is_empty) == Some(true)
    }
}

/// 一个手动推进的权限请求。
struct Pending(Pin<Box<dyn Future<Output = PermissionDecision> + Send>>);

impl Pending {
    fn poll(&mut self) -> Poll<PermissionDecision> {
        self.0
            .as_mut()
            .poll(&mut Context::from_waker(Waker::noop()))
    }

    fn waiting(&mut self) {
        assert!(self.poll().is_pending(), "这一步应当在等");
    }

    fn done(&mut self) -> PermissionDecision {
        match self.poll() {
            Poll::Ready(d) => d,
            Poll::Pending => panic!("应当已经有结论，不需要别人再做一步"),
        }
    }
}

/// 停靠点表（方案 2.6 第 3 条）里的每一行：一次 `stop_accepting()` 之后，停在等受理、等判定方、等回执的
/// 请求都返回拒绝，不需要编排再转一圈；停止之后才登记的、停止之后判定方才返回（走到 `submit`）的，在锁里
/// 当场被拒；停止之前已拿到回执 `true` 的保留原结论；表是空的。
#[test]
fn stop_accepting_releases_every_waiter() {
    let b = Bench::new();
    // 第 2 行：等受理
    let mut asked = b.ask("asked");
    asked.waiting();
    // 第 3 行：等判定方
    let mut deciding = b.ask("deciding");
    deciding.waiting();
    b.slot.admit(2, at());
    deciding.waiting();
    // 第 5 行：交了结论、等回执
    let mut acking = b.ask("acking");
    acking.waiting();
    b.slot.admit(3, at());
    b.decider.answer("acking");
    acking.waiting();
    assert_eq!(
        b.slot.inspect(|h| h.decided(3)),
        Some(Some(Some(ApprovalId::from_raw(RawUlid::from_u128(9)))))
    );
    // 第 6 行：停止之前已被采纳
    let mut adopted = b.ask("adopted");
    adopted.waiting();
    b.slot.admit(4, at());
    b.decider.answer("adopted");
    adopted.waiting();
    assert!(b.slot.adopt(4).is_some());
    // 第五个：停止时在等判定方，停止之后判定方才返回
    let mut late = b.ask("late");
    late.waiting();
    b.slot.admit(5, at());
    late.waiting();

    b.slot.stop_accepting();
    assert!(b.table_is_empty());

    assert_eq!(asked.done(), gone());
    assert_eq!(deciding.done(), gone());
    assert_eq!(acking.done(), gone());
    // 判定方的结论与撤销同时就绪时结论优先，走到 submit，在锁里被拒
    b.decider.answer("late");
    assert_eq!(late.done(), gone());
    // 第四个：停止之后才来
    assert_eq!(b.ask("after").done(), gone());
    assert_eq!(
        adopted.done(),
        PermissionDecision::Allow,
        "对照：已采纳的保留结论"
    );
    assert!(b.table_is_empty());
}

/// 撤销与采纳只有一个赢家，赢家由编排定。先采纳：之后的停止对它没有作用；先停止：处理方返回拒绝，
/// 之后才交的结论在 `submit` 里当场被拒。
#[test]
fn ticket_revoke_and_adopt_have_one_winner() {
    // 先采纳
    let b = Bench::new();
    let mut p = b.ask("x");
    p.waiting();
    b.slot.admit(1, at());
    b.decider.answer("x");
    p.waiting();
    assert!(b.slot.adopt(1).is_some());
    b.slot.stop_accepting();
    assert_eq!(p.done(), PermissionDecision::Allow);

    // 先停止
    let b = Bench::new();
    let mut p = b.ask("x");
    p.waiting();
    b.slot.admit(1, at());
    p.waiting();
    b.slot.stop_accepting();
    assert!(b.slot.adopt(1).is_none(), "停止之后没有可采纳的");
    assert_eq!(p.done(), gone());
    assert!(matches!(b.slot.submit(turn(), 1, None), Err(Stopped)));
}

/// 假会话的 `cancel()`：照 `iwork-agent-acp` 的约定，等名下的权限任务都把答复写出去，再记
/// `session/cancel`。不设上限：编排不守约定时这里卡住，由 `join_within` 报出来（没有「等满 GRACE_PERIOD
/// 也能过」这回事）。
#[derive(Default)]
struct Wire {
    log: Mutex<Vec<String>>,
    in_flight: Mutex<usize>,
    idle: Condvar,
}

impl Wire {
    fn task_started(&self) {
        *self.in_flight.lock().unwrap() += 1;
    }

    fn answered(&self, who: &str, decision: &PermissionDecision) {
        let word = if *decision == PermissionDecision::Allow {
            "allow"
        } else {
            "deny"
        };
        self.log.lock().unwrap().push(format!("{who} {word}"));
        *self.in_flight.lock().unwrap() -= 1;
        self.idle.notify_all();
    }

    fn cancel(&self) {
        let guard = self.in_flight.lock().unwrap();
        let _idle = self.idle.wait_while(guard, |n| *n > 0).unwrap();
        self.log.lock().unwrap().push("session/cancel".into());
    }

    fn log(&self) -> Vec<String> {
        self.log.lock().unwrap().clone()
    }
}

/// 等线程结束，最多 5 秒。
fn join_within(handle: std::thread::JoinHandle<()>, what: &str) {
    let start = std::time::Instant::now();
    while !handle.is_finished() {
        assert!(
            start.elapsed() < std::time::Duration::from_secs(5),
            "5 秒内没结束：{what}"
        );
        std::thread::sleep(std::time::Duration::from_millis(1));
    }
    handle.join().expect("线程不 panic");
}

/// 线程 T：停止受理，再调假会话的 `cancel()`。
fn stopper(b: &Bench, wire: &Arc<Wire>) -> std::thread::JoinHandle<()> {
    let (slot, wire) = (b.slot.clone(), wire.clone());
    std::thread::spawn(move || {
        slot.stop_accepting();
        wire.cancel();
    })
}

/// 屏障 a（5/F-1）：发送方人在临界区里时停止。线程 S 自己拿着锁调 `Handoff::register`（或 `submit`），
/// 这时起线程 T 调 `stop_accepting()` 与 `cancel()`。T 在等锁还是晚点才来不去分辨：两种情形下停止都只能
/// 整个排在登记之后。S 放锁后跑完剩下那段（产品里同一段代码），它的拒绝排在 `session/cancel` 之前。
#[test]
fn stop_while_sender_is_inside_the_lock() {
    // register
    let b = Bench::new();
    let wire = Arc::new(Wire::default());
    wire.task_started();
    {
        let mut guard = b.slot.lock();
        let t = stopper(&b, &wire);
        let (registered, _waker) = guard
            .handoff
            .as_mut()
            .expect("回合在")
            .register(0)
            .expect("锁在 S 手里，停止插不到登记前面");
        drop(guard);
        let decision = pollster::block_on(b.gate.after_register(registered, &req("s")));
        wire.answered("S", &decision);
        assert_eq!(decision, gone());
        join_within(t, "停止与取消");
    }
    assert_eq!(wire.log(), ["S deny", "session/cancel"]);
    assert!(b.table_is_empty());

    // submit：票先由测试扮编排受理
    let b = Bench::new();
    let wire = Arc::new(Wire::default());
    let registered = b.slot.register(0).expect("登记");
    b.slot.admit(registered.ticket, at());
    wire.task_started();
    let mut guard = b.slot.lock();
    let t = stopper(&b, &wire);
    let (acked, _waker) = guard
        .handoff
        .as_mut()
        .expect("回合在")
        .submit(turn(), registered.ticket, None)
        .expect("锁在 S 手里，停止插不到交结论前面");
    drop(guard);
    let decision = pollster::block_on(after_submit(PermissionDecision::Allow, acked));
    wire.answered("S", &decision);
    assert_eq!(decision, gone());
    join_within(t, "停止与取消");
    assert_eq!(wire.log(), ["S deny", "session/cancel"]);
    assert!(b.table_is_empty());
}

/// 屏障 b（5/F-1）：发送方刚登记完（`asked` 还没人答）、刚交完结论（`acked` 还没人答）时停止。
/// 线程 S 把处理方轮询一次停在那里，线程 T 停止受理、取消；S 接着跑完，拒绝排在取消之前。
#[test]
fn stop_right_after_sender_registered() {
    for submitted in [false, true] {
        let b = Bench::new();
        let wire = Arc::new(Wire::default());
        let posed = Arc::new(Barrier::new(2));
        if submitted {
            b.decider.answer("s");
        }
        let s = {
            let (b_slot, gate, wire, posed) =
                (b.slot.clone(), b.gate.clone(), wire.clone(), posed.clone());
            wire.task_started();
            std::thread::spawn(move || {
                let mut p = Pending(Box::pin(async move { gate.on_permission(req("s")).await }));
                p.waiting();
                if submitted {
                    // 扮编排受理；判定方已答，处理方一轮走到交结论、等回执
                    b_slot.admit(1, at());
                    p.waiting();
                    assert!(b_slot.inspect(|h| h.decided(1).is_some()) == Some(true));
                }
                posed.wait();
                // 停在这里的请求只等它自己等的那个接收端：T 停止受理时那里就有了回绝
                let decision = pollster::block_on(p.0);
                wire.answered("S", &decision);
                assert_eq!(decision, gone());
            })
        };
        posed.wait();
        let t = stopper(&b, &wire);
        join_within(t, "停止与取消");
        join_within(s, "发送方");
        assert_eq!(
            wire.log(),
            ["S deny", "session/cancel"],
            "submitted={submitted}"
        );
        assert!(b.table_is_empty());
    }
}

/// 抢锁循环：`register` / `submit` 与 `stop_accepting()` 两条真线程同时放行，不设停靠点。每一轮两边返回后，
/// 请求要么当场被拒，要么它等的接收端里已经有了回绝；表是空的。抓的是「以后有人把检查挪到锁外面」。
#[test]
fn stop_then_register_never_waits() {
    for round in 0..2000 {
        let submit = round % 2 == 1;
        let b = Bench::new();
        let ticket = if submit {
            let r = b.slot.register(0).expect("登记");
            b.slot.admit(r.ticket, at());
            Some(r.ticket)
        } else {
            None
        };
        let go = Arc::new(Barrier::new(2));
        let t = {
            let (slot, go) = (b.slot.clone(), go.clone());
            std::thread::spawn(move || {
                go.wait();
                slot.stop_accepting();
            })
        };
        go.wait();
        match ticket {
            None => {
                if let Ok(mut r) = b.slot.register(0) {
                    t.join().unwrap();
                    assert_eq!(
                        r.asked.try_recv().map(|a| a.map(|x| x.is_some())),
                        Ok(Some(false)),
                        "第 {round} 轮：登记先拿到锁，就被停止取走、回了不受理"
                    );
                } else {
                    t.join().unwrap();
                }
            }
            Some(ticket) => {
                if let Ok(mut acked) = b.slot.submit(turn(), ticket, None) {
                    t.join().unwrap();
                    assert_eq!(
                        acked.try_recv(),
                        Ok(Some(false)),
                        "第 {round} 轮：交结论先拿到锁，就被停止取走、回执 false"
                    );
                } else {
                    t.join().unwrap();
                }
            }
        }
        assert!(b.table_is_empty(), "第 {round} 轮");
    }
}
