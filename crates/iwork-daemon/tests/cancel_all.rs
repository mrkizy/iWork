//! A5（`CancelAll`，1/F-4）：一条命令管到哪些回合，在它那一笔提交里定死；要么都取消，要么一个都没取消。
//!
//! 三种布置（只有在跑的、只有排着的、两种都有）× 四种打断（受理函数在提交前被中止、提交在途时被中止、提交返回后
//! 被中止、提交回 `Busy`）。判据都是同步的：这条命令的回执在不在库里，和每个目标回合到没到 `Cancelled` 一一对应；
//! 排着的回合被取消了就从没被派发（库里没有它进 `Running` 的那一条）。那一笔落盘之后才受理的新消息照常跑完。

mod support;

use std::sync::Arc;
use std::sync::atomic::Ordering;

use iwork_daemon::gate::GatedStore;
use iwork_daemon::handler::dispatch::{CommandPorts, DispatchHooks, Dispatcher};
use iwork_daemon::time::{MonotonicIds, SystemClock};
use iwork_domain::event::DomainEvent;
use iwork_domain::id::{CommandId, TurnId};
use iwork_domain::turn::TurnState;
use iwork_ipc::server::RequestHandler;
use iwork_ports::store::StoragePort;
use iwork_proto::pb;
use support::fixtures::read_model;
use support::turns::*;
use tokio::sync::mpsc;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Layout {
    Running,
    Queued,
    Both,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Interrupt {
    BeforeCommit,
    InFlight,
    AfterReturn,
    Busy,
}

impl Interrupt {
    /// 这一种打断下那一笔落没落盘（期望值按方案 A1、A5 手写）
    fn lands(self) -> bool {
        matches!(self, Interrupt::InFlight | Interrupt::AfterReturn)
    }
}

/// 带屏障的分发入口：`BeforeCommit` 时受理函数算好之后停在提交之前，报到 `reached`。
fn dispatcher(h: &H, stop_before_commit: bool) -> (Arc<Disp>, mpsc::UnboundedReceiver<()>) {
    let (tx, rx) = mpsc::unbounded_channel();
    let ports = CommandPorts {
        store: Arc::new(GatedStore::new(h.raw.clone(), h.gate.clone())),
        read: Arc::new(read_model(&h.raw)),
        engine: h.engine.clone(),
        ids: Arc::new(MonotonicIds::new(SystemClock::new())),
        clock: Arc::new(SystemClock::new()),
        host: Arc::new(h.host.clone()),
    };
    let hooks = DispatchHooks {
        before_commit: stop_before_commit.then(|| {
            Box::new(move |_| {
                let tx = tx.clone();
                Box::pin(async move {
                    let _ = tx.send(());
                    std::future::pending::<()>().await;
                }) as iwork_daemon::handler::dispatch::HookFuture
            }) as Box<dyn Fn(CommandId) -> _ + Send + Sync>
        }),
        ..Default::default()
    };
    (Arc::new(Dispatcher::new(ports).with_hooks(hooks)), rx)
}

fn cancel_all() -> pb::command::Kind {
    pb::command::Kind::CancelAll(pb::CancelAll {
        conversation_id: conversation().to_string(),
    })
}

fn ran(evs: &[(u64, DomainEvent)], t: TurnId) -> bool {
    evs.iter().any(|(_, e)| {
        matches!(e, DomainEvent::TurnStateChanged(s) if s.turn_id == t && s.to == TurnState::Running)
    })
}

async fn one_cell(layout: Layout, interrupt: Interrupt) {
    let cell = format!("{layout:?} × {interrupt:?}");
    let mut h = harness(Opts::default()).await;
    h.agent.hold.store(true, Ordering::SeqCst);
    let mut running = None;
    if layout != Layout::Queued {
        h.start_host();
        let t = h.send(1, "跑着").await;
        h.agent.wait_prompts(1).await;
        running = Some(t);
    }
    let mut queued = Vec::new();
    if layout != Layout::Running {
        // 只有排着的：宿主还没起，它们留在排队
        queued.push(h.send(2, "排着一").await);
        queued.push(h.send(3, "排着二").await);
    }
    let id = fresh_id(100);
    let (d, mut reached) = dispatcher(&h, interrupt == Interrupt::BeforeCommit);
    if interrupt == Interrupt::Busy {
        h.gate.refuse_when(move |r| r.command_id == id);
    }
    let (enq_tx, mut enqueued) = mpsc::unbounded_channel();
    if interrupt == Interrupt::InFlight {
        h.gate.on_enqueued(move |c| {
            if c == id {
                let _ = enq_tx.send(());
            }
        });
    }
    let task = {
        let d = d.clone();
        tokio::spawn(async move {
            d.handle(
                &ctx(),
                pb::Request {
                    request_id: 1,
                    body: Some(pb::request::Body::Command(pb::Command {
                        command_id: id.to_string(),
                        kind: Some(cancel_all()),
                    })),
                },
            )
            .await
        })
    };
    match interrupt {
        Interrupt::BeforeCommit => {
            within("停在提交之前", reached.recv()).await;
            task.abort();
        }
        Interrupt::InFlight => {
            within("那一笔入了队", enqueued.recv()).await;
            task.abort();
        }
        Interrupt::AfterReturn | Interrupt::Busy => {
            let _ = within("受理回来", task).await;
        }
    }
    // 在途的那一笔被中止之后照样会落盘：等写队列把它处理完（排在它后面的查回执一定看得到结果）
    let landed = within("查回执", h.raw.command_receipt(id))
        .await
        .unwrap()
        .is_some();
    assert_eq!(landed, interrupt.lands(), "{cell}：那一笔落没落盘");
    if layout == Layout::Queued {
        h.start_host();
    }
    let targets: Vec<TurnId> = running
        .iter()
        .copied()
        .chain(queued.iter().copied())
        .collect();
    if landed {
        for t in &targets {
            h.wait_state(*t, |s| s == TurnState::Cancelled).await;
        }
        let evs = events(&h.raw).await;
        for q in &queued {
            assert!(!ran(&evs, *q), "{cell}：排着的被取消了就没被派发");
        }
    } else {
        if let Some(t) = running {
            assert_eq!(h.turn_state(t).await, TurnState::Running, "{cell}");
        }
        let evs = events(&h.raw).await;
        assert!(
            !evs.iter().any(|(_, e)| matches!(
                e,
                DomainEvent::TurnStateChanged(s) if s.to == TurnState::Cancelled
            )),
            "{cell}：那一笔没落，一个都没取消"
        );
        assert!(
            !evs.iter()
                .any(|(_, e)| matches!(e, DomainEvent::TurnIntentRecorded(_))),
            "{cell}：没有取消意图"
        );
        // 收掉布置里的回合，下面的新消息才排得上
        for _ in &targets {
            h.agent.finish_one();
        }
        for t in &targets {
            h.wait_state(*t, |s| s == TurnState::Completed).await;
        }
    }
    // 之后受理的新消息照常跑完
    h.agent.hold.store(false, Ordering::SeqCst);
    let later = h.send(9, "之后").await;
    h.wait_state(later, |s| s == TurnState::Completed).await;
    h.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn cancel_all_grid() {
    for layout in [Layout::Running, Layout::Queued, Layout::Both] {
        for interrupt in [
            Interrupt::BeforeCommit,
            Interrupt::InFlight,
            Interrupt::AfterReturn,
            Interrupt::Busy,
        ] {
            one_cell(layout, interrupt).await;
        }
    }
}
